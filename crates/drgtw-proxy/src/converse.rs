//! OpenAI <-> Bedrock Converse translation (WP-3 / WP-4 per
//! `md/bedrock-converse-design.md`).
//!
//! Pure `serde_json::Value` transforms — no async, no I/O — so they unit-test
//! and fuzz cleanly. Three directions:
//!
//! - [`openai_to_converse`] — OpenAI `/v1/chat/completions` request JSON ->
//!   Converse request JSON, lifting `model` out to the URL path and reporting
//!   the `stream` flag (which selects `/converse` vs `/converse-stream`).
//! - [`converse_to_openai`] — Converse response JSON -> OpenAI
//!   `chat.completion` JSON (non-streaming).
//! - [`converse_event_to_sse`] — one decoded Converse stream event ->
//!   already-framed OpenAI SSE `data: {...}\n\n` bytes, threading a
//!   [`StreamXlateState`] across events.
//!
//! The emitted OpenAI shapes are byte-compatible with what
//! [`crate::sse_restore::SseRestorer`] (text at `choices[0].delta.content`) and
//! [`crate::usage_tap`] (top-level `usage` with `prompt_tokens`/
//! `completion_tokens`) already parse, so PII restore and usage capture compose
//! with zero changes to those modules (design §4.3, §6).

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

/// Translation failures surfaced as a 400-class OpenAI error by the caller
/// (design §8). All variants happen *before* any upstream call.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TranslateError {
    /// The request body was not a JSON object.
    #[error("request body must be a JSON object")]
    NotAnObject,
    /// `model` was missing or not a string.
    #[error("request is missing a string `model` field")]
    MissingModel,
    /// `messages` was missing or not an array.
    #[error("request is missing a `messages` array")]
    MissingMessages,
    /// A message carried non-text content (image/audio/etc.) — text-only this
    /// round (design §4.1, §11).
    #[error("unsupported message content: {0} (text-only this round)")]
    UnsupportedContent(String),
}

// ── Request: OpenAI -> Converse ───────────────────────────────────────────────

/// Translate an OpenAI `/v1/chat/completions` request body into a Converse
/// request body.
///
/// Returns `(converse_body, model_id, stream)`. `model` is removed from the
/// body and returned so the caller can build the `/model/{id}/converse[-stream]`
/// URL; `stream` selects the streaming endpoint (it is never forwarded in the
/// body).
///
/// System messages are extracted to a top-level `system` array; `user`/
/// `assistant` messages map to `messages[].content[].text`. Inference params
/// (`max_tokens`/`max_completion_tokens`, `temperature`, `top_p`, `stop`) map
/// into `inferenceConfig`. Every other OpenAI field (`tools`, `tool_choice`,
/// `functions`, `response_format`, `metadata`, `n`, `logprobs`, `user`, …) is
/// **dropped** — Converse rejects unknown top-level keys (design §4.1).
pub fn openai_to_converse(openai: &Value) -> Result<(Value, String, bool), TranslateError> {
    let obj = openai.as_object().ok_or(TranslateError::NotAnObject)?;

    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .ok_or(TranslateError::MissingModel)?
        .to_owned();

    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(TranslateError::MissingMessages)?;

    let mut system: Vec<Value> = Vec::new();
    let mut converse_messages: Vec<Value> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let text_parts = extract_text_parts(msg.get("content"))?;

        match role {
            "system" => {
                for text in text_parts {
                    system.push(json!({ "text": text }));
                }
            }
            // `user` / `assistant` (and any other role) pass through; Converse
            // only documents user/assistant but we forward the role verbatim
            // and let the upstream validate (design §4.1).
            other => {
                let content: Vec<Value> =
                    text_parts.into_iter().map(|t| json!({ "text": t })).collect();
                converse_messages.push(json!({ "role": other, "content": content }));
            }
        }
    }

    // inferenceConfig — only present keys are emitted.
    let mut inference = Map::new();

    // `max_completion_tokens` wins over `max_tokens` when both are present.
    let max_tokens = obj
        .get("max_completion_tokens")
        .and_then(Value::as_u64)
        .or_else(|| obj.get("max_tokens").and_then(Value::as_u64));
    if let Some(mt) = max_tokens {
        inference.insert("maxTokens".to_owned(), json!(mt));
    }
    if let Some(temp) = obj.get("temperature").and_then(Value::as_f64) {
        inference.insert("temperature".to_owned(), json!(temp));
    }
    if let Some(top_p) = obj.get("top_p").and_then(Value::as_f64) {
        inference.insert("topP".to_owned(), json!(top_p));
    }
    if let Some(stops) = stop_sequences(obj.get("stop")) {
        inference.insert("stopSequences".to_owned(), Value::Array(stops));
    }

    let mut converse = Map::new();
    converse.insert("messages".to_owned(), Value::Array(converse_messages));
    if !system.is_empty() {
        converse.insert("system".to_owned(), Value::Array(system));
    }
    if !inference.is_empty() {
        converse.insert("inferenceConfig".to_owned(), Value::Object(inference));
    }

    Ok((Value::Object(converse), model, stream))
}

