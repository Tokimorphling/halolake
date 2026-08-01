use super::*;

pub(crate) struct GatewayRequest {
    pub(crate) headers: HeaderMap,
    pub(crate) path:    String,
    pub(crate) body:    Bytes,
}

pub(crate) struct RouteLookup {
    pub(crate) token:           String,
    pub(crate) requested_model: String,
    pub(crate) path:            String,
    pub(crate) headers:         HeaderMap,
    pub(crate) body:            Bytes,
    pub(crate) peer_ip:         IpAddr,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteParts {
    pub(crate) auth:     RequestAuth,
    pub(crate) route:    RouteContext,
    pub(crate) affinity: Option<RouteAffinityContext>,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteAffinityContext {
    pub(crate) cache_key:   String,
    pub(crate) ttl_seconds: u64,
    pub(crate) rule_name:   String,
}
