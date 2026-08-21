use super::*;

pub(crate) struct GatewayRequest {
    pub(crate) headers: HeaderMap,
    pub(crate) path:    String,
    pub(crate) body:    Bytes,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteLookup {
    pub(crate) tokens:          Vec<String>,
    pub(crate) requested_model: FastStr,
    pub(crate) path:            String,
    pub(crate) headers:         HeaderMap,
    pub(crate) body:            Bytes,
    pub(crate) peer_ip:         IpAddr,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteParts {
    pub(crate) auth:     RequestAuth,
    pub(crate) route:    Rc<RouteContext>,
    pub(crate) affinity: Option<RouteAffinityContext>,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteAffinityContext {
    pub(crate) cache_key:             String,
    pub(crate) ttl_seconds:           u64,
    pub(crate) rule_name:             String,
    pub(crate) skip_retry_on_failure: bool,
}
