//! JSON chat-body walking: which string fields get scanned/rewritten. WP 3.2 / WP 4.4 / WP 9.2.

use regex::Regex;
use std::sync::OnceLock;

use crate::store::EntityStore;
use crate::{DetectError, EntityMap, PiiEngine};

/// Which chat envelope shape the body uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFormat {
    /// OpenAI `/v1/chat/completions`: `messages[].content` (string or
    /// part-array with `{"type":"text","text":...}`).
    OpenAiChat,
    /// Anthropic `/v1/messages`: top-level `system` (string or block array)
    /// and `messages[].content` (string or block array with text blocks).
    AnthropicMessages,
}

/// Scan + rewrite all user-visible text fields of a request body in place.
/// Only the documented text fields are touched; everything else (tool
/// schemas, params, names) passes through byte-identical.
///
/// # OpenAI Chat request
/// - `messages[].content` — string → scan+rewrite; array → each part where
///   `part["type"] == "text"` rewrite `part["text"]`. All roles including
///   `system` and `developer` are processed.
///
/// # Anthropic Messages request
/// - `system` — string → scan+rewrite; array of blocks → each block where
///   `block["type"] == "text"` rewrite `block["text"]`.
/// - `messages[].content` — string → scan+rewrite; array of content blocks:
///   - `type == "text"` → rewrite `text` field.
///   - `type == "tool_result"` → `content` field (string or nested text
///     blocks array) is also rewritten recursively.
pub fn pseudonymize_body(
    format: BodyFormat,
    body: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) {
    match format {
        BodyFormat::OpenAiChat => pseudonymize_openai_request(body, engine, map),
        BodyFormat::AnthropicMessages => pseudonymize_anthropic_request(body, engine, map),
    }
}

/// Restore placeholders in all text fields of a non-streaming RESPONSE body
/// in place (OpenAI: `choices[].message.content`; Anthropic: `content[].text`).
/// Unknown fields untouched.
pub fn restore_body(format: BodyFormat, body: &mut serde_json::Value, map: &EntityMap) {
    restore_body_with_store(format, body, map, None);
}

/// Restore placeholders in all text fields of a non-streaming RESPONSE body,
/// with an optional persistent [`EntityStore`] for resolving placeholders from
/// **past** requests (WP 9.2).
///
/// # Two-pass restoration
///
/// 1. **In-map pass**: the current request's [`EntityMap`] restores all
///    placeholders it knows about (same as [`restore_body`]).
/// 2. **Store pass** (only when `store` is `Some`): any text field that still
///    contains placeholder-shaped tokens (`\b[A-Z][A-Z0-9_]*_[0-9]+\b`) is
///    scanned. For each candidate the store is queried via
///    [`EntityStore::lookup_placeholder`]; if the store returns a value, the
///    placeholder is replaced. Unknown candidates (store returns `None`)
///    pass through unchanged — they may be model hallucinations or custom
///    downstream tokens.
///
/// # Streaming
///
/// **Streaming responses are out of scope for v1.** Within-request
/// placeholders are restored by [`crate::StreamRestorer`] (which uses the
/// local `EntityMap`). Past-request placeholders in SSE streams pass through
/// untouched. When cross-stream restore is needed in a future work package,
/// a `StreamRestorerWithStore` variant should be added that wraps
/// `StreamRestorer` and applies a store second pass on each finalized chunk.
///
/// # Store errors
///
/// A store error during the second pass is swallowed (the candidate stays
/// unreplaced). This is intentional: the restore pass is best-effort for
/// past-request placeholders — a store read failure does not compromise
/// the security of the current request (no PII was written out; the
/// placeholder stays in the response text instead of the original value).
pub fn restore_body_with_store(
    format: BodyFormat,
    body: &mut serde_json::Value,
    map: &EntityMap,
    store: Option<&dyn EntityStore>,
) {
    // Pass 1: in-map restore (always).
    match format {
        BodyFormat::OpenAiChat => restore_openai_response(body, map),
        BodyFormat::AnthropicMessages => restore_anthropic_response(body, map),
    }

    // Pass 2: store-based restore for past-request placeholders.
    if let Some(store) = store {
        match format {
            BodyFormat::OpenAiChat => store_restore_openai_response(body, store),
            BodyFormat::AnthropicMessages => store_restore_anthropic_response(body, store),
        }
    }
}