/// Pull the text out of an OpenAI message `content` field.
///
/// - A string becomes a single-element vec.
/// - An array of parts keeps every `{type:"text", text}` part in order; any
///   non-text part (`image_url`, `input_audio`, …) is an
///   [`TranslateError::UnsupportedContent`] (text-only this round).
/// - Missing / null content yields no text parts (an empty vec).
fn extract_text_parts(content: Option<&Value>) -> Result<Vec<String>, TranslateError> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(parts)) => {
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                let part_type = part.get("type").and_then(Value::as_str);
                match part_type {
                    Some("text") => {
                        let text = part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.push(text.to_owned());
                    }
                    Some(other) => {
                        return Err(TranslateError::UnsupportedContent(other.to_owned()));
                    }
                    None => {
                        return Err(TranslateError::UnsupportedContent(
                            "untyped content part".to_owned(),
                        ));
                    }
                }
            }
            Ok(out)
        }
        Some(other) => Err(TranslateError::UnsupportedContent(format!(
            "content of type {}",
            json_type_name(other)
        ))),
    }
}

/// Normalise the OpenAI `stop` field (string OR array of strings) into a
/// Converse `stopSequences` array. Returns `None` when absent so the key is
/// omitted entirely.
fn stop_sequences(stop: Option<&Value>) -> Option<Vec<Value>> {
    match stop {
        Some(Value::String(s)) => Some(vec![Value::String(s.clone())]),
        Some(Value::Array(arr)) => {
            let seqs: Vec<Value> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| Value::String(s.to_owned())))
                .collect();
            if seqs.is_empty() {
                None
            } else {
                Some(seqs)
            }
        }
        _ => None,
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ── Response: Converse -> OpenAI (non-streaming) ──────────────────────────────

/// Translate a Converse response body into an OpenAI `chat.completion` body.
///
/// `output.message.content[].text` is concatenated into a single assistant
/// message; `stopReason` maps to `finish_reason` (see [`map_finish_reason`]);
/// `usage.{inputTokens,outputTokens,totalTokens}` are renamed to
/// `prompt_tokens`/`completion_tokens`/`total_tokens` so the existing
/// `extract_usage_openai` reads them unchanged (design §4.2, §6).
///
/// Synthesises `id` (`chatcmpl-<uuid>`), `object`, and `created` per the OpenAI
/// shape. Total failure modes are absent: a malformed/empty Converse body
/// yields an empty-content `stop` completion (best-effort), matching how the
/// rest of the gateway never panics on upstream JSON.
pub fn converse_to_openai(converse: &Value, model: &str) -> Value {
    let content = concat_output_text(converse);

    let stop_reason = converse.get("stopReason").and_then(Value::as_str);
    let finish_reason = map_finish_reason(stop_reason);

    let mut completion = Map::new();
    completion.insert("id".to_owned(), json!(synth_id()));
    completion.insert("object".to_owned(), json!("chat.completion"));
    completion.insert("created".to_owned(), json!(unix_secs()));
    completion.insert("model".to_owned(), json!(model));
    completion.insert(
        "choices".to_owned(),
        json!([{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish_reason,
        }]),
    );

    if let Some(usage) = converse.get("usage").and_then(Value::as_object) {
        completion.insert("usage".to_owned(), rename_usage(usage));
    }

    Value::Object(completion)
}

