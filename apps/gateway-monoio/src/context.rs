use super::*;

certain_map::certain_map! {
    #[empty(_RequestContextEmpty)]
    #[full(_FullRequestContext)]
    #[style = "unfilled"]
    #[derive(Clone)]
    pub struct RequestContext {
        request_id: RequestId,
        peer_addr: PeerAddr,
        downstream_protocol: DownstreamProtocol,
        request_auth: RequestAuth,
        route_context: RouteContext,
    }
}

#[derive(Debug, Clone)]
pub struct RequestId(pub String);

#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownstreamProtocol {
    OpenAiChat,
    OpenAiImage,
    OpenAiRaw,
    ClaudeMessages,
    GeminiGenerateContent,
}

#[derive(Debug, Clone)]
pub struct RequestAuth {
    pub user_id:  String,
    pub token_id: String,
}

/// Runtime proxy policy for a selected channel.
///
/// Keeping `Unavailable` distinct from `Direct` prevents a channel whose
/// required proxy was deleted or disabled from leaking traffic over the
/// gateway's direct egress.
#[derive(Clone, PartialEq, Eq)]
pub enum ProxyRoute {
    Direct,
    Required(String),
    Unavailable,
}

impl std::fmt::Debug for ProxyRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => formatter.write_str("Direct"),
            Self::Required(_) => formatter.write_str("Required(<redacted>)"),
            Self::Unavailable => formatter.write_str("Unavailable"),
        }
    }
}

impl ProxyRoute {
    pub fn from_snapshot(proxy: Option<String>, required: bool) -> Self {
        match proxy.map(|value| value.trim().to_string()) {
            Some(url) if !url.is_empty() => Self::Required(url),
            _ if required => Self::Unavailable,
            _ => Self::Direct,
        }
    }

    pub fn redacted_label(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Required(_) => "configured",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone)]
pub struct RouteContext {
    /// Route alias used only by model mappings and affinity.
    pub channel_id:             String,
    /// Stable management-store ID used by feedback, usage and runtime isolation.
    pub management_channel_id:  Option<u64>,
    pub provider:               Provider,
    pub base_url:               String,
    pub api_key:                String,
    /// SHA-256 of the selected credential; safe for stale-feedback comparison.
    pub api_key_fingerprint:    String,
    pub api_key_index:          Option<usize>,
    pub using_group:            String,
    pub requested_model:        String,
    pub upstream_model:         String,
    pub proxy:                  ProxyRoute,
    /// Explicit channel header overrides (after default/auth headers).
    pub header_override:        std::collections::BTreeMap<String, String>,
    /// auto | openai | openai-response (channel setting upstream_endpoint_type).
    pub upstream_endpoint_type: String,
}

impl std::fmt::Debug for RouteContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header_override_keys = self.header_override.keys().collect::<Vec<_>>();
        formatter
            .debug_struct("RouteContext")
            .field("channel_id", &self.channel_id)
            .field("management_channel_id", &self.management_channel_id)
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("api_key_fingerprint", &self.api_key_fingerprint)
            .field("api_key_index", &self.api_key_index)
            .field("using_group", &self.using_group)
            .field("requested_model", &self.requested_model)
            .field("upstream_model", &self.upstream_model)
            .field("proxy", &self.proxy)
            .field("header_override_keys", &header_override_keys)
            .field("upstream_endpoint_type", &self.upstream_endpoint_type)
            .finish()
    }
}

pub(crate) fn reporting_channel_id(management_id: Option<u64>, route_alias: &str) -> String {
    management_id.map_or_else(|| route_alias.to_string(), |id| id.to_string())
}

pub(crate) fn proxy_circuit_channel_identity(
    management_id: Option<u64>,
    route_alias: &str,
) -> String {
    management_id.map_or_else(
        || format!("alias:{route_alias}"),
        |id| format!("management:{id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::{ProxyRoute, RouteContext, proxy_circuit_channel_identity, reporting_channel_id};
    use halolake_router_core::Provider;

    #[test]
    fn proxy_route_distinguishes_direct_required_and_unavailable() {
        assert_eq!(ProxyRoute::from_snapshot(None, false), ProxyRoute::Direct);
        assert_eq!(
            ProxyRoute::from_snapshot(Some(" http://proxy.test:8080 ".into()), false),
            ProxyRoute::Required("http://proxy.test:8080".into())
        );
        assert_eq!(
            ProxyRoute::from_snapshot(None, true),
            ProxyRoute::Unavailable
        );
        assert_eq!(
            ProxyRoute::from_snapshot(Some("   ".into()), true),
            ProxyRoute::Unavailable
        );

        let route =
            ProxyRoute::Required("http://unique-user:unique-password@proxy.test:8080".into());
        let debug = format!("{route:?}");
        assert!(!debug.contains("unique-user"));
        assert!(!debug.contains("unique-password"));
    }

    #[test]
    fn stable_channel_identities_prefer_management_id() {
        assert_eq!(reporting_channel_id(Some(42), "numeric-looking-7"), "42");
        assert_eq!(reporting_channel_id(None, "legacy-alias"), "legacy-alias");
        assert_eq!(
            proxy_circuit_channel_identity(Some(42), "ignored-alias"),
            "management:42"
        );
        assert_eq!(
            proxy_circuit_channel_identity(None, "legacy-alias"),
            "alias:legacy-alias"
        );
    }

    #[test]
    fn route_context_debug_redacts_selected_api_key() {
        let mut header_override = std::collections::BTreeMap::new();
        header_override.insert(
            "authorization".to_string(),
            "Bearer unique-override-secret".to_string(),
        );
        let route = RouteContext {
            channel_id: "route-a".to_string(),
            management_channel_id: Some(42),
            provider: Provider::OpenAi,
            base_url: "https://example.com".to_string(),
            api_key: "unique-plaintext-secret".to_string(),
            api_key_fingerprint: "safe-fingerprint".to_string(),
            api_key_index: Some(1),
            using_group: "default".to_string(),
            requested_model: "model-a".to_string(),
            upstream_model: "model-a".to_string(),
            proxy: ProxyRoute::Direct,
            header_override,
            upstream_endpoint_type: "auto".to_string(),
        };

        let debug = format!("{route:?}");
        assert!(!debug.contains("unique-plaintext-secret"));
        assert!(!debug.contains("unique-override-secret"));
        assert!(debug.contains("safe-fingerprint"));
    }
}