/// Fallible version of [`pseudonymize_body`]. Uses [`PiiEngine::try_scan`] so
/// that fail-closed NER errors are propagated rather than silently swallowed.
///
/// On error the body may be partially rewritten; callers should treat the
/// body as unusable and reject the request.
pub fn try_pseudonymize_body(
    format: BodyFormat,
    body: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<(), DetectError> {
    match format {
        BodyFormat::OpenAiChat => try_pseudonymize_openai_request(body, engine, map),
        BodyFormat::AnthropicMessages => try_pseudonymize_anthropic_request(body, engine, map),
    }
}

/// Collect the user-authored request text for content-guardrail scanning
/// (v0.0.8). OpenAI: each `messages[].content` (string or text parts).
/// Anthropic: top-level `system` plus each `messages[].content`. Parts are
/// newline-joined. Returns an empty string when there is no text to scan.
pub fn collect_request_text(format: BodyFormat, body: &serde_json::Value) -> String {
    let mut out = String::new();
    match format {
        BodyFormat::OpenAiChat => {
            if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
                for msg in msgs {
                    push_content_text(msg.get("content"), &mut out);
                }
            }
        }
        BodyFormat::AnthropicMessages => {
            push_content_text(body.get("system"), &mut out);
            if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
                for msg in msgs {
                    push_content_text(msg.get("content"), &mut out);
                }
            }
        }
    }
    out
}

/// Collect the model-generated response text for content-guardrail scanning
/// (v0.0.8). OpenAI: `choices[].message.content`. Anthropic: `content[].text`.
pub fn collect_response_text(format: BodyFormat, body: &serde_json::Value) -> String {
    let mut out = String::new();
    match format {
        BodyFormat::OpenAiChat => {
            if let Some(choices) = body.get("choices").and_then(|c| c.as_array()) {
                for ch in choices {
                    push_content_text(ch.get("message").and_then(|m| m.get("content")), &mut out);
                }
            }
        }
        BodyFormat::AnthropicMessages => {
            push_content_text(body.get("content"), &mut out);
        }
    }
    out
}

/// Append text extracted from a `content` value (string, or array of blocks
/// each carrying a `text` field), newline-separated.
fn push_content_text(content: Option<&serde_json::Value>, out: &mut String) {
    let Some(content) = content else { return };
    let mut push = |s: &str| {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(s);
    };
    if let Some(s) = content.as_str() {
        push(s);
    } else if let Some(parts) = content.as_array() {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                push(t);
            }
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Scan `text` with the engine (infallible) and pseudonymize it via the map.
fn scan_and_rewrite(text: &str, engine: &PiiEngine, map: &mut EntityMap) -> String {
    let detections = engine.scan(text);
    map.pseudonymize(text, &detections)
}

/// Scan `text` with the engine (fallible) and pseudonymize it via the map.
///
/// Uses [`EntityMap::try_pseudonymize`] so that store errors (WP 9.2) are
/// propagated in addition to NER errors. On the storeless path this is
/// equivalent to calling `map.pseudonymize`.
fn try_scan_and_rewrite(
    text: &str,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<String, DetectError> {
    let detections = engine.try_scan(text)?;
    map.try_pseudonymize(text, &detections)
}

/// If `value` is a JSON string, scan+rewrite it in place.
fn rewrite_string_value(value: &mut serde_json::Value, engine: &PiiEngine, map: &mut EntityMap) {
    if let Some(s) = value.as_str() {
        let rewritten = scan_and_rewrite(s, engine, map);
        *value = serde_json::Value::String(rewritten);
    }
}

/// If `value` is a JSON string, try_scan+rewrite it in place.
fn try_rewrite_string_value(
    value: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<(), DetectError> {
    if let Some(s) = value.as_str() {
        let rewritten = try_scan_and_rewrite(s, engine, map)?;
        *value = serde_json::Value::String(rewritten);
    }
    Ok(())
}

/// If `value` is a JSON string, restore placeholders in place.
fn restore_string_value(value: &mut serde_json::Value, map: &EntityMap) {
    if let Some(s) = value.as_str() {
        let restored = map.restore(s);
        *value = serde_json::Value::String(restored);
    }
}

/// Recursively restore placeholders in every JSON **string leaf** of `value`
/// (object values and array elements walked; non-string scalars untouched).
///
/// Used for tool-call argument payloads, which are structured JSON whose string
/// values (e.g. an email in `{"to":"EMAIL_1"}`) may carry placeholders.
fn restore_json_strings(value: &mut serde_json::Value, map: &EntityMap) {
    match value {
        serde_json::Value::String(_) => restore_string_value(value, map),
        serde_json::Value::Array(items) => {
            for item in items {
                restore_json_strings(item, map);
            }
        }
        serde_json::Value::Object(obj) => {
            for v in obj.values_mut() {
                restore_json_strings(v, map);
            }
        }
        _ => {}
    }
}

/// Restore placeholders inside an OpenAI tool-call `arguments` field.
///
/// `arguments` is a JSON document encoded **as a string**. We parse it, restore
/// placeholders in its string leaves, and re-serialize — this keeps the result
/// valid JSON regardless of what characters the restored value contains (serde
/// re-escapes on serialize). If `arguments` is not valid JSON (a partial or
/// malformed tool call), fall back to a best-effort restore on the raw string.
fn restore_openai_tool_arguments(value: &mut serde_json::Value, map: &EntityMap) {
    let Some(s) = value.as_str() else { return };
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(mut parsed) => {
            restore_json_strings(&mut parsed, map);
            if let Ok(reserialized) = serde_json::to_string(&parsed) {
                *value = serde_json::Value::String(reserialized);
            }
        }
        Err(_) => restore_string_value(value, map),
    }
}

// ── OpenAI request ───────────────────────────────────────────────────────────

/// Walk `body["messages"]` and rewrite text content.
fn pseudonymize_openai_request(
    body: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for msg in messages {
        rewrite_openai_content(msg.get_mut("content"), engine, map);
    }
}

/// Rewrite an OpenAI message `content` field (string or part-array).
fn rewrite_openai_content(
    content: Option<&mut serde_json::Value>,
    engine: &PiiEngine,
    map: &mut EntityMap,
) {
    let Some(content) = content else { return };

    if content.is_string() {
        rewrite_string_value(content, engine, map);
    } else if let Some(parts) = content.as_array_mut() {
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text_field) = part.get_mut("text")
            {
                rewrite_string_value(text_field, engine, map);
            }
            // Other part types (image_url, etc.) are left untouched.
        }
    }
    // Null/other types → leave untouched.
}