/// Concatenate every `output.message.content[].text` block in order.
fn concat_output_text(converse: &Value) -> String {
    let blocks = converse
        .get("output")
        .and_then(|o| o.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array);

    let mut text = String::new();
    if let Some(blocks) = blocks {
        for block in blocks {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    text
}

/// Rename Converse `usage` (`inputTokens`/`outputTokens`/`totalTokens`) into
/// the OpenAI shape (`prompt_tokens`/`completion_tokens`/`total_tokens`).
/// `total_tokens` falls back to `input+output` when the upstream omits it.
fn rename_usage(usage: &Map<String, Value>) -> Value {
    let input = usage.get("inputTokens").and_then(Value::as_u64);
    let output = usage.get("outputTokens").and_then(Value::as_u64);
    let total = usage.get("totalTokens").and_then(Value::as_u64).or_else(|| {
        match (input, output) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        }
    });

    let mut out = Map::new();
    out.insert("prompt_tokens".to_owned(), json!(input.unwrap_or(0)));
    out.insert("completion_tokens".to_owned(), json!(output.unwrap_or(0)));
    out.insert("total_tokens".to_owned(), json!(total.unwrap_or(0)));
    Value::Object(out)
}

/// Map a Converse `stopReason` to an OpenAI `finish_reason` (design §4.2).
///
/// `end_turn`/`stop_sequence`/`malformed_model_output` -> `stop`;
/// `max_tokens`/`model_context_window_exceeded` -> `length`;
/// `tool_use`/`malformed_tool_use` -> `tool_calls`;
/// `content_filtered`/`guardrail_intervened` -> `content_filter`;
/// anything unknown (or absent) -> `stop` (best-effort).
fn map_finish_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("max_tokens") | Some("model_context_window_exceeded") => "length",
        Some("tool_use") | Some("malformed_tool_use") => "tool_calls",
        Some("content_filtered") | Some("guardrail_intervened") => "content_filter",
        // end_turn / stop_sequence / malformed_model_output / unknown / None
        _ => "stop",
    }
}

// ── Streaming: Converse event -> OpenAI SSE ───────────────────────────────────

/// State threaded across Converse stream events by [`converse_event_to_sse`].
///
/// Holds the synthesised chat-completion id (stable across every chunk of one
/// response), the mapped `finish_reason` captured on `messageStop`, and a guard
/// so `[DONE]` is emitted at most once.
#[derive(Debug, Clone)]
pub struct StreamXlateState {
    id: String,
    created: u64,
    /// Mapped finish_reason held from `messageStop` until the final chunk.
    finish_reason: Option<&'static str>,
    /// `true` once a terminal (`metadata`/exception) chunk + `[DONE]` was sent.
    done: bool,
}

impl Default for StreamXlateState {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamXlateState {
    /// Fresh state for one streamed response (one synthesised id + `created`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: synth_id(),
            created: unix_secs(),
            finish_reason: None,
            done: false,
        }
    }

    /// Terminate an incomplete stream. When no terminal `metadata`/exception
    /// event arrived (upstream disconnect, framing error, non-eventstream
    /// body), emits the trailing `[DONE]` sentinel so the client always sees
    /// a complete SSE stream. Empty when the stream already ended normally.
    #[must_use]
    pub fn finalize(&mut self) -> Vec<u8> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        done_block()
    }
}

