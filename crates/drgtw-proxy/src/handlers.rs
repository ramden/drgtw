//! Route handlers: POST /v1/chat/completions, POST /v1/messages, GET /v1/models.
//!
//! WP 3.4: PII pipeline wired in.
//! WP 8.3: budgets, multi-connection fallback, and usage events wired in.
//!
//! ## PII mode resolution (both endpoints)
//! Header `x-drgtw-pii: on|off` (case-insensitive) overrides the config
//! default.  Any other value → 400.  No header → config default.
//!
//! ## Request path when PII on
//! 1. Parse body once into `serde_json::Value` (serves both routing and PII).
//! 2. Pseudonymize in place; serialize modified value as the upstream body.
//!    `EntityMap` is frozen after this point and wrapped in `Arc` for sharing
//!    with the response path.
//! 3. When PII is off: forward original bytes byte-for-byte (no re-serialise).
//!
//! ## Budget (WP 8.3)
//! After the rate-limit check, `budget.check(key_id)` gates the request.
//! `Exhausted` → 429 endpoint-native error (`insufficient_budget` /
//! `rate_limit_error`) with a `retry-after` header. Spend is recorded once the
//! request's cost is known (after the response usage is parsed).
//!
//! ## Fallback (WP 8.3)
//! When `config.fallback.enabled`, the handler iterates the ordered candidate
//! connections from `connections_for_model`, filtered to those whose format
//! matches the endpoint. A connect/transport error or a 502/503/504/429 status
//! is *retriable*: if more candidates remain, the next is tried (counting an
//! attempt). A non-retriable status or a success terminates the loop. For
//! streaming, failover happens before the response body is consumed (reqwest
//! yields the status before the body), so a partially streamed response is
//! never failed over mid-stream. When fallback is disabled, only the first
//! candidate is tried.
//!
//! ## Usage events (WP 8.3)
//! When an [`EventSink`] is configured, every completed proxied request (both
//! success and upstream-error paths, after auth succeeds) emits one
//! [`UsageEvent`]. Auth failures emit nothing. Non-streaming responses parse
//! usage from the buffered body; streaming responses capture usage via the
//! [`usage_tap`] wrapper and emit at stream end.
//!
//! ## Compression
//! The reqwest client is built without compression feature flags, so upstream
//! responses arrive as plain bytes — no decompression layer is needed.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use drgtw_config::{ApiFormat, Connection};
use drgtw_events::{
    cost_for, extract_usage_anthropic, extract_usage_openai, ModelCost, UsageEvent,
};
use drgtw_keys::{BudgetDecision, RateDecision};
use drgtw_pii::{
    body::{restore_body_with_store, try_pseudonymize_body, BodyFormat},
    EntityMap, StreamRestorer,
};
use drgtw_trace::{LlmMeta, TraceEntry, TraceKind};
use std::collections::{BTreeMap, HashMap};
use tracing::{info_span, Instrument as _};

use crate::converse::{converse_event_to_sse, converse_to_openai, openai_to_converse, StreamXlateState};
use crate::error::{ErrorFormat, ProxyError};
use crate::eventstream::EventStreamDecoder;
use crate::otel_enrich;
use crate::sigv4::{sign_bedrock_request, SigV4Creds};
use crate::sse_restore::SseRestorer;
use crate::upstream::{
    bedrock_invoke_url, chat_completions_url, converse_stream_url, converse_url, embeddings_url,
    messages_url,
};
use crate::usage_tap::{usage_tap_stream, StreamUsage};
use crate::ProxyState;

// ---------------------------------------------------------------------------
// PII mode resolution
// ---------------------------------------------------------------------------

/// Resolved per-request PII mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PiiMode {
    On,
    Off,
}

/// Resolve the debug flag from the `x-drgtw-debug` request header.
///
/// `on` (case-insensitive) enables the debug block. Any other value — or an
/// absent header — leaves it off. This is a best-effort demo affordance, so an
/// unrecognized value is silently ignored rather than rejected.
fn resolve_debug(headers: &HeaderMap) -> bool {
    headers
        .get("x-drgtw-debug")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("on"))
        .unwrap_or(false)
}

/// Resolve PII mode from the `x-drgtw-pii` request header and the config
/// default.
fn resolve_pii_mode(
    headers: &HeaderMap,
    enabled_by_default: bool,
    error_fmt: ErrorFormat,
) -> Result<PiiMode, ProxyError> {
    match headers.get("x-drgtw-pii") {
        None => Ok(if enabled_by_default { PiiMode::On } else { PiiMode::Off }),
        Some(v) => match v.to_str().unwrap_or("").to_ascii_lowercase().as_str() {
            "on" => Ok(PiiMode::On),
            "off" => Ok(PiiMode::Off),
            _ => Err(ProxyError::InvalidPiiHeader { fmt: error_fmt }),
        },
    }
}

/// Resolve the request id: honour the `x-drgtw-request-id` header set by the
/// bin middleware; otherwise generate a short fallback id (handlers may be
/// exercised without the middleware, e.g. proxy-crate integration tests).
fn resolve_request_id(headers: &HeaderMap) -> String {
    if let Some(id) = headers.get("x-drgtw-request-id").and_then(|v| v.to_str().ok())
        && !id.is_empty()
    {
        return id.to_owned();
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("drgtw-{nanos:016x}")
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Attribution metadata capture (Feature 2)
// ---------------------------------------------------------------------------

/// Header name prefix that flags a request header as attribution metadata.
const META_HEADER_PREFIX: &str = "x-drgtw-meta-";
/// Maximum number of metadata keys retained on an event.
const META_MAX_KEYS: usize = 16;
/// Maximum metadata key length (chars). Keys longer than this are dropped.
const META_MAX_KEY_LEN: usize = 64;
/// Maximum metadata value length (chars). Longer values are truncated.
const META_MAX_VALUE_LEN: usize = 256;

/// Truncate a string to at most `max` characters (char boundaries respected).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

/// Collect caller-supplied attribution metadata for the usage event.
///
/// Sources, merged with **headers winning on key collision**:
///   1. The request body's top-level `metadata` object — string values are
///      taken verbatim, non-string values are JSON-stringified. This function
///      only reads `parsed`; the caller strips the `metadata` object before
///      forwarding (Azure-style upstreams reject unknown params with 400).
///   2. Request headers prefixed `x-drgtw-meta-` — the prefix is stripped and
///      the remainder lowercased to form the key.
///
/// Caps (documented on [`UsageEvent::metadata`]): keys longer than
/// [`META_MAX_KEY_LEN`] are dropped; values are truncated to
/// [`META_MAX_VALUE_LEN`]; at most [`META_MAX_KEYS`] keys are kept, excess keys
/// dropped deterministically in sorted (BTreeMap) order. Returns `None` when no
/// metadata survives, so the event field stays absent for backward compat.
///
/// The `x-drgtw-meta-*` headers are never forwarded upstream: the upstream
/// request is built from an explicit allowlist of headers, not the inbound set,
/// so these never leak to the provider.
fn collect_metadata(headers: &HeaderMap, parsed: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();

    // 1. Body `metadata` object first (headers override below).
    if let Some(obj) = parsed.get("metadata").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if k.is_empty() || k.chars().count() > META_MAX_KEY_LEN {
                continue;
            }
            let value = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => continue,
                other => other.to_string(),
            };
            map.insert(k.clone(), truncate_chars(&value, META_MAX_VALUE_LEN));
        }
    }

    // 2. `x-drgtw-meta-*` headers override on key collision.
    for (name, value) in headers {
        let name = name.as_str();
        let Some(suffix) = name.strip_prefix(META_HEADER_PREFIX) else {
            continue;
        };
        if suffix.is_empty() || suffix.chars().count() > META_MAX_KEY_LEN {
            continue;
        }
        let Ok(val) = value.to_str() else { continue };
        map.insert(suffix.to_ascii_lowercase(), truncate_chars(val, META_MAX_VALUE_LEN));
    }

    if map.is_empty() {
        return None;
    }

    // Cap key count deterministically: BTreeMap iterates in sorted order, so
    // keeping the first META_MAX_KEYS drops the lexicographically-largest keys.
    if map.len() > META_MAX_KEYS {
        let drop: Vec<String> = map.keys().skip(META_MAX_KEYS).cloned().collect();
        for k in drop {
            map.remove(&k);
        }
    }

    Some(map)
}

// ---------------------------------------------------------------------------
// Per-endpoint static configuration
// ---------------------------------------------------------------------------

/// Endpoint-specific knobs shared between the two handlers.
struct EndpointSpec {
    /// Wire format this endpoint requires of a connection.
    format: ApiFormat,
    /// PII body format for pseudonymise/restore.
    body_format: BodyFormat,
    /// Error wire format for this endpoint.
    error_fmt: ErrorFormat,
    /// `endpoint` field for usage events.
    name: &'static str,
}

const OPENAI_SPEC: EndpointSpec = EndpointSpec {
    format: ApiFormat::OpenAi,
    body_format: BodyFormat::OpenAiChat,
    error_fmt: ErrorFormat::OpenAi,
    name: "chat_completions",
};

