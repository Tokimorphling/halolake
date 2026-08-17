//! Normalization and validation for imported upstream base URLs.
//!
//! Import files in the wild commonly contain an operation URL (for example
//! `/v1/chat/completions`) even though a Halolake channel stores a base URL
//! and appends the operation path at relay time.  Keeping this logic in one
//! place avoids provider-specific, subtly different URL handling.

use http::Uri;

/// Normalize an imported channel base URL.
///
/// `provider` is the normalized provider name (`openai`, `anthropic`, or
/// `xai`). `xai_using_api` is only used for the known xAI OAuth/API hosts.
/// Empty input is intentionally represented as `None`, allowing the runtime
/// provider default to apply.
pub(crate) fn normalize_import_base_url(
    raw: Option<&str>,
    provider: &str,
    xai_using_api: Option<bool>,
) -> Result<Option<String>, String> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    let uri: Uri = raw
        .parse()
        .map_err(|err| format!("invalid base_url: {err}"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| "base_url must be an absolute URL".to_string())?;
    if !matches!(scheme, "http" | "https") {
        return Err("base_url scheme must be http or https".into());
    }
    let authority = uri
        .authority()
        .ok_or_else(|| "base_url must include a host".to_string())?;
    if authority.as_str().contains('@') {
        return Err("base_url must not contain userinfo".into());
    }
    if uri.query().is_some() {
        return Err("base_url must not contain a query string".into());
    }
    let host = uri
        .host()
        .ok_or_else(|| "base_url must include a host".to_string())?;
    let provider = provider.trim().to_ascii_lowercase();
    let mut path = uri.path().trim_end_matches('/').to_string();
    strip_operation_suffix(&mut path, &provider);
    if path == "/" {
        path.clear();
    }

    let is_xai = provider == "xai" || provider == "grok";
    let host_lower = host.to_ascii_lowercase();
    if is_xai && host_lower == "api.x.ai" {
        let use_api = xai_using_api.unwrap_or(true);
        if !use_api {
            return Ok(Some("https://cli-chat-proxy.grok.com/v1".into()));
        }
        return Ok(Some("https://api.x.ai/v1".into()));
    }
    if is_xai && host_lower == "cli-chat-proxy.grok.com" {
        return Ok(Some("https://cli-chat-proxy.grok.com/v1".into()));
    }

    // Relay paths already include `/v1` for standard OpenAI/Claude requests.
    // Persist the host/prefix only; otherwise joining would produce `/v1/v1`.
    if matches!(provider.as_str(), "openai" | "anthropic" | "claude")
        && (path == "/v1" || path.ends_with("/v1"))
    {
        path.truncate(path.len().saturating_sub(3));
    }
    let authority = authority.as_str();
    let mut normalized = format!("{scheme}://{authority}{path}");
    if normalized.ends_with('/') && path != "/" {
        normalized.pop();
    }
    Ok(Some(normalized))
}

/// Remove paths that are sent by a client but must not be persisted as the
/// channel base. The longest forms are checked first.
fn strip_operation_suffix(path: &mut String, provider: &str) {
    // Codex and xAI channels append `/responses` to a base that may already
    // include `/v1`. Preserve that version prefix when an imported file stores
    // a complete operation URL. Standard OpenAI/Claude channels append paths
    // that already include `/v1`, so their full suffix is removed below.
    if matches!(provider, "codex" | "xai" | "grok") {
        const RESPONSE_SUFFIXES: &[&str] = &[
            "/chat/completions",
            "/responses/compact",
            "/responses",
            "/embeddings",
            "/completions",
        ];
        let lower = path.to_ascii_lowercase();
        for suffix in RESPONSE_SUFFIXES {
            if lower.ends_with(suffix) {
                let end = path.len().saturating_sub(suffix.len());
                path.truncate(end);
                return;
            }
        }
    }

    const SUFFIXES: &[&str] = &[
        "/v1/chat/completions",
        "/chat/completions",
        "/v1/responses/compact",
        "/responses/compact",
        "/v1/responses",
        "/responses",
        "/v1/messages/count_tokens",
        "/messages/count_tokens",
        "/v1/messages",
        "/messages",
        "/v1/embeddings",
        "/embeddings",
        "/v1/completions",
        "/completions",
    ];
    let lower = path.to_ascii_lowercase();
    for suffix in SUFFIXES {
        if lower.ends_with(suffix) {
            let end = path.len().saturating_sub(suffix.len());
            path.truncate(end);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_operation_paths_and_preserves_prefix() {
        assert_eq!(
            normalize_import_base_url(
                Some("https://relay.example/xai/v1/chat/completions"),
                "openai",
                None,
            )
            .unwrap()
            .as_deref(),
            Some("https://relay.example/xai")
        );
        assert_eq!(
            normalize_import_base_url(Some("https://api.x.ai/v1/responses"), "xai", Some(true))
                .unwrap()
                .as_deref(),
            Some("https://api.x.ai/v1")
        );
        assert_eq!(
            normalize_import_base_url(Some("https://api.x.ai/responses"), "xai", Some(false),)
                .unwrap()
                .as_deref(),
            Some("https://cli-chat-proxy.grok.com/v1")
        );
        assert_eq!(
            normalize_import_base_url(
                Some("https://relay.example/custom/v1/responses"),
                "xai",
                Some(true),
            )
            .unwrap()
            .as_deref(),
            Some("https://relay.example/custom/v1")
        );
        assert_eq!(
            normalize_import_base_url(
                Some("https://relay.example/custom/v1/chat/completions"),
                "codex",
                None,
            )
            .unwrap()
            .as_deref(),
            Some("https://relay.example/custom/v1")
        );
    }

    #[test]
    fn strips_v1_for_standard_root_urls() {
        assert_eq!(
            normalize_import_base_url(Some("https://api.openai.com"), "openai", None)
                .unwrap()
                .as_deref(),
            Some("https://api.openai.com")
        );
        assert_eq!(
            normalize_import_base_url(
                Some("https://api.openai.com/v1/chat/completions"),
                "openai",
                None,
            )
            .unwrap()
            .as_deref(),
            Some("https://api.openai.com")
        );
        assert_eq!(
            normalize_import_base_url(Some("https://api.anthropic.com/"), "anthropic", None)
                .unwrap()
                .as_deref(),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            normalize_import_base_url(
                Some("https://relay.example/prefix/v1/chat/completions"),
                "openai",
                None,
            )
            .unwrap()
            .as_deref(),
            Some("https://relay.example/prefix")
        );
    }

    #[test]
    fn rejects_unsafe_url_forms() {
        for value in [
            "ftp://example.com/v1",
            "https://user:pass@example.com/v1",
            "https://example.com/v1?key=secret",
            "/relative/path",
        ] {
            assert!(
                normalize_import_base_url(Some(value), "openai", None).is_err(),
                "{value}"
            );
        }
    }
}
