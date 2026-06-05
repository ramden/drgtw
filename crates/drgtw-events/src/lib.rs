//! # drgtw-events
//!
//! Usage-event pipeline for the DRGTW LLM gateway.
//!
//! ## Privacy invariant
//!
//! **This crate NEVER records request or response content, nor any API keys.**
//! [`UsageEvent`] contains only metadata (token counts, latency, model, status, etc.).
//! Any caller that adds content fields to a `UsageEvent` will break this contract.
//! Every code-path in this crate upholds this invariant; do not weaken it.
//!
//! ## Components
//!
//! - [`UsageEvent`] — the wire-format event struct (Serde-serialisable).
//! - [`EventSink`] — non-blocking async sink that POSTs events to a webhook URL.
//! - [`cost_for`] — pure cost-calculation from a model-cost table.
//! - [`extract_usage_openai`], [`extract_usage_anthropic`] and streaming
//!   helpers — pure token-extraction from response bodies.

pub mod cost;
pub mod event;
pub mod extract;
pub mod sink;

pub use cost::cost_for;
pub use event::UsageEvent;
pub use extract::{
    extract_usage_anthropic, extract_usage_anthropic_stream_delta,
    extract_usage_anthropic_stream_start, extract_usage_openai,
};
pub use sink::EventSink;

/// Lightweight mirror of `drgtw_config::ModelCost` used as the cost-table entry.
///
/// Once `drgtw-config` exports this type, callers may convert from it; until then
/// this identical copy keeps `drgtw-events` self-contained for unit tests.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCost {
    /// Price in USD per 1 000 000 input tokens.
    pub input_per_1m: f64,
    /// Price in USD per 1 000 000 output tokens.
    pub output_per_1m: f64,
}