const ANTHROPIC_SPEC: EndpointSpec = EndpointSpec {
    format: ApiFormat::Anthropic,
    body_format: BodyFormat::AnthropicMessages,
    error_fmt: ErrorFormat::Anthropic,
    name: "messages",
};

/// Does `spec` accept a candidate connection that speaks `conn_fmt`?
///
/// The `/v1/messages` endpoint (`ANTHROPIC_SPEC`) accepts BOTH `anthropic` and
/// `bedrock` connections — a native-Bedrock connection serves the Anthropic
/// Messages surface via InvokeModel. Every other endpoint requires an exact
/// format match.
fn spec_accepts_format(spec: &EndpointSpec, conn_fmt: ApiFormat) -> bool {
    match spec.format {
        ApiFormat::Anthropic => {
            matches!(conn_fmt, ApiFormat::Anthropic | ApiFormat::Bedrock)
        }
        // The `/v1/chat/completions` endpoint (`OPENAI_SPEC`) accepts BOTH
        // `open_ai` and `bedrock_converse` connections — a bedrock_converse
        // connection serves the OpenAI surface via the Converse API.
        ApiFormat::OpenAi => {
            matches!(conn_fmt, ApiFormat::OpenAi | ApiFormat::BedrockConverse)
        }
        other => conn_fmt == other,
    }
}

// ---------------------------------------------------------------------------
// POST /v1/chat/completions  (OpenAI format)
// ---------------------------------------------------------------------------

pub async fn chat_completions(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    match proxy_endpoint(state, req, &OPENAI_SPEC).await {
        Ok(resp) => resp,
        Err(e) => e.into_response_fmt(ErrorFormat::OpenAi),
    }
}

// ---------------------------------------------------------------------------
// POST /v1/messages  (Anthropic Messages API)
// ---------------------------------------------------------------------------

pub async fn messages(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    match proxy_endpoint(state, req, &ANTHROPIC_SPEC).await {
        Ok(resp) => resp,
        Err(e) => e.into_response_fmt(ErrorFormat::Anthropic),
    }
}

// ---------------------------------------------------------------------------
// Shared proxy core (WP 3.4 + 8.3)
// ---------------------------------------------------------------------------