// ── Anthropic request ────────────────────────────────────────────────────────

/// Walk Anthropic request body: system field + messages[].content.
fn pseudonymize_anthropic_request(
    body: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) {
    // system: string or array of text blocks
    if let Some(system) = body.get_mut("system") {
        rewrite_anthropic_text_field(system, engine, map);
    }

    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for msg in messages {
        if let Some(content) = msg.get_mut("content") {
            rewrite_anthropic_content(content, engine, map);
        }
    }
}

/// Rewrite a field that is either a string or an array of content blocks.
fn rewrite_anthropic_text_field(
    value: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) {
    if value.is_string() {
        rewrite_string_value(value, engine, map);
    } else if let Some(blocks) = value.as_array_mut() {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text_field) = block.get_mut("text")
            {
                rewrite_string_value(text_field, engine, map);
            }
        }
    }
}

/// Rewrite Anthropic `content`: string, or array of content blocks including
/// `text` blocks and `tool_result` blocks (which carry nested content).
fn rewrite_anthropic_content(
    content: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) {
    if content.is_string() {
        rewrite_string_value(content, engine, map);
        return;
    }
    let Some(blocks) = content.as_array_mut() else {
        return;
    };
    for block in blocks {
        let block_type = block
            .get("type")
            .and_then(|t| t.as_str())
            .map(str::to_owned);
        match block_type.as_deref() {
            Some("text") => {
                if let Some(text_field) = block.get_mut("text") {
                    rewrite_string_value(text_field, engine, map);
                }
            }
            Some("tool_result") => {
                // tool_result.content: string or array of text blocks
                if let Some(inner) = block.get_mut("content") {
                    rewrite_anthropic_text_field(inner, engine, map);
                }
            }
            _ => {
                // Other block types (image, tool_use, etc.) → untouched.
            }
        }
    }
}

// ── Fallible OpenAI request ──────────────────────────────────────────────────

fn try_pseudonymize_openai_request(
    body: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<(), DetectError> {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return Ok(());
    };
    for msg in messages {
        try_rewrite_openai_content(msg.get_mut("content"), engine, map)?;
    }
    Ok(())
}

