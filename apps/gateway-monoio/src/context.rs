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

#[derive(Debug, Clone)]
pub struct RouteContext {
    pub channel_id:      String,
    pub provider:        Provider,
    pub base_url:        String,
    pub api_key:         String,
    pub api_key_index:   Option<usize>,
    pub using_group:     String,
    pub requested_model: String,
    pub upstream_model:  String,
    pub proxy:           Option<String>,
    /// Explicit channel header overrides (after default/auth headers).
    pub header_override: std::collections::BTreeMap<String, String>,
}