async fn proxy_endpoint(
    state: Arc<ProxyState>,
    req: Request,
    spec: &'static EndpointSpec,
) -> Result<Response, ProxyError> {
    // Load a snapshot of the hot-swappable live bundle once per request.
    // All per-config fields (keys, limiter, budget, pii, mcp, cost_tables,
    // config) are accessed via this guard for the lifetime of the request.
    let live = state.live.load();
    let started = Instant::now();
    let (parts, req_body) = req.into_parts();

    // 1. Authenticate from inbound headers. (Auth failures emit no event.)
    let resolved = live.keys.authenticate(&parts.headers).map_err(ProxyError::from)?;

    let request_id = resolve_request_id(&parts.headers);

    // 2. Rate-limit check (WP 2.1).
    let rate_decision = live.limiter.check(&resolved.key_id);
    if let RateDecision::Limited { retry_after_secs, limit } = rate_decision {
        emit_trace_reject(
            &state,
            spec.name,
            &request_id,
            &resolved.key_id,
            started,
            "rate limited",
        );
        return Err(ProxyError::RateLimited { retry_after_secs, limit });
    }

    // 3. Budget check (WP 8.3): does NOT consume; spend recorded post-response.
    if let BudgetDecision::Exhausted { max_usd, retry_after_secs } =
        live.budget.check(&resolved.key_id)
    {
        emit_trace_reject(
            &state,
            spec.name,
            &request_id,
            &resolved.key_id,
            started,
            "budget exhausted",
        );
        return Err(ProxyError::BudgetExhausted { max_usd, retry_after_secs });
    }

    // 4. Resolve PII mode (WP 3.4).
    let pii_mode = resolve_pii_mode(
        &parts.headers,
        live.config.pii.enabled_by_default,
        spec.error_fmt,
    )?;

    // Demo debug header: only meaningful for non-streaming PII-on requests; the
    // relay decides whether to actually emit the block.
    let debug = resolve_debug(&parts.headers);

    // 5. Buffer body bytes (WP 6.3: enforce max_body_bytes limit).
    let max = live.config.server.max_body_bytes;
    let raw_body: Bytes = match to_bytes(req_body, max).await {
        Ok(b) => b,
        Err(_) => return Err(ProxyError::BodyTooLarge),
    };

    // 6. Parse body once for routing + optional PII rewrite.
    let mut parsed: serde_json::Value =
        serde_json::from_slice(&raw_body).unwrap_or(serde_json::Value::Null);

    // Capture attribution metadata from the ORIGINAL body + inbound headers
    // before any rewrite.
    let metadata = collect_metadata(&parts.headers, &parsed);

    // Drop the harvested body `metadata` object before forwarding: several
    // OpenAI-compatible upstreams (e.g. Azure OpenAI) reject unknown params
    // with 400. Attribution flows through the usage event, never the
    // provider; `x-drgtw-meta-*` headers are the provider-neutral channel.
    let body_metadata_stripped = parsed
        .as_object_mut()
        .map(|obj| obj.remove("metadata").is_some())
        .unwrap_or(false);

    // Extract the caller-supplied model, then resolve it through the global
    // `[model_aliases]` table (one level only) and rewrite the body `model`
    // field so ALL downstream logic — routing, allowlist, cost lookup, usage
    // events — sees the resolved model.
    let requested_model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or(ProxyError::MissingModel)?
        .to_owned();
    let model = live.config.resolve_model_alias(&requested_model).to_owned();
    let model_rewritten = model != requested_model;
    if model_rewritten
        && let Some(obj) = parsed.as_object_mut()
    {
        obj.insert("model".to_owned(), serde_json::Value::String(model.clone()));
    }

    let streaming = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    // 7. Resolve ordered candidate connections, then filter to this endpoint's
    //    format. Per WP 8.3: if zero candidates match the format, surface the
    //    existing FormatMismatch using the FIRST candidate's format.
    let all_candidates = resolved.connections_for_model(&model).map_err(ProxyError::from)?;
    let matching: Vec<&Connection> = all_candidates
        .iter()
        .copied()
        .filter(|c| spec_accepts_format(spec, c.format))
        .collect();
    if matching.is_empty() {
        let first_fmt = all_candidates[0].format;
        return Err(format_mismatch_error(spec, first_fmt, &model));
    }

    // When fallback is disabled, only the first matching candidate is tried.
    let candidates: Vec<&Connection> = if live.config.fallback.enabled {
        matching
    } else {
        vec![matching[0]]
    };

    // 7b. Native Bedrock streaming guard (0.0.2 limitation). `bedrock`
    //     connections cannot serve `stream:true` (native streaming uses
    //     `invoke-with-response-stream` eventstream framing, deferred), so for
    //     streaming requests they are removed from the candidate list — the
    //     fallback loop must never dispatch a stream to one, not even as a
    //     later candidate. If no candidate remains, reject with a clean 400
    //     BEFORE any upstream call (error in the endpoint's wire format).
    let candidates: Vec<&Connection> = if streaming {
        let non_bedrock: Vec<&Connection> = candidates
            .iter()
            .copied()
            .filter(|c| c.format != ApiFormat::Bedrock)
            .collect();
        if non_bedrock.is_empty() {
            return Err(ProxyError::FormatMismatch(
                "native Bedrock streaming is not supported in this release; \
                 retry without `stream: true`"
                    .to_owned(),
            ));
        }
        non_bedrock
    } else {
        candidates
    };

    // 8. Anthropic only: determine the anthropic-version header to forward.
    let anthropic_version = parts
        .headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("2023-06-01")
        .to_owned();

    // 8b. Native Bedrock body transform (the ONE place the messages path is not
    //     a pure passthrough). For a `bedrock` connection, InvokeModel takes the
    //     model id in the URL path, not the body, and requires the
    //     `anthropic_version` Bedrock marker. We therefore, on the parsed value:
    //       * remove the top-level `model` field (it moves into the URL), and
    //       * insert `"anthropic_version": "bedrock-2023-05-31"` if the client
    //         did not already supply one (client value is preserved).
    //     Done before pseudonymisation so the rewritten value flows through the
    //     existing serialise-once path. `model_for_transform` forces a
    //     re-serialise even on the PII-off path.
    let bedrock_transform = candidates[0].format == ApiFormat::Bedrock;
    if bedrock_transform
        && let Some(obj) = parsed.as_object_mut()
    {
        obj.remove("model");
        obj.entry("anthropic_version".to_owned())
            .or_insert_with(|| serde_json::Value::String("bedrock-2023-05-31".to_owned()));
    }

    // 9. PII request rewrite (WP 3.4 / 4.4). The pseudonymised bytes are reused
    //    as-is across every fallback attempt (same map — required for restore).
    let (upstream_body, pii_map) = if pii_mode == PiiMode::On {
        let engine = Arc::clone(&live.pii);
        let body_format = spec.body_format;
        let raw_for_fallback = raw_body.clone();
        // WP 9.3: when a persistent vault is configured, build the request map
        // backed by it so placeholders are STABLE across requests (the
        // embeddings/RAG guarantee). Without a vault, behaviour is unchanged.
        let store = state.entity_store.clone();
        let (rewritten, map) = tokio::task::spawn_blocking(move || {
            let mut map = match store {
                Some(s) => EntityMap::with_store(s),
                None => EntityMap::new(),
            };
            try_pseudonymize_body(body_format, &mut parsed, &engine, &mut map)?;
            Ok::<_, drgtw_pii::DetectError>((serde_json::to_vec(&parsed).ok(), map))
        })
        .await
        .map_err(|e| pii_unavailable(spec, e.to_string()))?
        .map_err(|e| pii_unavailable(spec, e.to_string()))?;
        let rewritten = rewritten.unwrap_or_else(|| raw_for_fallback.to_vec());
        (Bytes::from(rewritten), Arc::new(map))
    } else if model_rewritten || bedrock_transform || body_metadata_stripped {
        // PII off but the body was rewritten (alias resolution, the Bedrock
        // model-strip + anthropic_version injection, and/or the attribution
        // `metadata` strip): re-serialize so the upstream sees the rewritten
        // body. Falls back to the original bytes if serialization somehow
        // fails.
        let rewritten = serde_json::to_vec(&parsed).unwrap_or_else(|_| raw_body.to_vec());
        (Bytes::from(rewritten), Arc::new(EntityMap::new()))
    } else {
        // PII off, no rewrite → byte-identical passthrough.
        (raw_body, Arc::new(EntityMap::new()))
    };

    let pii_flag = pii_mode == PiiMode::On && !pii_map.is_empty();

    let span = info_span!(
        "proxy_request",
        endpoint = spec.name,
        key_id = %resolved.key_id,
        model = %model,
        streaming,
        request_id = %request_id,
    );

    let rate_allowed = if let RateDecision::Allowed { remaining, limit } = rate_decision {
        Some((remaining, limit))
    } else {
        None
    };

    let key_id = resolved.key_id.clone();

    async move {
        // 10. Fallback dispatch loop.
        let mut fallback_attempts: u32 = 0;
        let last_idx = candidates.len() - 1;
        let mut final_outcome: Option<(reqwest::Response, &Connection)> = None;
        let mut last_err: Option<ProxyError> = None;

        for (i, connection) in candidates.iter().copied().enumerate() {
            let is_last = i == last_idx;

            // For `bedrock_converse`, the wire body, URL, and signature all
            // differ from the OpenAI passthrough and must be computed per
            // candidate (the SigV4 signature binds to the resolved URL/host).
            // The translation runs on the post-PII OpenAI bytes (design §5).
            // `converse_*` holds the per-candidate body when this is a
            // bedrock_converse connection; otherwise the shared upstream body
            // is used verbatim.
            let converse_wire: Option<(Bytes, String, bool)> =
                if connection.format == ApiFormat::BedrockConverse {
                    Some(build_converse_body(&upstream_body)?)
                } else {
                    None
                };

            // URL + auth branch on the CONNECTION's format (not the endpoint
            // spec): a `bedrock` connection serves the `/v1/messages` surface
            // but dispatches to native InvokeModel with bearer auth; a
            // `bedrock_converse` connection serves the OpenAI surface but
            // dispatches to Converse / ConverseStream.
            let url = match connection.format {
                ApiFormat::OpenAi => chat_completions_url(&connection.base_url),
                ApiFormat::Anthropic => messages_url(&connection.base_url),
                ApiFormat::Bedrock => bedrock_invoke_url(&connection.base_url, &model),
                ApiFormat::BedrockConverse => {
                    // Stream flag from the translated body selects the endpoint;
                    // it always equals the request-level `streaming` here.
                    let (_, model_id, stream) =
                        converse_wire.as_ref().expect("converse body present");
                    if *stream {
                        converse_stream_url(&connection.base_url, model_id)
                    } else {
                        converse_url(&connection.base_url, model_id)
                    }
                }
            };

            let wire_body = match &converse_wire {
                Some((body, _, _)) => body.clone(),
                None => upstream_body.clone(),
            };

            let mut builder = state
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(wire_body.clone());
            builder = match connection.format {
                ApiFormat::OpenAi | ApiFormat::Bedrock => {
                    builder.header("Authorization", format!("Bearer {}", connection.api_key))
                }
                // bedrock_converse: SigV4 sign when AWS creds are present;
                // otherwise fall back to the Bedrock API key as a bearer token
                // (design §2 auth resolution order). Signing binds to the exact
                // post-translate wire bytes + URL + host.
                ApiFormat::BedrockConverse => match sigv4_creds_for(connection) {
                    Some(creds) => {
                        let signed = sign_bedrock_request(
                            "POST",
                            &url,
                            &[("content-type", "application/json")],
                            &wire_body,
                            &creds,
                            SystemTime::now(),
                        )
                        .map_err(|e| ProxyError::FormatMismatch(e.to_string()))?;
                        let mut b = builder;
                        for (name, value) in signed {
                            b = b.header(name, value);
                        }
                        b
                    }
                    None => {
                        builder.header("Authorization", format!("Bearer {}", connection.api_key))
                    }
                },
                ApiFormat::Anthropic => builder
                    .header("x-api-key", connection.api_key.clone())
                    .header("anthropic-version", &anthropic_version),
            };

            match builder.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if is_retriable_status(status) && !is_last {
                        // Retriable status with more candidates: try next.
                        fallback_attempts += 1;
                        continue;
                    }
                    // Success, non-retriable error, or last candidate: use it.
                    final_outcome = Some((resp, connection));
                    break;
                }
                Err(e) => {
                    // Connect/transport error.
                    if !is_last {
                        fallback_attempts += 1;
                        last_err = Some(ProxyError::Upstream(e));
                        continue;
                    }
                    last_err = Some(ProxyError::Upstream(e));
                    break;
                }
            }
        }

        let (upstream_resp, connection) = match final_outcome {
            Some(pair) => pair,
            None => {
                // Every candidate failed with a transport error: relay the last.
                // (No usage event — there is no upstream response to account.)
                return Err(last_err.unwrap_or(ProxyError::BodyTooLarge));
            }
        };

        let conn_name = connection.name.clone();
        let conn_format = connection.format;
        let cost_table = live.cost_tables.get(&conn_name).cloned().unwrap_or_default();

        // 11. Relay (with PII restore) + capture usage + emit event.
        let ctx = RelayCtx {
            state: Arc::clone(&state),
            spec,
            conn_format,
            request_id: request_id.clone(),
            key_id: key_id.clone(),
            model: model.clone(),
            conn_name,
            cost_table,
            pii_flag,
            started,
            fallback_attempts,
            debug,
            upstream_body: upstream_body.clone(),
            metadata: metadata.clone(),
            base_url: connection.base_url.clone(),
            pii_entities: pii_map.len() as u64,
            pii_map: Arc::clone(&pii_map),
        };

        let mut resp = relay_with_usage(
            upstream_resp,
            streaming,
            pii_mode,
            pii_map,
            ctx,
        )
        .await?;

        if let Some((remaining, limit)) = rate_allowed {
            attach_rate_limit_allowed_headers(resp.headers_mut(), remaining, limit);
        }
        if fallback_attempts > 0
            && let Ok(v) = HeaderValue::from_str(&fallback_attempts.to_string())
        {
            resp.headers_mut()
                .insert(HeaderName::from_static("x-drgtw-fallback-attempts"), v);
        }

        Ok(resp)
    }
    .instrument(span)
    .await
}

// ---------------------------------------------------------------------------
// Relay + usage capture
// ---------------------------------------------------------------------------

/// Context needed to compute cost and emit a usage event.
struct RelayCtx {
    state: Arc<ProxyState>,
    spec: &'static EndpointSpec,
    /// Format of the connection that actually served this request. Differs from
    /// `spec.format` for connections served on a foreign endpoint surface
    /// (`bedrock_converse` on the OpenAI endpoint, native `bedrock` on the
    /// messages endpoint). Drives Converse response/stream translation.
    conn_format: ApiFormat,
    request_id: String,
    key_id: String,
    model: String,
    conn_name: String,
    cost_table: HashMap<String, ModelCost>,
    pii_flag: bool,
    started: Instant,
    fallback_attempts: u32,
    /// Demo debug header (`x-drgtw-debug: on`). Honoured only on non-streaming
    /// PII-on success responses; otherwise a no-op.
    debug: bool,
    /// The exact bytes sent upstream (post-pseudonymization). Used to populate
    /// `drgtw_debug.pseudonymized_request` when `debug` is set.
    upstream_body: Bytes,
    /// Caller-supplied attribution metadata (Feature 2). `None` when absent.
    metadata: Option<BTreeMap<String, String>>,
    /// Upstream base URL of the serving connection. Used only to derive
    /// `server.address`/`server.port` span attributes (host + port, never
    /// path/query).
    base_url: String,
    /// Number of PII entities pseudonymized this request (count only — the
    /// entities themselves never reach telemetry).
    pii_entities: u64,
    /// Per-request entity map. Used by `emit_event` to derive per-kind
    /// detection counts for `pii_detections` history rows.  Empty when PII
    /// is off or no entities were found.
    pii_map: Arc<EntityMap>,
}