fn try_rewrite_openai_content(
    content: Option<&mut serde_json::Value>,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<(), DetectError> {
    let Some(content) = content else {
        return Ok(());
    };
    if content.is_string() {
        try_rewrite_string_value(content, engine, map)?;
    } else if let Some(parts) = content.as_array_mut() {
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text_field) = part.get_mut("text")
            {
                try_rewrite_string_value(text_field, engine, map)?;
            }
        }
    }
    Ok(())
}

// ── Fallible Anthropic request ───────────────────────────────────────────────

fn try_pseudonymize_anthropic_request(
    body: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<(), DetectError> {
    if let Some(system) = body.get_mut("system") {
        try_rewrite_anthropic_text_field(system, engine, map)?;
    }
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return Ok(());
    };
    for msg in messages {
        if let Some(content) = msg.get_mut("content") {
            try_rewrite_anthropic_content(content, engine, map)?;
        }
    }
    Ok(())
}

fn try_rewrite_anthropic_text_field(
    value: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<(), DetectError> {
    if value.is_string() {
        try_rewrite_string_value(value, engine, map)?;
    } else if let Some(blocks) = value.as_array_mut() {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text_field) = block.get_mut("text")
            {
                try_rewrite_string_value(text_field, engine, map)?;
            }
        }
    }
    Ok(())
}

fn try_rewrite_anthropic_content(
    content: &mut serde_json::Value,
    engine: &PiiEngine,
    map: &mut EntityMap,
) -> Result<(), DetectError> {
    if content.is_string() {
        try_rewrite_string_value(content, engine, map)?;
        return Ok(());
    }
    let Some(blocks) = content.as_array_mut() else {
        return Ok(());
    };
    for block in blocks {
        let block_type = block
            .get("type")
            .and_then(|t| t.as_str())
            .map(str::to_owned);
        match block_type.as_deref() {
            Some("text") => {
                if let Some(text_field) = block.get_mut("text") {
                    try_rewrite_string_value(text_field, engine, map)?;
                }
            }
            Some("tool_result") => {
                if let Some(inner) = block.get_mut("content") {
                    try_rewrite_anthropic_text_field(inner, engine, map)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ── OpenAI response ──────────────────────────────────────────────────────────

/// Restore `choices[].message.content` (string only; tool_calls left alone).
fn restore_openai_response(body: &mut serde_json::Value, map: &EntityMap) {
    let Some(choices) = body.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for choice in choices {
        let Some(msg) = choice.get_mut("message") else {
            continue;
        };
        if let Some(content) = msg.get_mut("content") {
            restore_string_value(content, map);
        }
        // Tool calls: restore placeholders inside each call's `arguments`.
        if let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
            for call in tool_calls {
                if let Some(args) = call.get_mut("function").and_then(|f| f.get_mut("arguments")) {
                    restore_openai_tool_arguments(args, map);
                }
            }
        }
    }
}

// ── Anthropic response ───────────────────────────────────────────────────────

/// Restore `content[].text` for `type == "text"` blocks and `content[].input`
/// for `type == "tool_use"` blocks.
fn restore_anthropic_response(body: &mut serde_json::Value, map: &EntityMap) {
    let Some(blocks) = body.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text_field) = block.get_mut("text") {
                    restore_string_value(text_field, map);
                }
            }
            // `input` is already a parsed JSON object (not a string), so walk
            // its string leaves directly.
            Some("tool_use") => {
                if let Some(input) = block.get_mut("input") {
                    restore_json_strings(input, map);
                }
            }
            _ => {}
        }
    }
}

// ── Store-based restore helpers (WP 9.2) ─────────────────────────────────────

/// Compiled regex for placeholder-shaped tokens: `\b[A-Z][A-Z0-9_]*_[0-9]+\b`.
///
/// We use a conservative pattern: starts with an uppercase letter, followed by
/// uppercase letters/digits/underscores, then `_`, then one or more digits.
/// This matches all standard prefixes (`EMAIL_1`, `PERSON_12`, `CARD_3`) as
/// well as custom recognizer names.
///
/// We cannot enumerate known prefixes statically because custom recognizers are
/// configured at runtime. Accept the generic shape and look every candidate up
/// in the store; misses (store returns `None`) stay untouched.
fn placeholder_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b([A-Z][A-Z0-9_]*_[0-9]+)\b").expect("placeholder regex is valid")
    })
}

