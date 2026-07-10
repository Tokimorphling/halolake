use super::*;

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub protocol: ProtocolConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub control: ControlPlaneConfig,
    #[serde(default = "default_version")]
    pub version: u64,
    #[serde(default)]
    pub tokens: Vec<halolake_router_core::TokenConfig>,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub model_mappings: Vec<halolake_router_core::ModelMapping>,
    #[serde(default)]
    pub channel_affinity: halolake_router_core::ChannelAffinityConfig,
    #[serde(default)]
    pub group_routing: halolake_router_core::GroupRoutingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_body_limit")]
    pub request_body_limit_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            request_body_limit_bytes: default_body_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProtocolConfig {
    #[serde(default = "default_claude_version")]
    pub claude_version: String,
    #[serde(default = "default_true")]
    pub pass_anthropic_beta: bool,
    #[serde(default = "default_gemini_api_version")]
    pub gemini_api_version: String,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            claude_version: default_claude_version(),
            pass_anthropic_beta: true,
            gemini_api_version: default_gemini_api_version(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub read_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_true")]
    pub accept_bearer: bool,
    #[serde(default = "default_true")]
    pub accept_x_api_key: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ControlPlaneConfig {
    #[serde(default)]
    pub snapshot_url: Option<String>,
    #[serde(default)]
    pub usage_url: Option<String>,
    #[serde(default)]
    pub channel_feedback_url: Option<String>,
    #[serde(default)]
    pub internal_key: Option<String>,
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default)]
    pub read_timeout_ms: Option<u64>,
    #[serde(default)]
    pub snapshot_poll_interval_ms: Option<u64>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            accept_bearer: true,
            accept_x_api_key: true,
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

fn default_body_limit() -> usize {
    16 * 1024 * 1024
}

fn default_claude_version() -> String {
    "2023-06-01".to_string()
}

fn default_gemini_api_version() -> String {
    "v1beta".to_string()
}

fn default_version() -> u64 {
    1
}

fn default_true() -> bool {
    true
}