/// Relay an upstream response as an axum response, restoring PII on 2xx JSON
/// bodies and capturing usage for the event sink.
async fn relay_with_usage(
    upstream_resp: reqwest::Response,
    streaming: bool,
    pii_mode: PiiMode,
    pii_map: Arc<EntityMap>,
    ctx: RelayCtx,
) -> Result<Response, ProxyError> {
    let upstream_status =
        StatusCode::from_u16(upstream_resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type: Option<HeaderValue> = upstream_resp.headers().get("content-type").cloned();

    // Streaming restore is within-request only: gated on a non-empty map.
    let should_restore_stream = pii_mode == PiiMode::On && !pii_map.is_empty();

    // Non-streaming restore additionally consults the persistent vault for
    // placeholders left by PAST requests (WP 9.3, the RAG case), so it runs
    // whenever PII is on AND (the current map has entries OR a store is present).
    let store_present = ctx.state.entity_store.is_some();
    let should_restore_body =
        pii_mode == PiiMode::On && (!pii_map.is_empty() || store_present);

    // Non-2xx: relay verbatim. Still emit an event (error path, post-auth).
    if !upstream_status.is_success() {
        let resp_bytes: Bytes = upstream_resp.bytes().await.map_err(ProxyError::Upstream)?;
        let status = upstream_status.as_u16();
        emit_event(&ctx, status, None, None, None, None, false);
        emit_trace_from_ctx(
            &ctx,
            status,
            None,
            None,
            Some(format!("upstream returned status {status}")),
        );
        return Ok(build_response(upstream_status, content_type, Body::from(resp_bytes)));
    }

    if streaming {
        // Streaming: usage captured via the tap wrapper; event emitted at end.
        // Restore stays within-request (the local map); past-request placeholders
        // in SSE chunks pass through untouched (documented limitation, WP 9.3).
        let restorer = if should_restore_stream {
            Some(SseRestorer::new(
                StreamRestorer::new(Arc::clone(&pii_map)),
                ctx.spec.body_format,
            ))
        } else {
            None
        };
        let status_u16 = upstream_status.as_u16();
        let format = ctx.spec.body_format;
        let is_converse = ctx.conn_format == ApiFormat::BedrockConverse;
        let model_for_stream = ctx.model.clone();
        let raw_stream = upstream_resp.bytes_stream();

        // Trace at handoff: latency = time-to-handoff, status known, no body
        // buffering (task 4). Token counts are not yet available for the stream.
        // Emitted BEFORE the completion closure consumes `ctx`.
        emit_trace_from_ctx(&ctx, status_u16, None, None, None);

        // OTel span enrichment at handoff: the request span closes when the
        // response is handed back, so allow-listed attrs (status; tokens not yet
        // known) go on now. Token/cost metrics are recorded at stream completion
        // by `emit_event`.
        otel_enrich::enrich_span(&otel_telemetry(&ctx, status_u16, None, None, None, None, true));

        // Move the pieces needed at completion into the callback. Cost is only
        // computed when both token counts are present (e.g. OpenAI without
        // include_usage yields None tokens → None cost → no budget record).
        let on_complete = move |usage: StreamUsage| {
            let (input, output) = (usage.input_tokens, usage.output_tokens);
            let cost = match (input, output) {
                (Some(i), Some(o)) => cost_for(&ctx.cost_table, &ctx.model, i, o),
                _ => None,
            };
            if let Some(c) = cost {
                ctx.state.live.load().budget.record(&ctx.key_id, c);
            }
            let ttft_s = usage.first_chunk_at.map(|t| t.duration_since(ctx.started).as_secs_f64());
            emit_event(&ctx, status_u16, input, output, cost, ttft_s, true);
        };

        // bedrock_converse: the upstream body is a binary AWS eventstream. Wrap
        // the raw stream in a re-framer that decodes each frame and emits OpenAI
        // SSE bytes, so the existing OpenAiChat usage tap + SSE PII restorer run
        // unchanged (design §5 streaming composition). The client always sees
        // `text/event-stream`, never the upstream eventstream content-type.
        if is_converse {
            let reframed = reframe_converse_stream(raw_stream, model_for_stream);
            let body =
                Body::from_stream(usage_tap_stream(reframed, restorer, format, on_complete));
            let sse_ct = HeaderValue::from_static("text/event-stream");
            return Ok(build_response(upstream_status, Some(sse_ct), body));
        }

        let body = Body::from_stream(usage_tap_stream(raw_stream, restorer, format, on_complete));
        return Ok(build_response(upstream_status, content_type, body));
    }

    // Non-streaming: buffer, parse usage, restore PII, emit event.
    let resp_bytes: Bytes = upstream_resp.bytes().await.map_err(ProxyError::Upstream)?;

    let is_json = content_type
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/json"))
        .unwrap_or(false);

    // Parse usage from the (un-restored) JSON body — restore does not change
    // token counts, but we extract before re-serialising for clarity.
    let parsed_body: Option<serde_json::Value> =
        if is_json { serde_json::from_slice(&resp_bytes).ok() } else { None };

    // bedrock_converse: translate the Converse response JSON into the OpenAI
    // `chat.completion` shape BEFORE usage extraction + PII restore, so the
    // existing OpenAI extractor reads `usage.prompt_tokens`/`completion_tokens`
    // and the client receives an OpenAI-shaped body (design §5 non-streaming).
    // `resp_bytes` is refreshed so the no-restore fallthrough returns the
    // translated body.
    let (parsed_body, resp_bytes) = match (ctx.conn_format, &parsed_body) {
        (ApiFormat::BedrockConverse, Some(converse)) => {
            let translated = converse_to_openai(converse, &ctx.model);
            let bytes = serde_json::to_vec(&translated)
                .map(Bytes::from)
                .unwrap_or(resp_bytes);
            (Some(translated), bytes)
        }
        _ => (parsed_body, resp_bytes),
    };

    let (input, output) = parsed_body
        .as_ref()
        .and_then(|v| match ctx.spec.format {
            // OPENAI_SPEC serves both `open_ai` and `bedrock_converse`; once a
            // Converse response is translated to OpenAI shape it carries
            // `usage.prompt_tokens`/`completion_tokens`, so the OpenAI extractor
            // reads token counts for both.
            ApiFormat::OpenAi | ApiFormat::BedrockConverse => extract_usage_openai(v),
            // The messages endpoint serves both `anthropic` and `bedrock`
            // connections; native Bedrock InvokeModel returns the Anthropic
            // Messages JSON, so the same extractor reads input/output tokens.
            ApiFormat::Anthropic | ApiFormat::Bedrock => extract_usage_anthropic(v),
        })
        .map(|(i, o)| (Some(i), Some(o)))
        .unwrap_or((None, None));

    let cost = match (input, output) {
        (Some(i), Some(o)) => cost_for(&ctx.cost_table, &ctx.model, i, o),
        _ => None,
    };
    if let Some(c) = cost {
        ctx.state.live.load().budget.record(&ctx.key_id, c);
    }
    emit_event(&ctx, upstream_status.as_u16(), input, output, cost, None, false);
    emit_trace_from_ctx(&ctx, upstream_status.as_u16(), input, output, None);

    // Demo debug block (`x-drgtw-debug: on`): only meaningful when PII is on for
    // a non-streaming JSON success response. Captured from the PRE-restore body
    // and the upstream (pseudonymized) request bytes, then inserted into the
    // response root AFTER restore. The entity mapping itself is NEVER emitted —
    // only the count.
    let want_debug = ctx.debug && pii_mode == PiiMode::On && parsed_body.is_some();
    let debug_block = if want_debug {
        Some(build_debug_block(
            ctx.spec.body_format,
            &ctx.upstream_body,
            parsed_body.as_ref().expect("checked is_some"),
            pii_map.len(),
        ))
    } else {
        None
    };

    // Build the (optionally restored) response body. Pass 1 restores from the
    // current request's map; pass 2 (when a vault is present) resolves
    // placeholders from PAST requests via the store (WP 9.3).
    //
    // We also re-serialize when a debug block must be attached even if no
    // restore is needed (PII on but zero entities detected) — so the demo
    // panels always show what was sent, rather than going blank.
    if (should_restore_body || debug_block.is_some())
        && let Some(mut value) = parsed_body
    {
        if should_restore_body {
            restore_body_with_store(
                ctx.spec.body_format,
                &mut value,
                &pii_map,
                ctx.state.entity_store.as_deref(),
            );
        }
        if let (Some(block), Some(obj)) = (debug_block, value.as_object_mut()) {
            obj.insert("drgtw_debug".to_owned(), block);
        }
        let restored = serde_json::to_vec(&value).unwrap_or_else(|_| resp_bytes.to_vec());
        return Ok(build_response(upstream_status, content_type, Body::from(restored)));
    }

    Ok(build_response(upstream_status, content_type, Body::from(resp_bytes)))
}

/// Re-frame a binary AWS eventstream (Converse / ConverseStream) into an OpenAI
/// SSE byte stream.
///
/// Each upstream chunk is fed to an [`EventStreamDecoder`]; every complete frame
/// is parsed (JSON payload) and translated to OpenAI `chat.completion.chunk` SSE
/// bytes via [`converse_event_to_sse`], threading a [`StreamXlateState`] so the
/// synthesised id is stable and `[DONE]` is emitted once. The output type is
/// `Result<Bytes, reqwest::Error>` so it drops straight into
/// [`usage_tap_stream`] in place of the raw stream (design §5, §7).
///
/// A mid-stream decoder error (CRC / length / header violation) terminates the
/// stream: the partial output already emitted is flushed and the stream ends
/// (logged via `tracing`, never carrying body content — the PII-gateway
/// invariant). Converse `*Exception` events are NOT decode errors; they decode
/// cleanly and `converse_event_to_sse` turns them into an OpenAI error chunk
/// plus `[DONE]`.
fn reframe_converse_stream<S>(
    upstream: S,
    model: String,
) -> impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static
where
    S: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    use futures::stream;
    use futures::StreamExt as _;

    struct State<S> {
        upstream: std::pin::Pin<Box<S>>,
        decoder: EventStreamDecoder,
        xlate: StreamXlateState,
        model: String,
        finished: bool,
    }

    let init = State {
        upstream: Box::pin(upstream),
        decoder: EventStreamDecoder::new(),
        xlate: StreamXlateState::new(),
        model,
        finished: false,
    };

    stream::unfold(init, |mut st| async move {
        if st.finished {
            return None;
        }
        loop {
            match st.upstream.as_mut().next().await {
                Some(Ok(bytes)) => {
                    st.decoder.feed(&bytes);
                    let frames = match st.decoder.drain() {
                        Ok(frames) => frames,
                        Err(e) => {
                            // Fatal framing violation (or a non-eventstream
                            // body, e.g. an HTML error page): close the SSE
                            // stream COMPLETELY — synthesize `[DONE]` when the
                            // terminal chunk never arrived. No body content is
                            // logged (PII invariant).
                            tracing::warn!(error = %e, "converse eventstream decode error; terminating stream");
                            st.finished = true;
                            let tail = st.xlate.finalize();
                            if tail.is_empty() {
                                return None;
                            }
                            return Some((Ok(Bytes::from(tail)), st));
                        }
                    };
                    let mut out = Vec::new();
                    for frame in frames {
                        let Some(event_type) = frame
                            .event_type
                            .as_deref()
                            .or(frame.exception_type.as_deref())
                        else {
                            continue;
                        };
                        let payload: serde_json::Value = serde_json::from_slice(&frame.payload)
                            .unwrap_or_else(|e| {
                                // Payload content is never logged (PII invariant).
                                tracing::warn!(error = %e, event_type, "malformed converse event payload JSON");
                                serde_json::Value::Null
                            });
                        out.extend(converse_event_to_sse(
                            event_type,
                            &payload,
                            &st.model,
                            &mut st.xlate,
                        ));
                    }
                    if out.is_empty() {
                        // No complete frame yet (or only no-op events); keep
                        // pulling rather than yielding an empty chunk.
                        continue;
                    }
                    return Some((Ok(Bytes::from(out)), st));
                }
                Some(Err(e)) => {
                    // Upstream transport error mid-stream: surface it. The tap
                    // treats an Err item as terminal; reqwest's byte stream is
                    // fused, so a re-poll would yield `None` and end this stream
                    // via the flush arm below.
                    return Some((Err(e), st));
                }
                None => {
                    // Upstream ended. Flush any final buffered frames.
                    st.finished = true;
                    let frames = match st.decoder.drain() {
                        Ok(frames) => frames,
                        Err(e) => {
                            tracing::warn!(error = %e, "converse eventstream decode error at end of stream");
                            let tail = st.xlate.finalize();
                            if tail.is_empty() {
                                return None;
                            }
                            return Some((Ok(Bytes::from(tail)), st));
                        }
                    };
                    let mut out = Vec::new();
                    for frame in frames {
                        let Some(event_type) = frame
                            .event_type
                            .as_deref()
                            .or(frame.exception_type.as_deref())
                        else {
                            continue;
                        };
                        let payload: serde_json::Value = serde_json::from_slice(&frame.payload)
                            .unwrap_or_else(|e| {
                                // Payload content is never logged (PII invariant).
                                tracing::warn!(error = %e, event_type, "malformed converse event payload JSON");
                                serde_json::Value::Null
                            });
                        out.extend(converse_event_to_sse(
                            event_type,
                            &payload,
                            &st.model,
                            &mut st.xlate,
                        ));
                    }
                    // Upstream ended without a terminal `metadata` event
                    // (disconnect): complete the SSE stream for the client.
                    out.extend(st.xlate.finalize());
                    if out.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(out)), st));
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Event emission
// ---------------------------------------------------------------------------

/// Build the allow-listed [`drgtw_otel::RequestTelemetry`] for this request.
fn otel_telemetry(
    ctx: &RelayCtx,
    status: u16,
    input: Option<u64>,
    output: Option<u64>,
    cost: Option<f64>,
    ttft_s: Option<f64>,
    streamed: bool,
) -> drgtw_otel::RequestTelemetry {
    otel_enrich::telemetry(
        ctx.spec.name,
        ctx.spec.format,
        &ctx.model,
        &ctx.conn_name,
        &ctx.base_url,
        &ctx.key_id,
        &ctx.request_id,
        Some(status),
        otel_enrich::error_class_for_status(status),
        input,
        output,
        cost,
        Some(ctx.started.elapsed().as_secs_f64()),
        ttft_s,
        ctx.pii_flag,
        streamed,
        ctx.fallback_attempts,
    )
}

/// Emit a usage event when a sink is configured. `cost` is precomputed by the
/// caller (which has already recorded it against the budget).
///
/// Also the OTel chokepoint: records metrics (when `[otel] metrics` is on) and
/// enriches the current request span — except on the streaming completion path
/// (`streamed`), which runs after the request span has closed; there the span
/// was already enriched at handoff and only metrics are recorded here.
fn emit_event(
    ctx: &RelayCtx,
    status: u16,
    input: Option<u64>,
    output: Option<u64>,
    cost: Option<f64>,
    ttft_s: Option<f64>,
    streamed: bool,
) {
    let telemetry = otel_telemetry(ctx, status, input, output, cost, ttft_s, streamed);
    if !streamed {
        otel_enrich::enrich_span(&telemetry);
    }
    otel_enrich::record_metrics(&ctx.state, &telemetry, ctx.pii_entities);

    let ev = UsageEvent {
        request_id: ctx.request_id.clone(),
        key_id: ctx.key_id.clone(),
        endpoint: ctx.spec.name.to_owned(),
        model: ctx.model.clone(),
        connection: ctx.conn_name.clone(),
        status,
        input_tokens: input,
        output_tokens: output,
        cost_usd: cost,
        latency_ms: ctx.started.elapsed().as_millis() as u64,
        pii: ctx.pii_flag,
        streamed,
        fallback_attempts: ctx.fallback_attempts,
        ts_unix_ms: now_unix_ms(),
        metadata: ctx.metadata.clone(),
    };
    let _ = ctx.state.usage_broadcast.send(ev.clone());
    // Fire-and-forget persistence to the admin-UI history store. Spawned so the
    // database write never adds latency to the response path; `None` (no store
    // configured) is a cheap branch that records nothing. Errors are ignored —
    // history is best-effort observability, not part of the request contract.
    if let Some(history) = &ctx.state.history {
        let history = Arc::clone(history);
        let ev = ev.clone();
        tokio::spawn(async move {
            let _ = history.record_usage(&ev).await;
        });
    }
    // Fire-and-forget PII detection counts. One row per entity kind per request.
    // Guarded on pii_flag so the hot path (PII off) is a single branch.
    if ctx.pii_flag {
        if let Some(history) = &ctx.state.history {
            let kind_counts = pii_kind_counts(&ctx.pii_map);
            if !kind_counts.is_empty() {
                let history = Arc::clone(history);
                let request_id = ctx.request_id.clone();
                let key_id = ctx.key_id.clone();
                let ts = now_unix_ms() as i64;
                tokio::spawn(async move {
                    let rows: Vec<drgtw_history::PiiDetectionRow> = kind_counts
                        .into_iter()
                        .map(|(entity_kind, count)| drgtw_history::PiiDetectionRow {
                            request_id: request_id.clone(),
                            key_id: key_id.clone(),
                            entity_kind,
                            count,
                            ts_unix_ms: ts,
                        })
                        .collect();
                    let _ = history.record_pii_detections(&rows).await;
                });
            }
        }
    }
    if let Some(sink) = &ctx.state.events {
        sink.emit(ev);
    }
}

// ---------------------------------------------------------------------------
// Trace emission
// ---------------------------------------------------------------------------

/// Build the kind-specific [`TraceKind`] for an LLM endpoint from its spec
/// name + extracted metadata. Bodies are NEVER included (PII gateway invariant).
fn llm_trace_kind(endpoint: &str, meta: LlmMeta) -> TraceKind {
    match endpoint {
        "messages" => TraceKind::Messages(meta),
        "embeddings" => TraceKind::Embeddings(meta),
        // "chat_completions" and any unexpected name default to chat.
        _ => TraceKind::Chat(meta),
    }
}

/// Emit a metadata-only trace entry for a completed LLM request.
///
/// No-op when tracing is disabled. Carries connection / model / token metadata
/// only — never request or response bodies.
#[allow(clippy::too_many_arguments)]
fn emit_trace_llm(
    state: &ProxyState,
    endpoint: &str,
    request_id: &str,
    key_id: &str,
    model: &str,
    conn_name: &str,
    status: u16,
    latency_ms: u64,
    input: Option<u64>,
    output: Option<u64>,
    error: Option<String>,
) {
    let Some(trace) = &state.trace else { return };
    let meta = LlmMeta {
        model: Some(model.to_owned()),
        connection: Some(conn_name.to_owned()),
        input_tokens: input,
        output_tokens: output,
    };
    trace.emit(TraceEntry {
        ts: rfc3339_now(),
        request_id: request_id.to_owned(),
        virtual_key: Some(key_id.to_owned()),
        status: Some(status),
        latency_ms: Some(latency_ms),
        error,
        detail: llm_trace_kind(endpoint, meta),
    });
}

/// Emit the LLM trace entry for a relayed request, deriving fields from the
/// [`RelayCtx`]. `error` is set for non-2xx / local failures where known.
fn emit_trace_from_ctx(
    ctx: &RelayCtx,
    status: u16,
    input: Option<u64>,
    output: Option<u64>,
    error: Option<String>,
) {
    emit_trace_llm(
        &ctx.state,
        ctx.spec.name,
        &ctx.request_id,
        &ctx.key_id,
        &ctx.model,
        &ctx.conn_name,
        status,
        ctx.started.elapsed().as_millis() as u64,
        input,
        output,
        error,
    );
}

/// Emit a trace entry for a pre-dispatch rejection (rate limit / budget). The
/// model and connection are not yet known at this point, so [`LlmMeta`] is left
/// default; status is always 429. No-op when tracing is disabled.
fn emit_trace_reject(
    state: &ProxyState,
    endpoint: &str,
    request_id: &str,
    key_id: &str,
    started: Instant,
    error: &str,
) {
    let Some(trace) = &state.trace else { return };
    trace.emit(TraceEntry {
        ts: rfc3339_now(),
        request_id: request_id.to_owned(),
        virtual_key: Some(key_id.to_owned()),
        status: Some(429),
        latency_ms: Some(started.elapsed().as_millis() as u64),
        error: Some(error.to_owned()),
        detail: llm_trace_kind(endpoint, LlmMeta::default()),
    });
}

/// RFC3339 (UTC) timestamp for trace entries. Reuses the trace crate's UTC
/// civil-time conversion is internal; we format here with a fixed-offset shape.
fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert unix seconds to a UTC civil date-time tuple `(year, month, day,
/// hour, min, sec)` (Howard Hinnant's days-from-civil inverse).
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Is this upstream status retriable for fallback purposes?
fn is_retriable_status(status: u16) -> bool {
    matches!(status, 502 | 503 | 504 | 429)
}

/// Resolve the per-connection SigV4 material for a `bedrock_converse` connection.
///
/// Returns `Some` only when BOTH the access-key id and the secret access key are
/// present (config validation guarantees `region` is set in that case, and that
/// a session token never appears without the key pair). A connection with only
/// an `api_key` returns `None`, signalling bearer auth.
fn sigv4_creds_for(connection: &Connection) -> Option<SigV4Creds> {
    match (
        connection.aws_access_key_id.as_deref(),
        connection.aws_secret_access_key.as_deref(),
    ) {
        (Some(akid), Some(secret)) if !akid.is_empty() && !secret.is_empty() => Some(SigV4Creds {
            access_key_id: akid.to_owned(),
            secret_access_key: secret.to_owned(),
            session_token: connection
                .aws_session_token
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            region: connection.region.clone().unwrap_or_default(),
        }),
        _ => None,
    }
}

/// Translate the (already-pseudonymized) OpenAI body bytes into a serialised
/// Converse request body, returning the bytes, the lifted model id, and the
/// stream flag. Surfaces a 400-class OpenAI error on translation failure (e.g.
/// non-text content) before any upstream call.
fn build_converse_body(openai_body: &[u8]) -> Result<(Bytes, String, bool), ProxyError> {
    let value: serde_json::Value =
        serde_json::from_slice(openai_body).unwrap_or(serde_json::Value::Null);
    let (converse, model_id, stream) =
        openai_to_converse(&value).map_err(|e| ProxyError::FormatMismatch(e.to_string()))?;
    let bytes = serde_json::to_vec(&converse).map_err(|e| ProxyError::FormatMismatch(e.to_string()))?;
    Ok((Bytes::from(bytes), model_id, stream))
}

/// Build the `drgtw_debug` object for the demo debug header.
///
/// * `pseudonymized_request` — the rewritten request JSON exactly as sent
///   upstream (parsed from `upstream_body`; falls back to a string if the bytes
///   aren't valid JSON, which should not happen on this path).
/// * `raw_response_text` — the assistant text BEFORE restore, one entry per
///   choice (OpenAI) or content block (Anthropic).
/// * `entities` — the number of distinct entities in the request map. The
///   mapping itself is intentionally omitted.
fn build_debug_block(
    format: BodyFormat,
    upstream_body: &Bytes,
    pre_restore_response: &serde_json::Value,
    entities: usize,
) -> serde_json::Value {
    let pseudonymized_request: serde_json::Value = serde_json::from_slice(upstream_body)
        .unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(upstream_body).into_owned())
        });

    let raw_response_text = extract_response_text(format, pre_restore_response);

    serde_json::json!({
        "pseudonymized_request": pseudonymized_request,
        "raw_response_text": raw_response_text,
        "entities": entities,
    })
}

/// Extract assistant text from a non-streaming response body, per choice
/// (OpenAI `choices[].message.content`) or per content block (Anthropic
/// `content[].text`). Non-string / missing fields are skipped.
fn extract_response_text(format: BodyFormat, response: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    match format {
        BodyFormat::OpenAiChat => {
            if let Some(choices) = response.get("choices").and_then(|c| c.as_array()) {
                for choice in choices {
                    if let Some(text) = choice
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                    {
                        out.push(text.to_owned());
                    }
                }
            }
        }
        BodyFormat::AnthropicMessages => {
            if let Some(blocks) = response.get("content").and_then(|c| c.as_array()) {
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        out.push(text.to_owned());
                    }
                }
            }
        }
    }
    out
}