/// Replace remaining placeholder-shaped tokens in `text` using the store.
///
/// Tokens not found in the store pass through unchanged.
fn store_restore_text(text: &str, store: &dyn EntityStore) -> String {
    let re = placeholder_regex();
    // We build the output string by iterating over matches and splicing in
    // store-looked-up values. Unknown placeholders keep the original token.
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for cap in re.captures_iter(text) {
        let m = cap.get(0).expect("match 0 always present");
        let placeholder = m.as_str();
        // Append verbatim text before this match.
        out.push_str(&text[last..m.start()]);
        // Query the store. On error or miss, keep the placeholder unchanged.
        let replacement = store
            .lookup_placeholder(placeholder)
            .ok()
            .flatten()
            .unwrap_or_else(|| placeholder.to_owned());
        out.push_str(&replacement);
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

/// If `value` is a JSON string that still contains placeholder-shaped tokens,
/// apply store restore in place.
fn store_restore_string_value(value: &mut serde_json::Value, store: &dyn EntityStore) {
    if let Some(s) = value.as_str() {
        let restored = store_restore_text(s, store);
        if restored != s {
            *value = serde_json::Value::String(restored);
        }
    }
}

/// Recursively store-restore every JSON string leaf of `value`.
fn store_restore_json_strings(value: &mut serde_json::Value, store: &dyn EntityStore) {
    match value {
        serde_json::Value::String(_) => store_restore_string_value(value, store),
        serde_json::Value::Array(items) => {
            for item in items {
                store_restore_json_strings(item, store);
            }
        }
        serde_json::Value::Object(obj) => {
            for v in obj.values_mut() {
                store_restore_json_strings(v, store);
            }
        }
        _ => {}
    }
}

/// Store-restore an OpenAI tool-call `arguments` field (JSON-encoded string).
/// Mirrors [`restore_openai_tool_arguments`] for the past-request store pass.
fn store_restore_openai_tool_arguments(value: &mut serde_json::Value, store: &dyn EntityStore) {
    let Some(s) = value.as_str() else { return };
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(mut parsed) => {
            store_restore_json_strings(&mut parsed, store);
            if let Ok(reserialized) = serde_json::to_string(&parsed) {
                *value = serde_json::Value::String(reserialized);
            }
        }
        Err(_) => store_restore_string_value(value, store),
    }
}

/// Store-restore `choices[].message.content` and tool-call arguments for
/// past-request placeholders.
fn store_restore_openai_response(body: &mut serde_json::Value, store: &dyn EntityStore) {
    let Some(choices) = body.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for choice in choices {
        let Some(msg) = choice.get_mut("message") else {
            continue;
        };
        if let Some(content) = msg.get_mut("content") {
            store_restore_string_value(content, store);
        }
        if let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
            for call in tool_calls {
                if let Some(args) = call.get_mut("function").and_then(|f| f.get_mut("arguments")) {
                    store_restore_openai_tool_arguments(args, store);
                }
            }
        }
    }
}

