# syntax=docker/dockerfile:1
# ---------------------------------------------------------------------------
# DRGTW — multi-stage Docker build
#
# Base OS: ubuntu:24.04 (glibc 2.39) for all stages.
#   ort 2.x with features = ["download-binaries"] downloads a pre-built ONNX
#   Runtime that was compiled against glibc 2.38+ (__isoc23_strtoll etc.).
#   debian:bookworm ships glibc 2.36 and fails at link time; ubuntu:24.04
#   ships glibc 2.39 and works.
#
# Stages:
#   builder  — rustup + cargo build --release -p drgtw (dep-cache layer)
#   libs     — extracts libonnxruntime*.so* (gracefully empty if static)
#   runtime  — ubuntu:24.04, non-root user, binary + libs
#
# NOTE: bump RUST_VERSION when Cargo.lock requires a newer rustc.
# ---------------------------------------------------------------------------

ARG RUST_VERSION=1.93.0

# ---- builder ---------------------------------------------------------------
FROM ubuntu:24.04 AS builder

ARG RUST_VERSION

# System packages:
#   curl, ca-certificates  — rustup installer + ort downloads ONNX Runtime over HTTPS
#   build-essential        — linker (cc/ld), not present by default
#   pkg-config, libssl-dev — pulled in by some crates
#   cmake, clang           — required by tokenizers (onig C binding)
RUN apt-get update && apt-get install -y --no-install-recommends \
        curl \
        ca-certificates \
        build-essential \
        pkg-config \
        libssl-dev \
        cmake \
        clang \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain "${RUST_VERSION}" --profile minimal \
    && rustup show

WORKDIR /build

# ---------------------------------------------------------------------------
# Dependency-caching layer
# Copy manifests + lock file, stub every workspace member, build deps only.
# Third-party compilation (including ort's ONNX Runtime download) is cached
# in this layer; rebuilds triggered by source changes skip it entirely.
# ---------------------------------------------------------------------------
COPY Cargo.toml Cargo.lock ./

COPY bins/drgtw/Cargo.toml           bins/drgtw/Cargo.toml
COPY bins/drgtw-bench/Cargo.toml     bins/drgtw-bench/Cargo.toml
COPY crates/drgtw-config/Cargo.toml  crates/drgtw-config/Cargo.toml
COPY crates/drgtw-keys/Cargo.toml    crates/drgtw-keys/Cargo.toml
COPY crates/drgtw-ner/Cargo.toml     crates/drgtw-ner/Cargo.toml
COPY crates/drgtw-pii/Cargo.toml     crates/drgtw-pii/Cargo.toml
COPY crates/drgtw-guardrails/Cargo.toml crates/drgtw-guardrails/Cargo.toml
COPY crates/drgtw-proxy/Cargo.toml   crates/drgtw-proxy/Cargo.toml
COPY crates/drgtw-events/Cargo.toml  crates/drgtw-events/Cargo.toml
COPY crates/drgtw-vault/Cargo.toml   crates/drgtw-vault/Cargo.toml
COPY crates/drgtw-mcp/Cargo.toml     crates/drgtw-mcp/Cargo.toml
COPY crates/drgtw-trace/Cargo.toml   crates/drgtw-trace/Cargo.toml
COPY crates/drgtw-otel/Cargo.toml    crates/drgtw-otel/Cargo.toml
COPY crates/drgtw-ui/Cargo.toml      crates/drgtw-ui/Cargo.toml
COPY crates/drgtw-history/Cargo.toml crates/drgtw-history/Cargo.toml
COPY crates/drgtw-ui-auth/Cargo.toml crates/drgtw-ui-auth/Cargo.toml

RUN set -e; \
    for member in \
        bins/drgtw \
        bins/drgtw-bench \
        crates/drgtw-config \
        crates/drgtw-keys \
        crates/drgtw-ner \
        crates/drgtw-pii \
        crates/drgtw-guardrails \
        crates/drgtw-proxy \
        crates/drgtw-events \
        crates/drgtw-vault \
        crates/drgtw-mcp \
        crates/drgtw-trace \
        crates/drgtw-otel \
        crates/drgtw-ui \
        crates/drgtw-history \
        crates/drgtw-ui-auth \
    ; do \
        mkdir -p "$member/src"; \
        printf 'pub fn _stub(){}' > "$member/src/lib.rs"; \
        printf 'fn main(){}' > "$member/src/main.rs"; \
    done