fn format_mismatch_error(spec: &EndpointSpec, actual: ApiFormat, model: &str) -> ProxyError {
    let fmt_name = format_name(actual);
    // `spec` is only ever OPENAI_SPEC or ANTHROPIC_SPEC; Bedrock connections are
    // served via ANTHROPIC_SPEC, so treat any non-OpenAI spec as the messages
    // surface and point a mismatch back at /v1/chat/completions.
    let other_endpoint = match spec.format {
        // `spec.format` is only ever the endpoint's own format; bedrock_converse
        // is served via OPENAI_SPEC so it folds into the OpenAi arm.
        ApiFormat::OpenAi | ApiFormat::BedrockConverse => "/v1/messages",
        ApiFormat::Anthropic | ApiFormat::Bedrock => "/v1/chat/completions",
    };
    ProxyError::FormatMismatch(format!(
        "model `{model}` is served by a connection with format `{fmt_name}`; use {other_endpoint}"
    ))
}

fn pii_unavailable(spec: &EndpointSpec, detail: String) -> ProxyError {
    match spec.format {
        // OpenAi and bedrock_converse both serve the OpenAI-shaped chat surface.
        ApiFormat::OpenAi | ApiFormat::BedrockConverse => ProxyError::PiiUnavailable(detail),
        // Anthropic and Bedrock both serve the messages endpoint → anthropic body.
        ApiFormat::Anthropic | ApiFormat::Bedrock => ProxyError::PiiUnavailableAnthropic(detail),
    }
}

