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
}
