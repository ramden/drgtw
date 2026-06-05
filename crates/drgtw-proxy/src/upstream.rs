//! Upstream URL construction helpers.

/// Build the upstream URL for `POST /chat/completions`.
///
/// `base_url` already includes `/v1` for OpenAI-style connections
/// (e.g. `https://api.openai.com/v1`). We trim any trailing slash and
/// append `/chat/completions`.
pub fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

/// Build the upstream URL for `POST /embeddings` (OpenAI convention). WP 9.3.
///
/// `base_url` already includes `/v1` for OpenAI-style connections
/// (e.g. `https://api.openai.com/v1`). We trim any trailing slash and append
/// `/embeddings`.
pub fn embeddings_url(base_url: &str) -> String {
    format!("{}/embeddings", base_url.trim_end_matches('/'))
}

/// Build the upstream URL for `POST /v1/messages` (Anthropic convention).
///
/// Anthropic SDK appends `/v1/messages` to the base URL, so `base_url` does
/// NOT include `/v1` (e.g. `https://api.anthropic.com`). We trim any trailing
/// slash and append `/v1/messages`.
pub fn messages_url(base_url: &str) -> String {
    format!("{}/v1/messages", base_url.trim_end_matches('/'))
}

/// Build the upstream URL for native Bedrock `POST /model/{model}/invoke`.
///
/// `base_url` has NO `/v1` (e.g.
/// `https://bedrock-runtime.eu-central-1.amazonaws.com`). We trim any trailing
/// slash and append `/model/{model}/invoke`.
///
/// Bedrock model ids contain `.` and `-` (URL-safe in a path segment) and a
/// `:` revision suffix (`anthropic.claude-3-5-sonnet-20241022-v2:0`). The `:`
/// is percent-encoded to `%3A`; dots and dashes are preserved verbatim.
pub fn bedrock_invoke_url(base_url: &str, model: &str) -> String {
    format!(
        "{}/model/{}/invoke",
        base_url.trim_end_matches('/'),
        encode_model_id(model)
    )
}

/// Build the upstream URL for Bedrock `POST /model/{model}/converse`.
///
/// `base_url` has NO `/v1` (e.g.
/// `https://bedrock-runtime.eu-central-1.amazonaws.com`). We trim any trailing
/// slash and append `/model/{model}/converse`. The model id is percent-encoded
/// the same way as the native InvokeModel path (only `:` → `%3A`).
pub fn converse_url(base_url: &str, model: &str) -> String {
    format!(
        "{}/model/{}/converse",
        base_url.trim_end_matches('/'),
        encode_model_id(model)
    )
}

/// Build the upstream URL for Bedrock `POST /model/{model}/converse-stream`.
///
/// Identical to [`converse_url`] but appends `/converse-stream`; used when the
/// client requested `stream: true`.
pub fn converse_stream_url(base_url: &str, model: &str) -> String {
    format!(
        "{}/model/{}/converse-stream",
        base_url.trim_end_matches('/'),
        encode_model_id(model)
    )
}

/// Percent-encode a Bedrock model id for use as a single URL path segment.
///
/// Only `:` is encoded (to `%3A`); every other character — including `.`, `-`,
/// and alphanumerics — passes through unchanged, matching how the AWS SDK
/// path-encodes inference-profile / foundation-model ids.
fn encode_model_id(model: &str) -> String {
    model.replace(':', "%3A")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_without_double_slash() {
        assert_eq!(
            chat_completions_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn trims_trailing_slash() {
        assert_eq!(
            chat_completions_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn trims_multiple_trailing_slashes() {
        assert_eq!(
            chat_completions_url("https://api.openai.com/v1///"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    // Embeddings URL helper
    #[test]
    fn embeddings_url_joins_without_double_slash() {
        assert_eq!(
            embeddings_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/embeddings"
        );
    }

    #[test]
    fn embeddings_url_trims_trailing_slash() {
        assert_eq!(
            embeddings_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/embeddings"
        );
    }

    // Anthropic URL helpers
    #[test]
    fn messages_url_no_v1_in_base() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn messages_url_trims_trailing_slash() {
        assert_eq!(
            messages_url("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn messages_url_trims_multiple_trailing_slashes() {
        assert_eq!(
            messages_url("https://api.anthropic.com///"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    // Bedrock native InvokeModel URL helpers
    #[test]
    fn bedrock_invoke_url_basic() {
        assert_eq!(
            bedrock_invoke_url(
                "https://bedrock-runtime.eu-central-1.amazonaws.com",
                "eu.anthropic.claude-sonnet-4-6"
            ),
            "https://bedrock-runtime.eu-central-1.amazonaws.com/model/eu.anthropic.claude-sonnet-4-6/invoke"
        );
    }

    #[test]
    fn bedrock_invoke_url_trims_trailing_slash() {
        assert_eq!(
            bedrock_invoke_url(
                "https://bedrock-runtime.eu-central-1.amazonaws.com///",
                "openai.gpt-oss-120b"
            ),
            "https://bedrock-runtime.eu-central-1.amazonaws.com/model/openai.gpt-oss-120b/invoke"
        );
    }

    #[test]
    fn bedrock_invoke_url_percent_encodes_colon() {
        // The `:0` revision suffix on a foundation-model id must be encoded to
        // `%3A0`; dots and dashes are preserved.
        assert_eq!(
            bedrock_invoke_url(
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                "anthropic.claude-3-5-sonnet-20241022-v2:0"
            ),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke"
        );
    }

    // Bedrock Converse / ConverseStream URL helpers
    #[test]
    fn converse_url_basic() {
        assert_eq!(
            converse_url(
                "https://bedrock-runtime.eu-central-1.amazonaws.com",
                "eu.amazon.nova-pro-v1:0"
            ),
            "https://bedrock-runtime.eu-central-1.amazonaws.com/model/eu.amazon.nova-pro-v1%3A0/converse"
        );
    }

    #[test]
    fn converse_stream_url_basic() {
        assert_eq!(
            converse_stream_url(
                "https://bedrock-runtime.eu-central-1.amazonaws.com",
                "eu.amazon.nova-pro-v1:0"
            ),
            "https://bedrock-runtime.eu-central-1.amazonaws.com/model/eu.amazon.nova-pro-v1%3A0/converse-stream"
        );
    }

    #[test]
    fn converse_url_trims_trailing_slash() {
        assert_eq!(
            converse_url(
                "https://bedrock-runtime.us-east-1.amazonaws.com///",
                "us.meta.llama3-1-70b-instruct-v1:0"
            ),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.meta.llama3-1-70b-instruct-v1%3A0/converse"
        );
    }

    #[test]
    fn converse_stream_url_trims_trailing_slash() {
        assert_eq!(
            converse_stream_url(
                "https://bedrock-runtime.us-east-1.amazonaws.com/",
                "us.amazon.titan-text-premier-v1:0"
            ),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.amazon.titan-text-premier-v1%3A0/converse-stream"
        );
    }
}