fn build_response(status: StatusCode, content_type: Option<HeaderValue>, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    if let Some(ct) = content_type {
        response.headers_mut().insert("content-type", ct);
    }
    response
}

/// Attach `x-ratelimit-limit` + `x-ratelimit-remaining` headers to a success response.
fn attach_rate_limit_allowed_headers(headers: &mut HeaderMap, remaining: u32, limit: u32) {
    if let Ok(v) = HeaderValue::from_str(&limit.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-limit"), v);
    }
    if let Ok(v) = HeaderValue::from_str(&remaining.to_string()) {
        headers.insert(HeaderName::from_static("x-ratelimit-remaining"), v);
    }
}

/// Count PII detections in `map` by entity kind, for `pii_detections` rows.
///
/// Placeholder keys have the form `EMAIL_1`, `PHONE_2`, `IBAN_1`, `CARD_3`,
/// etc. Strip the trailing `_N` suffix (all trailing `_` + digits) to obtain
/// the kind prefix. Returns a map of `kind_string → count`.
fn pii_kind_counts(map: &EntityMap) -> HashMap<String, i32> {
    let mut counts: HashMap<String, i32> = HashMap::new();
    for (placeholder, _original) in map.iter() {
        // Strip trailing `_<digits>` to get the kind prefix.
        let kind = match placeholder.rfind('_') {
            Some(idx) if placeholder[idx + 1..].chars().all(|c| c.is_ascii_digit()) => {
                &placeholder[..idx]
            }
            _ => placeholder,
        };
        *counts.entry(kind.to_owned()).or_insert(0) += 1;
    }
    counts
}