/// Translate one decoded Converse stream event into zero-or-more OpenAI SSE
/// `data: {...}\n\n` byte blocks (already framed), per design §4.3:
///
/// | Converse event       | Emitted                                               |
/// |----------------------|-------------------------------------------------------|
/// | `messageStart`       | `delta:{role:"assistant"}` chunk                      |
/// | `contentBlockDelta`  | `delta:{content:<text>}` chunk                        |
/// | `contentBlockStart`/`contentBlockStop` | nothing                          |
/// | `messageStop`        | nothing emitted; holds mapped `finish_reason`         |
/// | `metadata`           | final chunk (`finish_reason` + `usage`) then `[DONE]` |
/// | `*Exception`         | OpenAI error chunk then `[DONE]`                      |
///
/// `payload` is the parsed JSON of the event. Bytes are concatenated in caller
/// order to form the client SSE stream.
pub fn converse_event_to_sse(
    event_type: &str,
    payload: &Value,
    model: &str,
    state: &mut StreamXlateState,
) -> Vec<u8> {
    if state.done {
        return Vec::new();
    }

    match event_type {
        "messageStart" => {
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("assistant");
            sse_block(&chunk(state, model, json!({ "role": role }), None, None))
        }
        "contentBlockDelta" => {
            let text = payload
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str);
            match text {
                Some(text) => sse_block(&chunk(
                    state,
                    model,
                    json!({ "content": text }),
                    None,
                    None,
                )),
                // toolUse / other delta kinds are text-only-dropped this round.
                None => Vec::new(),
            }
        }
        "contentBlockStart" | "contentBlockStop" => Vec::new(),
        "messageStop" => {
            let stop_reason = payload.get("stopReason").and_then(Value::as_str);
            state.finish_reason = Some(map_finish_reason(stop_reason));
            Vec::new()
        }
        "metadata" => {
            let finish = state.finish_reason.unwrap_or("stop");
            let usage = payload
                .get("usage")
                .and_then(Value::as_object)
                .map(rename_usage);
            let mut out = sse_block(&chunk(
                state,
                model,
                json!({}),
                Some(finish),
                usage,
            ));
            out.extend_from_slice(&done_block());
            state.done = true;
            out
        }
        // Exception members (design §0, §8): emit an OpenAI-shaped error chunk
        // then terminate the stream so a mid-stream client gets a clean
        // terminus.
        other if other.ends_with("Exception") => {
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream stream error");
            let err = json!({
                "error": {
                    "message": message,
                    "type": other,
                }
            });
            let mut out = sse_block(&err);
            out.extend_from_slice(&done_block());
            state.done = true;
            out
        }
        // Unknown event type: ignore (forward nothing).
        _ => Vec::new(),
    }
}

/// Build one `chat.completion.chunk` object with a single choice.
fn chunk(
    state: &StreamXlateState,
    model: &str,
    delta: Value,
    finish_reason: Option<&str>,
    usage: Option<Value>,
) -> Value {
    let mut choice = Map::new();
    choice.insert("index".to_owned(), json!(0));
    choice.insert("delta".to_owned(), delta);
    choice.insert(
        "finish_reason".to_owned(),
        match finish_reason {
            Some(fr) => json!(fr),
            None => Value::Null,
        },
    );

    let mut obj = Map::new();
    obj.insert("id".to_owned(), json!(state.id));
    obj.insert("object".to_owned(), json!("chat.completion.chunk"));
    obj.insert("created".to_owned(), json!(state.created));
    obj.insert("model".to_owned(), json!(model));
    obj.insert("choices".to_owned(), json!([Value::Object(choice)]));
    if let Some(usage) = usage {
        obj.insert("usage".to_owned(), usage);
    }
    Value::Object(obj)
}

/// Frame a JSON value as an SSE `data:` block: `data: <json>\n\n`.
fn sse_block(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(serde_json::to_string(value).unwrap_or_default().as_bytes());
    out.extend_from_slice(b"\n\n");
    out
}

/// The terminal `data: [DONE]\n\n` block.
fn done_block() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}

// ── Shared synth helpers ──────────────────────────────────────────────────────