# Build deps (ort downloads libonnxruntime here — network access required).
RUN cargo build --release -p drgtw

# Remove workspace-local stub artifacts so the real build recompiles them.
# Third-party dep artifacts remain intact — that is the caching point.
RUN find target/release/deps \( -name 'drgtw*' -o -name 'libdrgtw*' \) \
        -exec rm -f {} + 2>/dev/null; \
    find target/release -maxdepth 1 \
        \( -name 'drgtw*' -o -name 'libdrgtw*' \) ! -name '*.d' \
        -exec rm -f {} +

# ---------------------------------------------------------------------------
# Real source build
#
# Builds with the DEFAULT feature set (`ui` is on by default — see
# bins/drgtw/Cargo.toml), which propagates `postgres` into drgtw-history,
# drgtw-ui, and drgtw-proxy. That compiles in the boot-time Postgres connect
# and the fire-and-forget usage recording the admin UI relies on. Do NOT add
# `--no-default-features` here: that drops sqlx and ships a UI-less binary
# that renders the "built without Postgres support" setup page.
# sqlx uses tls-rustls (pure Rust) — no libpq / Postgres client lib needed in
# either the builder or the runtime stage.
# ---------------------------------------------------------------------------
COPY bins/    bins/
COPY crates/  crates/

RUN cargo build --release -p drgtw

# Report where ort put the shared library (informational).
RUN printf '\n=== ONNX Runtime shared libs ===\n' && \
    find target/release -maxdepth 1 -name 'libonnxruntime*' \
    && printf '================================\n' \
    || printf '(none found — may be statically linked)\n'

# ---- libs ------------------------------------------------------------------
# Intermediate stage that collects libonnxruntime*.so* into /app/lib.
# This stage always succeeds: if no .so files exist the directory is empty.
FROM ubuntu:24.04 AS libs

RUN mkdir -p /app/lib

COPY --from=builder /build/target/release/ /tmp/release/

RUN find /tmp/release -maxdepth 1 -name 'libonnxruntime*.so*' \
        -exec install -m755 {} /app/lib/ \; \
    && rm -rf /tmp/release \
    && echo "Contents of /app/lib:" && ls /app/lib/

# ---- runtime ---------------------------------------------------------------
FROM ubuntu:24.04 AS runtime

# ca-certificates: required for outbound TLS to LLM providers.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root service user.
RUN useradd -r -u 1001 -m -s /usr/sbin/nologin drgtw

WORKDIR /app

# Gateway binary.
COPY --from=builder /build/target/release/drgtw /usr/local/bin/drgtw

# ONNX Runtime shared library (empty dir if ort linked statically — harmless).
COPY --from=libs /app/lib/ /app/lib/

# Add /app/lib to the dynamic linker search path.
ENV LD_LIBRARY_PATH=/app/lib

# Fix ownership.
RUN chown -R drgtw:drgtw /app && chmod 755 /usr/local/bin/drgtw

USER drgtw

# Volumes:
#   /app/drgtw.toml  — configuration file  (bind-mount from host)
#   /app/models      — optional NER model directory
#   /app/data        — optional SQLite vault storage
VOLUME ["/app/models", "/app/data"]

EXPOSE 8080

# OCI image labels.
LABEL org.opencontainers.image.title="drgtw" \
      org.opencontainers.image.description="Privacy-first LLM gateway — pseudonymizes PII before forwarding to OpenAI/Anthropic providers" \
      org.opencontainers.image.source="https://github.com/ramden/drgtw" \
      org.opencontainers.image.licenses="Apache-2.0"

ENTRYPOINT ["drgtw"]
CMD ["--config", "/app/drgtw.toml"]