/// Human-readable format name for error messages.
fn format_name(fmt: ApiFormat) -> &'static str {
    match fmt {
        ApiFormat::OpenAi => "open_ai",
        ApiFormat::Anthropic => "anthropic",
        ApiFormat::Bedrock => "bedrock",
        ApiFormat::BedrockConverse => "bedrock_converse",
    }
}

// ---------------------------------------------------------------------------
// GET /v1/models
// ---------------------------------------------------------------------------

pub async fn list_models(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ProxyError> {
    let live = state.live.load();
    let started = Instant::now();
    let resolved = live.keys.authenticate(&headers).map_err(ProxyError::from)?;
    let request_id = resolve_request_id(&headers);

    let span = info_span!("list_models", key_id = %resolved.key_id);
    let _guard = span.enter();

    let models: Vec<serde_json::Value> = resolved
        .allowed_models()
        .into_iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "owned_by": "drgtw",
            })
        })
        .collect();

    let body = serde_json::json!({
        "object": "list",
        "data": models,
    });

    // Trace the (always-200) listing: kind=models, no model/connection fields.
    if let Some(trace) = &state.trace {
        trace.emit(TraceEntry {
            ts: rfc3339_now(),
            request_id: request_id.clone(),
            virtual_key: Some(resolved.key_id.clone()),
            status: Some(200),
            latency_ms: Some(started.elapsed().as_millis() as u64),
            error: None,
            detail: TraceKind::Models(LlmMeta::default()),
        });
    }

    drop(_guard);
    Ok(axum::Json(body))
}

// ---------------------------------------------------------------------------
// POST /v1/embeddings  (OpenAI format) — WP 9.3
// ---------------------------------------------------------------------------
//
// Works with the stock OpenAI SDK `client.embeddings.create`. The pipeline
// mirrors chat for auth/rate-limit/budget/PII-mode/fallback/usage, but:
//   * the request body shape is `{ model, input }` where `input` is a string,
//     an array of strings, or an array of token-id arrays (passed through
//     untouched — no text to scan);
//   * there is NO response restore (the response is opaque float vectors);
//   * usage accounts INPUT tokens only (`usage.prompt_tokens`), priced with the
//     model's input price only.
//   * the endpoint requires an `open_ai`-format connection.

pub async fn embeddings(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    match embeddings_inner(state, req).await {
        Ok(resp) => resp,
        Err(e) => e.into_response_fmt(ErrorFormat::OpenAi),
    }
}

async fn embeddings_inner(
    state: Arc<ProxyState>,
    req: Request,
) -> Result<Response, ProxyError> {
    // Load a snapshot of the hot-swappable live bundle once per request.
    let live = state.live.load();
    let started = Instant::now();
    let (parts, req_body) = req.into_parts();

    // 1. Authenticate. (Auth failures emit no event.)
    let resolved = live.keys.authenticate(&parts.headers).map_err(ProxyError::from)?;
    let request_id = resolve_request_id(&parts.headers);

    // 2. Rate-limit.
    let rate_decision = live.limiter.check(&resolved.key_id);
    if let RateDecision::Limited { retry_after_secs, limit } = rate_decision {
        emit_trace_reject(
            &state,
            "embeddings",
            &request_id,
            &resolved.key_id,
            started,
            "rate limited",
        );
        return Err(ProxyError::RateLimited { retry_after_secs, limit });
    }

    // 3. Budget (does not consume; spend recorded after the response usage).
    if let BudgetDecision::Exhausted { max_usd, retry_after_secs } =
        live.budget.check(&resolved.key_id)
    {
        emit_trace_reject(
            &state,
            "embeddings",
            &request_id,
            &resolved.key_id,
            started,
            "budget exhausted",
        );
        return Err(ProxyError::BudgetExhausted { max_usd, retry_after_secs });
    }

    // 4. PII mode.
    let pii_mode = resolve_pii_mode(
        &parts.headers,
        live.config.pii.enabled_by_default,
        ErrorFormat::OpenAi,
    )?;

    // 5. Buffer body (enforce max_body_bytes).
    let max = live.config.server.max_body_bytes;
    let raw_body: Bytes = match to_bytes(req_body, max).await {
        Ok(b) => b,
        Err(_) => return Err(ProxyError::BodyTooLarge),
    };

    // 6. Parse for routing + optional PII rewrite. `model` is required.
    let mut parsed: serde_json::Value =
        serde_json::from_slice(&raw_body).unwrap_or(serde_json::Value::Null);

    // Capture attribution metadata from the original body + inbound headers.
    let metadata = collect_metadata(&parts.headers, &parsed);

    // Drop the harvested body `metadata` object before forwarding (same
    // rationale as the chat path: Azure-style upstreams 400 on unknown params;
    // attribution lives in the event).
    let body_metadata_stripped = parsed
        .as_object_mut()
        .map(|obj| obj.remove("metadata").is_some())
        .unwrap_or(false);

    // Resolve the model through the global alias table (one level) and rewrite
    // the body so routing, allowlist, cost, and usage all see the resolved name.
    let requested_model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or(ProxyError::MissingModel)?
        .to_owned();
    let model = live.config.resolve_model_alias(&requested_model).to_owned();
    let model_rewritten = model != requested_model;
    if model_rewritten
        && let Some(obj) = parsed.as_object_mut()
    {
        obj.insert("model".to_owned(), serde_json::Value::String(model.clone()));
    }

    // 7. Resolve candidates, filter to open_ai connections.
    let all_candidates = resolved.connections_for_model(&model).map_err(ProxyError::from)?;
    let matching: Vec<&Connection> = all_candidates
        .iter()
        .copied()
        .filter(|c| c.format == ApiFormat::OpenAi)
        .collect();
    if matching.is_empty() {
        // No open_ai connection serves this model → format mismatch.
        return Err(ProxyError::FormatMismatch(format!(
            "model `{model}` is not served by an `open_ai`-format connection; \
             /v1/embeddings requires open_ai format"
        )));
    }
    let candidates: Vec<&Connection> = if live.config.fallback.enabled {
        matching
    } else {
        vec![matching[0]]
    };

    // 8. PII request rewrite (input string/array of strings). Token-id array
    //    inputs are passed through untouched. When a vault is configured the
    //    placeholders are STABLE across requests (the embeddings guarantee).
    let (upstream_body, pii_kinds): (Bytes, HashMap<String, i32>) = if pii_mode == PiiMode::On {
        let engine = Arc::clone(&live.pii);
        let store = state.entity_store.clone();
        let raw_for_fallback = raw_body.clone();
        let (rewritten, kinds) = tokio::task::spawn_blocking(move || {
            let mut map = match store {
                Some(s) => EntityMap::with_store(s),
                None => EntityMap::new(),
            };
            pseudonymize_embeddings_input(&mut parsed, &engine, &mut map)?;
            let kinds = pii_kind_counts(&map);
            Ok::<_, drgtw_pii::DetectError>((serde_json::to_vec(&parsed).ok(), kinds))
        })
        .await
        .map_err(|e| ProxyError::PiiUnavailable(e.to_string()))?
        .map_err(|e| ProxyError::PiiUnavailable(e.to_string()))?;
        let rewritten = rewritten.unwrap_or_else(|| raw_for_fallback.to_vec());
        (Bytes::from(rewritten), kinds)
    } else if model_rewritten || body_metadata_stripped {
        // PII off but the body was rewritten (alias resolution and/or the
        // attribution `metadata` strip): re-serialize so the upstream sees
        // the rewritten body.
        let rewritten = serde_json::to_vec(&parsed).unwrap_or_else(|_| raw_body.to_vec());
        (Bytes::from(rewritten), HashMap::new())
    } else {
        // PII off, no rewrite → byte-identical passthrough.
        (raw_body, HashMap::new())
    };
    let pii_used = !pii_kinds.is_empty();

    let span = info_span!(
        "proxy_request",
        endpoint = "embeddings",
        key_id = %resolved.key_id,
        model = %model,
        streaming = false,
        request_id = %request_id,
    );

    let rate_allowed = if let RateDecision::Allowed { remaining, limit } = rate_decision {
        Some((remaining, limit))
    } else {
        None
    };
    let key_id = resolved.key_id.clone();

    async move {
        // 9. Fallback dispatch loop (no streaming).
        let mut fallback_attempts: u32 = 0;
        let last_idx = candidates.len() - 1;
        let mut final_outcome: Option<(reqwest::Response, &Connection)> = None;
        let mut last_err: Option<ProxyError> = None;

        for (i, connection) in candidates.iter().copied().enumerate() {
            let is_last = i == last_idx;
            let url = embeddings_url(&connection.base_url);
            let builder = state
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", connection.api_key))
                .body(upstream_body.clone());

            match builder.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if is_retriable_status(status) && !is_last {
                        fallback_attempts += 1;
                        continue;
                    }
                    final_outcome = Some((resp, connection));
                    break;
                }
                Err(e) => {
                    if !is_last {
                        fallback_attempts += 1;
                        last_err = Some(ProxyError::Upstream(e));
                        continue;
                    }
                    last_err = Some(ProxyError::Upstream(e));
                    break;
                }
            }
        }

        let (upstream_resp, connection) = match final_outcome {
            Some(pair) => pair,
            None => return Err(last_err.unwrap_or(ProxyError::BodyTooLarge)),
        };

        let conn_name = connection.name.clone();
        let cost_table = live.cost_tables.get(&conn_name).cloned().unwrap_or_default();

        // 10. Relay verbatim (no restore) + capture input-only usage + event.
        let upstream_status = StatusCode::from_u16(upstream_resp.status().as_u16())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let content_type: Option<HeaderValue> =
            upstream_resp.headers().get("content-type").cloned();
        let resp_bytes: Bytes = upstream_resp.bytes().await.map_err(ProxyError::Upstream)?;

        // Parse input-token usage only (embeddings have no output tokens).
        let input_tokens: Option<u64> = if upstream_status.is_success() {
            serde_json::from_slice::<serde_json::Value>(&resp_bytes)
                .ok()
                .as_ref()
                .and_then(|v| v.get("usage").and_then(|u| u.get("prompt_tokens")).and_then(|t| t.as_u64()))
        } else {
            None
        };

        // Cost: input price only (output tokens = 0).
        let cost = input_tokens.and_then(|i| cost_for(&cost_table, &model, i, 0));
        if let Some(c) = cost {
            state.live.load().budget.record(&key_id, c);
        }

        let status_u16 = upstream_status.as_u16();
        let trace_error = if upstream_status.is_success() {
            None
        } else {
            Some(format!("upstream returned status {status_u16}"))
        };
        emit_trace_llm(
            &state,
            "embeddings",
            &request_id,
            &key_id,
            &model,
            &conn_name,
            status_u16,
            started.elapsed().as_millis() as u64,
            input_tokens,
            None,
            trace_error,
        );

        // OTel: enrich the request span + record metrics (embeddings emit
        // in-span; never streaming). Entity counts are not tracked on this
        // path — the pii flag alone is carried.
        let telemetry = otel_enrich::telemetry(
            "embeddings",
            connection.format,
            &model,
            &conn_name,
            &connection.base_url,
            &key_id,
            &request_id,
            Some(status_u16),
            otel_enrich::error_class_for_status(status_u16),
            input_tokens,
            None,
            cost,
            Some(started.elapsed().as_secs_f64()),
            None,
            pii_used,
            false,
            fallback_attempts,
        );
        otel_enrich::enrich_span(&telemetry);
        otel_enrich::record_metrics(&state, &telemetry, 0);

        {
            let ev = UsageEvent {
                request_id: request_id.clone(),
                key_id: key_id.clone(),
                endpoint: "embeddings".to_owned(),
                model: model.clone(),
                connection: conn_name,
                status: upstream_status.as_u16(),
                input_tokens,
                output_tokens: None,
                cost_usd: cost,
                latency_ms: started.elapsed().as_millis() as u64,
                pii: pii_used,
                streamed: false,
                fallback_attempts,
                ts_unix_ms: now_unix_ms(),
                metadata: metadata.clone(),
            };
            let _ = state.usage_broadcast.send(ev.clone());
            // Fire-and-forget persistence to the admin-UI history store (see the
            // matching block in `emit_event`). Never blocks the response.
            if let Some(history) = &state.history {
                let history = Arc::clone(history);
                let ev = ev.clone();
                tokio::spawn(async move {
                    let _ = history.record_usage(&ev).await;
                });
            }
            // Fire-and-forget PII detection counts (one row per kind).
            if pii_used {
                if let Some(history) = &state.history {
                    if !pii_kinds.is_empty() {
                        let history = Arc::clone(history);
                        let request_id = request_id.clone();
                        let key_id = key_id.clone();
                        let ts = now_unix_ms() as i64;
                        let kinds = pii_kinds.clone();
                        tokio::spawn(async move {
                            let rows: Vec<drgtw_history::PiiDetectionRow> = kinds
                                .into_iter()
                                .map(|(entity_kind, count)| drgtw_history::PiiDetectionRow {
                                    request_id: request_id.clone(),
                                    key_id: key_id.clone(),
                                    entity_kind,
                                    count,
                                    ts_unix_ms: ts,
                                })
                                .collect();
                            let _ = history.record_pii_detections(&rows).await;
                        });
                    }
                }
            }
            if let Some(sink) = &state.events {
                sink.emit(ev);
            }
        }

        let mut resp = build_response(upstream_status, content_type, Body::from(resp_bytes));
        if let Some((remaining, limit)) = rate_allowed {
            attach_rate_limit_allowed_headers(resp.headers_mut(), remaining, limit);
        }
        if fallback_attempts > 0
            && let Ok(v) = HeaderValue::from_str(&fallback_attempts.to_string())
        {
            resp.headers_mut()
                .insert(HeaderName::from_static("x-drgtw-fallback-attempts"), v);
        }
        Ok(resp)
    }
    .instrument(span)
    .await
}

