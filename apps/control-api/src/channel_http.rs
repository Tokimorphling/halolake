//! Channel-scoped HTTP client construction.
//!
//! Control-plane operations such as channel tests, model discovery, balance queries, and
//! credential refresh must use the same channel proxy binding as gateway traffic. A configured
//! `proxy_id` is therefore required: missing or disabled proxy records fail closed instead of
//! silently falling back to a direct connection.

use crate::proxy::ProxyStore;
use halolake_control_plane::ManagementError;
use halolake_domain::ChannelRecord;
use serde_json::Value as JsonValue;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Builds short-lived control-plane clients from a channel's transport policy.
#[derive(Debug, Clone)]
pub(crate) struct ChannelHttpClientFactory {
    proxies: ProxyStore,
}

impl ChannelHttpClientFactory {
    pub(crate) fn new(proxies: ProxyStore) -> Self {
        Self { proxies }
    }

    pub(crate) fn client_for_channel(
        &self,
        channel: &ChannelRecord,
    ) -> Result<reqwest::Client, ManagementError> {
        self.client(channel.proxy_id, channel.setting.as_deref())
    }

    pub(crate) fn client(
        &self,
        proxy_id: Option<u64>,
        setting: Option<&str>,
    ) -> Result<reqwest::Client, ManagementError> {
        let mut builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT);
        if let Some(proxy_url) = self.resolve_proxy_url(proxy_id, setting)? {
            let proxy = reqwest::Proxy::all(&proxy_url)
                .map_err(|_| ManagementError::InvalidRequest("channel proxy URL is invalid"))?;
            builder = builder.proxy(proxy);
        }
        builder
            .build()
            .map_err(|_| ManagementError::Storage("failed to build channel HTTP client".into()))
    }

    fn resolve_proxy_url(
        &self,
        proxy_id: Option<u64>,
        setting: Option<&str>,
    ) -> Result<Option<String>, ManagementError> {
        if let Some(proxy_id) = proxy_id {
            return self.proxies.resolve_url(Some(proxy_id)).map(Some).ok_or(
                ManagementError::InvalidRequest("channel proxy is missing or disabled"),
            );
        }
        Ok(legacy_setting_proxy_url(setting))
    }
}

fn legacy_setting_proxy_url(setting: Option<&str>) -> Option<String> {
    let raw = setting?.trim();
    if raw.is_empty() {
        return None;
    }
    let value: JsonValue = serde_json::from_str(raw).ok()?;
    value
        .get("proxy")?
        .as_str()
        .map(str::trim)
        .filter(|proxy| !proxy.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{CreateProxyRequest, ProxyRecord};
    use service_async::Service;

    #[tokio::test]
    async fn configured_proxy_is_required_and_never_falls_back_to_legacy_url() {
        let factory = ChannelHttpClientFactory::new(ProxyStore::memory());
        let result =
            factory.resolve_proxy_url(Some(42), Some(r#"{"proxy":"http://legacy.example:8080"}"#));
        assert!(matches!(result, Err(ManagementError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn disabled_proxy_fails_closed() {
        let proxies = ProxyStore::memory();
        let created = proxies
            .call(CreateProxyRequest {
                proxy: ProxyRecord {
                    id:     0,
                    name:   "disabled".into(),
                    url:    "http://proxy.example:8080".into(),
                    status: 0,
                    remark: String::new(),
                },
            })
            .await
            .unwrap();
        let factory = ChannelHttpClientFactory::new(proxies);
        let result = factory.resolve_proxy_url(Some(created.id), None);
        assert!(matches!(result, Err(ManagementError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn enabled_proxy_and_legacy_proxy_are_resolved() {
        let proxies = ProxyStore::memory();
        let created = proxies
            .call(CreateProxyRequest {
                proxy: ProxyRecord {
                    id:     0,
                    name:   "enabled".into(),
                    url:    "http://proxy.example:8080".into(),
                    status: 1,
                    remark: String::new(),
                },
            })
            .await
            .unwrap();
        let factory = ChannelHttpClientFactory::new(proxies);
        assert_eq!(
            factory.resolve_proxy_url(Some(created.id), None).unwrap(),
            Some("http://proxy.example:8080".into())
        );
        assert_eq!(
            factory
                .resolve_proxy_url(None, Some(r#"{"proxy":"socks5h://127.0.0.1:1080"}"#))
                .unwrap(),
            Some("socks5h://127.0.0.1:1080".into())
        );
    }
}