fn synth_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Concatenate all `delta.content` strings across an emitted OpenAI SSE
    /// byte stream (mirrors the sse_restore test helper).
    fn join_sse_content(bytes: &[u8]) -> String {
        let text = std::str::from_utf8(bytes).unwrap();
        let mut joined = String::new();
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload.trim() == "[DONE]" {
                continue;
            }
            let v: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(c) = v["choices"][0]["delta"]["content"].as_str() {
                joined.push_str(c);
            }
        }
        joined
    }

    /// Parse every `data:` block (excluding `[DONE]`) into JSON values.
    fn sse_values(bytes: &[u8]) -> Vec<Value> {
        let text = std::str::from_utf8(bytes).unwrap();
        let mut out = Vec::new();
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload.trim() == "[DONE]" {
                continue;
            }
            out.push(serde_json::from_str(payload).unwrap());
        }
        out
    }

    fn ends_with_done(bytes: &[u8]) -> bool {
        bytes.ends_with(b"data: [DONE]\n\n")
    }

    // ── openai_to_converse: full request translation ───────────────────────────

    #[test]
    fn request_system_multipart_and_params() {
        let openai = json!({
            "model": "eu.amazon.nova-pro-v1:0",
            "stream": false,
            "messages": [
                { "role": "system", "content": "You are helpful." },
                { "role": "user", "content": [
                    { "type": "text", "text": "Hello " },
                    { "type": "text", "text": "world" }
                ]},
                { "role": "assistant", "content": "Hi!" }
            ],
            "max_tokens": 256,
            "temperature": 0.7,
            "top_p": 0.9,
            "stop": ["END", "STOP"]
        });

        let (body, model, stream) = openai_to_converse(&openai).unwrap();
        assert_eq!(model, "eu.amazon.nova-pro-v1:0");
        assert!(!stream);

        // System lifted to top-level array.
        assert_eq!(body["system"], json!([{ "text": "You are helpful." }]));

        // Multi-part user text kept as separate content blocks, role preserved.
        assert_eq!(
            body["messages"][0],
            json!({ "role": "user", "content": [
                { "text": "Hello " }, { "text": "world" }
            ]})
        );
        assert_eq!(
            body["messages"][1],
            json!({ "role": "assistant", "content": [{ "text": "Hi!" }] })
        );

        // inferenceConfig.
        assert_eq!(body["inferenceConfig"]["maxTokens"], json!(256));
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.7));
        assert_eq!(body["inferenceConfig"]["topP"], json!(0.9));
        assert_eq!(
            body["inferenceConfig"]["stopSequences"],
            json!(["END", "STOP"])
        );
    }

    #[test]
    fn request_string_content_wrapped_in_text_block() {
        let openai = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "just a string" }]
        });
        let (body, _, _) = openai_to_converse(&openai).unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            json!([{ "text": "just a string" }])
        );
    }

    #[test]
    fn request_max_completion_tokens_wins_over_max_tokens() {
        let openai = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 100,
            "max_completion_tokens": 42
        });
        let (body, _, _) = openai_to_converse(&openai).unwrap();
        assert_eq!(body["inferenceConfig"]["maxTokens"], json!(42));
    }

    #[test]
    fn request_max_tokens_used_when_no_completion_variant() {
        let openai = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 77
        });
        let (body, _, _) = openai_to_converse(&openai).unwrap();
        assert_eq!(body["inferenceConfig"]["maxTokens"], json!(77));
    }

    #[test]
    fn request_stop_scalar_wrapped_into_array() {
        let openai = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "stop": "END"
        });
        let (body, _, _) = openai_to_converse(&openai).unwrap();
        assert_eq!(body["inferenceConfig"]["stopSequences"], json!(["END"]));
    }

    #[test]
    fn request_stream_flag_parsed() {
        let openai = json!({
            "model": "m",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let (_, _, stream) = openai_to_converse(&openai).unwrap();
        assert!(stream);
    }

    #[test]
    fn request_unsupported_params_dropped() {
        let openai = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "f" } }],
            "tool_choice": "auto",
            "functions": [{ "name": "legacy" }],
            "response_format": { "type": "json_object" },
            "metadata": { "trace": "x" },
            "n": 3,
            "logprobs": true,
            "user": "u-123",
            "stream_options": { "include_usage": true },
            "frequency_penalty": 0.5
        });
        let (body, _, _) = openai_to_converse(&openai).unwrap();
        let obj = body.as_object().unwrap();
        // Only the translated keys survive; nothing OpenAI-specific leaks.
        for dropped in [
            "tools",
            "tool_choice",
            "functions",
            "response_format",
            "metadata",
            "n",
            "logprobs",
            "user",
            "stream",
            "stream_options",
            "frequency_penalty",
            "model",
        ] {
            assert!(!obj.contains_key(dropped), "{dropped} should be dropped");
        }
        // No inferenceConfig at all since no inference params were present.
        assert!(!obj.contains_key("inferenceConfig"));
        assert!(obj.contains_key("messages"));
    }

    #[test]
    fn request_empty_system_omitted() {
        let openai = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let (body, _, _) = openai_to_converse(&openai).unwrap();
        assert!(!body.as_object().unwrap().contains_key("system"));
    }

    // ── openai_to_converse: error paths ────────────────────────────────────────

    #[test]
    fn request_not_an_object_errors() {
        let err = openai_to_converse(&json!([1, 2, 3])).unwrap_err();
        assert_eq!(err, TranslateError::NotAnObject);
    }

    #[test]
    fn request_missing_model_errors() {
        let openai = json!({ "messages": [] });
        assert_eq!(
            openai_to_converse(&openai).unwrap_err(),
            TranslateError::MissingModel
        );
    }

    #[test]
    fn request_missing_messages_errors() {
        let openai = json!({ "model": "m" });
        assert_eq!(
            openai_to_converse(&openai).unwrap_err(),
            TranslateError::MissingMessages
        );
    }

    #[test]
    fn request_image_content_unsupported() {
        let openai = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": [
                { "type": "image_url", "image_url": { "url": "http://x/y.png" } }
            ]}]
        });
        let err = openai_to_converse(&openai).unwrap_err();
        assert_eq!(
            err,
            TranslateError::UnsupportedContent("image_url".to_owned())
        );
    }

    // ── converse_to_openai: response translation ───────────────────────────────

    #[test]
    fn response_concatenates_content_and_renames_usage() {
        let converse = json!({
            "output": { "message": { "role": "assistant", "content": [
                { "text": "Hello, " },
                { "text": "world!" }
            ]}},
            "stopReason": "end_turn",
            "usage": { "inputTokens": 12, "outputTokens": 5, "totalTokens": 17 }
        });
        let out = converse_to_openai(&converse, "eu.amazon.nova-pro-v1:0");

        assert_eq!(out["object"], json!("chat.completion"));
        assert_eq!(out["model"], json!("eu.amazon.nova-pro-v1:0"));
        assert!(out["id"].as_str().unwrap().starts_with("chatcmpl-"));
        assert!(out["created"].as_u64().is_some());

        assert_eq!(out["choices"][0]["index"], json!(0));
        assert_eq!(out["choices"][0]["message"]["role"], json!("assistant"));
        assert_eq!(
            out["choices"][0]["message"]["content"],
            json!("Hello, world!")
        );
        assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));

        // Usage renamed to the OpenAI shape that extract_usage_openai reads.
        assert_eq!(out["usage"]["prompt_tokens"], json!(12));
        assert_eq!(out["usage"]["completion_tokens"], json!(5));
        assert_eq!(out["usage"]["total_tokens"], json!(17));
    }

    #[test]
    fn response_total_tokens_derived_when_absent() {
        let converse = json!({
            "output": { "message": { "content": [{ "text": "x" }] }},
            "stopReason": "end_turn",
            "usage": { "inputTokens": 4, "outputTokens": 6 }
        });
        let out = converse_to_openai(&converse, "m");
        assert_eq!(out["usage"]["total_tokens"], json!(10));
    }

    #[test]
    fn response_missing_usage_omitted() {
        let converse = json!({
            "output": { "message": { "content": [{ "text": "x" }] }},
            "stopReason": "end_turn"
        });
        let out = converse_to_openai(&converse, "m");
        assert!(out.as_object().unwrap().get("usage").is_none());
    }

    #[test]
    fn response_finish_reason_table() {
        let cases = [
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("malformed_model_output", "stop"),
            ("max_tokens", "length"),
            ("model_context_window_exceeded", "length"),
            ("tool_use", "tool_calls"),
            ("malformed_tool_use", "tool_calls"),
            ("content_filtered", "content_filter"),
            ("guardrail_intervened", "content_filter"),
            ("something_unknown", "stop"),
        ];
        for (stop_reason, expected) in cases {
            let converse = json!({
                "output": { "message": { "content": [] }},
                "stopReason": stop_reason
            });
            let out = converse_to_openai(&converse, "m");
            assert_eq!(
                out["choices"][0]["finish_reason"],
                json!(expected),
                "stopReason {stop_reason} -> {expected}"
            );
        }
    }

    #[test]
    fn response_absent_stop_reason_defaults_to_stop() {
        let converse = json!({ "output": { "message": { "content": [] }}});
        let out = converse_to_openai(&converse, "m");
        assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
    }

    #[test]
    fn response_malformed_empty_body_is_best_effort_stop() {
        let out = converse_to_openai(&json!({}), "m");
        assert_eq!(out["choices"][0]["message"]["content"], json!(""));
        assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
    }

    // ── converse_event_to_sse: stream chunk sequence ───────────────────────────

    #[test]
    fn stream_full_sequence_produces_valid_openai_chunks() {
        let model = "eu.amazon.nova-pro-v1:0";
        let mut state = StreamXlateState::new();
        let mut out = Vec::new();

        out.extend(converse_event_to_sse(
            "messageStart",
            &json!({ "role": "assistant" }),
            model,
            &mut state,
        ));
        out.extend(converse_event_to_sse(
            "contentBlockDelta",
            &json!({ "contentBlockIndex": 0, "delta": { "text": "Hello, " } }),
            model,
            &mut state,
        ));
        out.extend(converse_event_to_sse(
            "contentBlockDelta",
            &json!({ "contentBlockIndex": 0, "delta": { "text": "world!" } }),
            model,
            &mut state,
        ));
        out.extend(converse_event_to_sse(
            "contentBlockStop",
            &json!({ "contentBlockIndex": 0 }),
            model,
            &mut state,
        ));
        out.extend(converse_event_to_sse(
            "messageStop",
            &json!({ "stopReason": "end_turn" }),
            model,
            &mut state,
        ));
        out.extend(converse_event_to_sse(
            "metadata",
            &json!({ "usage": { "inputTokens": 9, "outputTokens": 3, "totalTokens": 12 } }),
            model,
            &mut state,
        ));

        // Text reassembles.
        assert_eq!(join_sse_content(&out), "Hello, world!");
        // Stream terminates with [DONE].
        assert!(ends_with_done(&out));

        let values = sse_values(&out);
        // First chunk: role delta.
        assert_eq!(values[0]["choices"][0]["delta"]["role"], json!("assistant"));
        assert_eq!(values[0]["object"], json!("chat.completion.chunk"));
        // contentBlockStop emits nothing, so values are: role, delta, delta, final.
        assert_eq!(values.len(), 4);

        // Final chunk carries finish_reason + usage in the OpenAI shape.
        let final_chunk = values.last().unwrap();
        assert_eq!(final_chunk["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(final_chunk["usage"]["prompt_tokens"], json!(9));
        assert_eq!(final_chunk["usage"]["completion_tokens"], json!(3));
        assert_eq!(final_chunk["usage"]["total_tokens"], json!(12));

        // Stable id across every chunk.
        let id = values[0]["id"].as_str().unwrap();
        assert!(id.starts_with("chatcmpl-"));
        for v in &values {
            assert_eq!(v["id"].as_str().unwrap(), id);
        }
    }

    #[test]
    fn stream_content_block_start_and_unknown_emit_nothing() {
        let mut state = StreamXlateState::new();
        assert!(converse_event_to_sse(
            "contentBlockStart",
            &json!({ "contentBlockIndex": 0 }),
            "m",
            &mut state
        )
        .is_empty());
        assert!(converse_event_to_sse("somethingNew", &json!({}), "m", &mut state).is_empty());
    }

    #[test]
    fn stream_delta_without_text_emits_nothing() {
        // e.g. a toolUse delta — text-only this round.
        let mut state = StreamXlateState::new();
        let out = converse_event_to_sse(
            "contentBlockDelta",
            &json!({ "delta": { "toolUse": { "input": "{}" } } }),
            "m",
            &mut state,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn stream_metadata_finish_reason_reflects_message_stop() {
        let mut state = StreamXlateState::new();
        converse_event_to_sse("messageStop", &json!({ "stopReason": "max_tokens" }), "m", &mut state);
        let out = converse_event_to_sse(
            "metadata",
            &json!({ "usage": { "inputTokens": 1, "outputTokens": 2, "totalTokens": 3 } }),
            "m",
            &mut state,
        );
        let final_chunk = &sse_values(&out)[0];
        assert_eq!(final_chunk["choices"][0]["finish_reason"], json!("length"));
    }

    #[test]
    fn stream_metadata_without_message_stop_defaults_to_stop() {
        let mut state = StreamXlateState::new();
        let out = converse_event_to_sse(
            "metadata",
            &json!({ "usage": { "inputTokens": 1, "outputTokens": 2 } }),
            "m",
            &mut state,
        );
        let final_chunk = &sse_values(&out)[0];
        assert_eq!(final_chunk["choices"][0]["finish_reason"], json!("stop"));
        // total derived.
        assert_eq!(final_chunk["usage"]["total_tokens"], json!(3));
    }

    #[test]
    fn stream_metadata_without_usage_emits_final_chunk_no_usage() {
        let mut state = StreamXlateState::new();
        converse_event_to_sse("messageStop", &json!({ "stopReason": "end_turn" }), "m", &mut state);
        let out = converse_event_to_sse("metadata", &json!({}), "m", &mut state);
        let final_chunk = &sse_values(&out)[0];
        assert_eq!(final_chunk["choices"][0]["finish_reason"], json!("stop"));
        assert!(final_chunk.as_object().unwrap().get("usage").is_none());
        assert!(ends_with_done(&out));
    }

    #[test]
    fn stream_terminates_after_done() {
        let mut state = StreamXlateState::new();
        let first = converse_event_to_sse(
            "metadata",
            &json!({ "usage": { "inputTokens": 1, "outputTokens": 1, "totalTokens": 2 } }),
            "m",
            &mut state,
        );
        assert!(ends_with_done(&first));
        // Any further events after the terminal chunk emit nothing.
        let after = converse_event_to_sse(
            "contentBlockDelta",
            &json!({ "delta": { "text": "late" } }),
            "m",
            &mut state,
        );
        assert!(after.is_empty());
    }

    #[test]
    fn stream_exception_emits_error_chunk_then_done() {
        let mut state = StreamXlateState::new();
        let out = converse_event_to_sse(
            "throttlingException",
            &json!({ "message": "Too many requests" }),
            "m",
            &mut state,
        );
        assert!(ends_with_done(&out));
        let err = &sse_values(&out)[0];
        assert_eq!(err["error"]["message"], json!("Too many requests"));
        assert_eq!(err["error"]["type"], json!("throttlingException"));
        // Stream is terminated.
        assert!(state.done);
    }

    #[test]
    fn stream_message_start_defaults_role_when_absent() {
        let mut state = StreamXlateState::new();
        let out = converse_event_to_sse("messageStart", &json!({}), "m", &mut state);
        let v = &sse_values(&out)[0];
        assert_eq!(v["choices"][0]["delta"]["role"], json!("assistant"));
    }
}