/// Pseudonymize the OpenAI embeddings `input` field in place.
///
/// `input` may be:
///   * a string → scan + rewrite;
///   * an array of strings → each element scanned + rewritten;
///   * an array of integers, or an array of integer-arrays (token ids) →
///     passed through untouched (no text to scan).
///
/// Uses the fallible scan + map path so NER fail-closed errors and vault store
/// errors propagate as [`drgtw_pii::DetectError`] (fail-closed).
fn pseudonymize_embeddings_input(
    body: &mut serde_json::Value,
    engine: &drgtw_pii::PiiEngine,
    map: &mut EntityMap,
) -> Result<(), drgtw_pii::DetectError> {
    let Some(input) = body.get_mut("input") else {
        return Ok(());
    };

    if input.is_string() {
        let s = input.as_str().unwrap_or_default();
        let dets = engine.try_scan(s)?;
        let rewritten = map.try_pseudonymize(s, &dets)?;
        *input = serde_json::Value::String(rewritten);
        return Ok(());
    }

    if let Some(arr) = input.as_array_mut() {
        for item in arr {
            // Only string elements are scanned. Integers / token-id arrays are
            // left untouched.
            if item.is_string() {
                let s = item.as_str().unwrap_or_default();
                let dets = engine.try_scan(s)?;
                let rewritten = map.try_pseudonymize(s, &dets)?;
                *item = serde_json::Value::String(rewritten);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drgtw_config::PiiConfig;
    use drgtw_pii::{EntityMap, PiiEngine};

    fn engine_email_phone() -> PiiEngine {
        drgtw_pii::build_engine_with_ner(&PiiConfig::default(), std::path::Path::new("."))
            .expect("build PII engine")
    }

    /// Build a map by pseudonymising a string containing known PII types.
    fn map_from_text(text: &str) -> EntityMap {
        let engine = engine_email_phone();
        let mut map = EntityMap::new();
        let dets = engine.try_scan(text).expect("scan");
        map.try_pseudonymize(text, &dets).expect("pseudonymize");
        map
    }

    #[test]
    fn pii_kind_counts_empty_map() {
        let map = EntityMap::new();
        let counts = pii_kind_counts(&map);
        assert!(counts.is_empty(), "empty map → empty counts");
    }

    #[test]
    fn pii_kind_counts_single_email() {
        let map = map_from_text("Contact max.mustermann@example.com please.");
        let counts = pii_kind_counts(&map);
        assert_eq!(counts.get("EMAIL").copied(), Some(1), "one EMAIL detected");
    }

    #[test]
    fn pii_kind_counts_multiple_kinds() {
        // Two emails, one phone → EMAIL: 2, PHONE: 1
        let map = map_from_text(
            "a@example.com and b@example.com, phone +49 89 1234567",
        );
        let counts = pii_kind_counts(&map);
        assert_eq!(counts.get("EMAIL").copied(), Some(2), "two EMAILs");
        assert_eq!(counts.get("PHONE").copied(), Some(1), "one PHONE");
    }

    #[test]
    fn pii_kind_counts_strips_suffix_correctly() {
        // Synthesise placeholders like EMAIL_12 (multi-digit suffix).
        let map = map_from_text(concat!(
            "a@example.com b@example.com c@example.com ",
            "d@example.com e@example.com f@example.com ",
            "g@example.com h@example.com i@example.com ",
            "j@example.com k@example.com l@example.com",
        ));
        let counts = pii_kind_counts(&map);
        // Should have 12 distinct emails, all counted under EMAIL.
        assert_eq!(counts.get("EMAIL").copied(), Some(12), "twelve EMAIL entries");
        assert_eq!(counts.len(), 1, "only EMAIL kind present");
    }
}