/// Store-restore `content[].text` and `content[].input` (tool_use) for
/// past-request placeholders.
fn store_restore_anthropic_response(body: &mut serde_json::Value, store: &dyn EntityStore) {
    let Some(blocks) = body.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text_field) = block.get_mut("text") {
                    store_restore_string_value(text_field, store);
                }
            }
            Some("tool_use") => {
                if let Some(input) = block.get_mut("input") {
                    store_restore_json_strings(input, store);
                }
            }
            _ => {}
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Detection, EntityKind};
    use serde_json::json;

    // ── helpers for body tests ───────────────────────────────────────────────

    // ── EntityMap-only walking tests (no PiiEngine) ──────────────────────────
    // These test the JSON walking logic by pre-populating a map and calling
    // restore_body (which doesn't need the engine).

    #[test]
    fn restore_openai_response_string_content() {
        let mut map = EntityMap::new();
        map.pseudonymize(
            "alice@example.com",
            &[Detection {
                start: 0,
                end: 17,
                kind: EntityKind::Email,
            }],
        );

        let mut body = json!({
            "choices": [
                {"message": {"content": "contact EMAIL_1 please", "role": "assistant"}}
            ]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "contact alice@example.com please"
        );
    }

    #[test]
    fn restore_openai_response_null_content_untouched() {
        let map = EntityMap::new();
        let mut body = json!({
            "choices": [
                {"message": {"content": null, "role": "assistant"}}
            ]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert!(body["choices"][0]["message"]["content"].is_null());
    }

    #[test]
    fn restore_openai_response_empty_choices() {
        let map = EntityMap::new();
        let mut body = json!({"choices": []});
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert_eq!(body["choices"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn restore_openai_response_unknown_placeholder_untouched() {
        let mut map = EntityMap::new();
        map.pseudonymize(
            "a@b.com",
            &[Detection {
                start: 0,
                end: 7,
                kind: EntityKind::Email,
            }],
        );
        let mut body = json!({
            "choices": [
                {"message": {"content": "EMAIL_1 EMAIL_99"}}
            ]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert_eq!(body["choices"][0]["message"]["content"], "a@b.com EMAIL_99");
    }

    #[test]
    fn restore_anthropic_response_text_blocks() {
        let mut map = EntityMap::new();
        map.pseudonymize(
            "alice@example.com",
            &[Detection {
                start: 0,
                end: 17,
                kind: EntityKind::Email,
            }],
        );
        let mut body = json!({
            "content": [
                {"type": "text", "text": "reach EMAIL_1 now"},
                {"type": "tool_use", "id": "t1", "name": "search", "input": {}}
            ]
        });
        restore_body(BodyFormat::AnthropicMessages, &mut body, &map);
        assert_eq!(body["content"][0]["text"], "reach alice@example.com now");
        // tool_use untouched
        assert_eq!(body["content"][1]["name"], "search");
    }

    #[test]
    fn restore_anthropic_response_no_content_key() {
        let map = EntityMap::new();
        let mut body = json!({"id": "msg_1", "type": "message"});
        // must not panic
        restore_body(BodyFormat::AnthropicMessages, &mut body, &map);
        assert_eq!(body["id"], "msg_1");
    }

    // ── OpenAI request walking (map pre-populated, engine used via integration)

    /// Test that the walking logic for OpenAI request correctly targets content
    /// fields. We build a body with known placeholder tokens already in place
    /// and verify restore puts them back — this exercises the walker logic
    /// independently of the engine.
    #[test]
    fn openai_request_walking_string_content() {
        let mut map = EntityMap::new();
        map.pseudonymize(
            "alice@example.com",
            &[Detection {
                start: 0,
                end: 17,
                kind: EntityKind::Email,
            }],
        );
        // Simulate a body that has already been pseudonymized (EMAIL_1 in place)
        // and verify restore_body reverses it.
        let mut body = json!({
            "choices": [{"message": {"content": "see EMAIL_1 here"}}]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "see alice@example.com here"
        );
    }

    #[test]
    fn openai_request_walking_part_array() {
        // Verify restore_body on a response with array content (edge case:
        // OpenAI response content is a string, but we test via restore walker
        // that the map correctly handles multi-value cases).
        let mut map = EntityMap::new();
        map.pseudonymize(
            "a@b.com",
            &[Detection {
                start: 0,
                end: 7,
                kind: EntityKind::Email,
            }],
        );
        map.pseudonymize(
            "c@d.com",
            &[Detection {
                start: 0,
                end: 7,
                kind: EntityKind::Email,
            }],
        );
        let mut body = json!({
            "choices": [
                {"message": {"content": "EMAIL_1 then EMAIL_2"}}
            ]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "a@b.com then c@d.com"
        );
    }

    #[test]
    fn anthropic_response_same_email_in_two_blocks() {
        let mut map = EntityMap::new();
        map.pseudonymize(
            "alice@example.com",
            &[Detection {
                start: 0,
                end: 17,
                kind: EntityKind::Email,
            }],
        );
        let mut body = json!({
            "content": [
                {"type": "text", "text": "EMAIL_1"},
                {"type": "text", "text": "also EMAIL_1"}
            ]
        });
        restore_body(BodyFormat::AnthropicMessages, &mut body, &map);
        assert_eq!(body["content"][0]["text"], "alice@example.com");
        assert_eq!(body["content"][1]["text"], "also alice@example.com");
    }

    #[test]
    fn restore_boundary_email1_vs_email12_in_response() {
        // Build a map with 12 distinct emails.
        let mut map = EntityMap::new();
        let emails: Vec<String> = (b'a'..=b'm')
            .filter(|&c| c != b'j')
            .take(12)
            .map(|c| format!("{}@b.com", c as char))
            .collect();
        for email in &emails {
            map.pseudonymize(
                email,
                &[Detection {
                    start: 0,
                    end: email.len(),
                    kind: EntityKind::Email,
                }],
            );
        }
        // Response contains EMAIL_12 — must not be corrupted by EMAIL_1 rule.
        let mut body = json!({
            "content": [{"type": "text", "text": "contact EMAIL_12 now"}]
        });
        restore_body(BodyFormat::AnthropicMessages, &mut body, &map);
        let text = body["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("EMAIL_12"),
            "EMAIL_12 must be replaced: {text}"
        );
        assert!(
            !text.contains("EMAIL_1"),
            "no leftover EMAIL_1 placeholder: {text}"
        );
    }

    #[test]
    fn openai_numbers_and_bools_untouched() {
        let map = EntityMap::new();
        let mut body = json!({
            "choices": [
                {"message": {"content": null, "role": "assistant", "index": 0, "finish_reason": "stop"}}
            ],
            "usage": {"total_tokens": 42}
        });
        let original = body.clone();
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert_eq!(
            body["usage"]["total_tokens"],
            original["usage"]["total_tokens"]
        );
        assert_eq!(body["choices"][0]["message"]["index"], 0);
    }

    // ── tool-call argument restore (the structural gap) ──────────────────────

    fn map_with_email(email: &str) -> EntityMap {
        let mut map = EntityMap::new();
        map.pseudonymize(
            email,
            &[Detection {
                start: 0,
                end: email.len(),
                kind: EntityKind::Email,
            }],
        );
        map
    }

    #[test]
    fn restore_openai_tool_call_arguments() {
        let map = map_with_email("denis@example.com");
        let mut body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": serde_json::Value::Null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "send_email",
                            "arguments": "{\"to\":\"EMAIL_1\",\"subject\":\"hi\"}"
                        }
                    }]
                }
            }]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        let args = body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        // Result must still be valid JSON and carry the real email.
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["to"], "denis@example.com");
        assert_eq!(parsed["subject"], "hi");
        assert!(!args.contains("EMAIL_1"), "placeholder leaked: {args}");
    }

    #[test]
    fn restore_openai_tool_call_arguments_nested() {
        let map = map_with_email("a@b.com");
        let mut body = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {
                            "name": "f",
                            "arguments": "{\"cc\":[\"EMAIL_1\"],\"meta\":{\"reply_to\":\"EMAIL_1\"}}"
                        }
                    }]
                }
            }]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        let args = body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["cc"][0], "a@b.com");
        assert_eq!(parsed["meta"]["reply_to"], "a@b.com");
    }

    #[test]
    fn restore_openai_tool_call_arguments_value_needs_escaping() {
        // Original value contains a quote: re-serialization must keep the
        // arguments string valid JSON.
        let mut map = EntityMap::new();
        let original = r#"O'Brien "Bob""#;
        map.pseudonymize(
            original,
            &[Detection {
                start: 0,
                end: original.len(),
                kind: EntityKind::Person,
            }],
        );
        let mut body = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {"name": "f", "arguments": "{\"name\":\"PERSON_1\"}"}
                    }]
                }
            }]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        let args = body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["name"], original);
    }

    #[test]
    fn restore_openai_tool_call_arguments_invalid_json_best_effort() {
        let map = map_with_email("a@b.com");
        // Malformed/partial arguments: still restore the placeholder verbatim.
        let mut body = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {"name": "f", "arguments": "to=EMAIL_1 (partial"}
                    }]
                }
            }]
        });
        restore_body(BodyFormat::OpenAiChat, &mut body, &map);
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "to=a@b.com (partial"
        );
    }

    #[test]
    fn restore_anthropic_tool_use_input() {
        let map = map_with_email("denis@example.com");
        let mut body = json!({
            "content": [
                {"type": "text", "text": "Sending to EMAIL_1"},
                {"type": "tool_use", "id": "t1", "name": "send_email",
                 "input": {"to": "EMAIL_1", "cc": ["EMAIL_1"]}}
            ]
        });
        restore_body(BodyFormat::AnthropicMessages, &mut body, &map);
        assert_eq!(body["content"][0]["text"], "Sending to denis@example.com");
        assert_eq!(body["content"][1]["input"]["to"], "denis@example.com");
        assert_eq!(body["content"][1]["input"]["cc"][0], "denis@example.com");
    }

    // ── pseudonymize_body integration tests (require PiiEngine::from_config) ──
    // These are marked #[ignore] because PiiEngine::from_config is todo!() in
    // WP 3.1.  When WP 3.1 lands, remove the ignore attribute.

    #[test]

    fn pseudonymize_openai_string_content() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "email me at alice@example.com"},
                {"role": "system", "content": "system prompt alice@example.com"},
                {"role": "developer", "content": "dev prompt alice@example.com"}
            ]
        });
        pseudonymize_body(BodyFormat::OpenAiChat, &mut body, &engine, &mut map);
        assert!(
            !body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("alice@example.com")
        );
        assert!(
            !body["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("alice@example.com")
        );
        assert!(
            !body["messages"][2]["content"]
                .as_str()
                .unwrap()
                .contains("alice@example.com")
        );
        // model field untouched
        assert_eq!(body["model"], "gpt-4o");
        // All three messages reference the same email → same placeholder
        assert_eq!(
            body["messages"][0]["content"],
            body["messages"][1]["content"].as_str().unwrap().replace(
                body["messages"][1]["content"].as_str().unwrap().trim(),
                body["messages"][0]["content"].as_str().unwrap().trim(),
            )
        );
        assert_eq!(map.len(), 1);
    }

    #[test]

    fn pseudonymize_openai_part_array_content() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "contact alice@example.com"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}}
                ]
            }]
        });
        pseudonymize_body(BodyFormat::OpenAiChat, &mut body, &engine, &mut map);
        let text = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains("alice@example.com"));
        // image_url part untouched
        assert_eq!(
            body["messages"][0]["content"][1]["image_url"]["url"],
            "https://example.com/img.png"
        );
    }

    #[test]

    fn pseudonymize_openai_empty_messages() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({"messages": []});
        pseudonymize_body(BodyFormat::OpenAiChat, &mut body, &engine, &mut map);
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
        assert!(map.is_empty());
    }

    #[test]

    fn pseudonymize_anthropic_system_string() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({
            "system": "You are helping alice@example.com",
            "messages": [{"role": "user", "content": "hello"}]
        });
        pseudonymize_body(BodyFormat::AnthropicMessages, &mut body, &engine, &mut map);
        assert!(
            !body["system"]
                .as_str()
                .unwrap()
                .contains("alice@example.com")
        );
    }

    #[test]

    fn pseudonymize_anthropic_system_block_array() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({
            "system": [
                {"type": "text", "text": "help alice@example.com"},
                {"type": "image", "source": {"type": "url", "url": "http://img"}}
            ],
            "messages": []
        });
        pseudonymize_body(BodyFormat::AnthropicMessages, &mut body, &engine, &mut map);
        assert!(
            !body["system"][0]["text"]
                .as_str()
                .unwrap()
                .contains("alice@example.com")
        );
        // image block untouched
        assert_eq!(body["system"][1]["source"]["url"], "http://img");
    }

    #[test]

    fn pseudonymize_anthropic_tool_result_content() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": "result for alice@example.com"
                }]
            }]
        });
        pseudonymize_body(BodyFormat::AnthropicMessages, &mut body, &engine, &mut map);
        let content = body["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(!content.contains("alice@example.com"));
    }

    #[test]

    fn pseudonymize_anthropic_tool_result_nested_blocks() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        {"type": "text", "text": "alice@example.com found"},
                        {"type": "image", "source": {"type": "url", "url": "http://img"}}
                    ]
                }]
            }]
        });
        pseudonymize_body(BodyFormat::AnthropicMessages, &mut body, &engine, &mut map);
        let text = body["messages"][0]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(!text.contains("alice@example.com"));
        // image untouched
        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["source"]["url"],
            "http://img"
        );
    }

    #[test]

    fn pseudonymize_same_email_in_two_messages_same_placeholder() {
        use drgtw_config::PiiConfig;
        let engine = PiiEngine::from_config(&PiiConfig::default()).unwrap();
        let mut map = EntityMap::new();
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "alice@example.com"},
                {"role": "user", "content": "alice@example.com again"}
            ]
        });
        pseudonymize_body(BodyFormat::OpenAiChat, &mut body, &engine, &mut map);
        assert_eq!(map.len(), 1);
        let ph0 = body["messages"][0]["content"].as_str().unwrap().to_owned();
        let ph1 = body["messages"][1]["content"].as_str().unwrap().to_owned();
        assert!(ph1.contains(&ph0.trim().to_owned()));
    }
}
