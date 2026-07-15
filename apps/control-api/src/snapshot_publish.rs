use crate::{
    AppState, channel_affinity_config_from_options, group_routing_config_from_options,
    proxy::ProxyStore,
    storage::{ManagementStore, OptionStore},
};
use halolake_control_plane::{
    ManagementData, ManagementError, MemorySnapshotBus, PublishSnapshotRequest,
};
use halolake_router_core::GatewaySnapshot;
use service_async::Service;

pub(crate) async fn publish_management_snapshot(state: &AppState) -> Result<(), ManagementError> {
    publish_enriched_management_snapshot(
        &state.management,
        &state.options,
        &state.snapshots,
        &state.proxies,
    )
    .await
}

pub(crate) async fn publish_enriched_management_snapshot(
    management: &ManagementStore,
    options: &OptionStore,
    snapshots: &MemorySnapshotBus,
    proxies: &ProxyStore,
) -> Result<(), ManagementError> {
    // Do NOT bump here. Write paths already advanced the version via
    // `mutate()`. Options-only changes must call `bump_version()` before this
    // helper so the gateway does not treat the republish as NotModified.
    let data = management.current_data()?;
    let mut snapshot = data.build_snapshot()?;
    apply_channel_proxies(&mut snapshot, &data, proxies);
    let option_values = options.values()?;
    snapshot.channel_affinity = channel_affinity_config_from_options(&option_values);
    snapshot.group_routing = group_routing_config_from_options(&option_values);
    snapshots
        .call(PublishSnapshotRequest { snapshot })
        .await
        .map_err(ManagementError::Snapshot)?;
    Ok(())
}

fn apply_channel_proxies(
    snapshot: &mut GatewaySnapshot,
    management: &ManagementData,
    proxies: &ProxyStore,
) {
    for ch in &mut snapshot.channels {
        let rec = match ch.management_id {
            Some(id) => management.channels.iter().find(|channel| channel.id == id),
            None => management.channels.iter().find(|channel| {
                channel
                    .snapshot_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map_or_else(|| channel.id.to_string(), str::to_string)
                    == ch.id
            }),
        };
        let Some(rec) = rec else {
            continue;
        };
        if let Some(pid) = rec.proxy_id {
            // A proxy binding is a routing requirement, not a best-effort
            // hint. Keep the channel fail-closed when the referenced proxy is
            // missing or disabled; otherwise `None` would silently mean
            // direct connect in the gateway.
            ch.proxy_required = true;
            ch.proxy = None;
            if let Some(url) = proxies.resolve_url(Some(pid)) {
                ch.proxy = Some(url);
            } else {
                // Runtime-only disable: keep persisted channel status intact so
                // re-enabling or restoring the proxy makes the channel
                // schedulable again on the next snapshot.
                ch.enabled = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{CreateProxyRequest, ProxyRecord};
    use halolake_domain::{CHANNEL_TYPE_OPENAI, ChannelRecord, STATUS_ENABLED};

    fn proxied_channel(proxy_id: u64) -> ChannelRecord {
        ChannelRecord {
            id:                   1,
            snapshot_id:          None,
            channel_type:         CHANNEL_TYPE_OPENAI,
            key:                  "sk-test".into(),
            status:               STATUS_ENABLED,
            name:                 "proxied".into(),
            weight:               Some(1),
            created_time:         0,
            test_time:            0,
            response_time:        0,
            base_url:             Some("https://example.com".into()),
            balance:              0.0,
            balance_updated_time: 0,
            models:               "gpt-test".into(),
            group:                "default".into(),
            used_quota:           0,
            model_mapping:        None,
            priority:             Some(0),
            auto_ban:             Some(1),
            tag:                  None,
            setting:              None,
            param_override:       None,
            header_override:      None,
            remark:               None,
            proxy_id:             Some(proxy_id),
        }
    }

    #[test]
    fn unresolved_required_proxy_disables_runtime_channel() {
        let management = ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![proxied_channel(7)],
            Vec::new(),
        );
        let mut snapshot = management.build_snapshot().expect("snapshot");

        apply_channel_proxies(&mut snapshot, &management, &ProxyStore::memory());

        let channel = snapshot.channels.first().expect("channel");
        assert!(channel.proxy_required);
        assert!(channel.proxy.is_none());
        assert!(!channel.enabled);
    }

    #[tokio::test]
    async fn resolved_required_proxy_stays_enabled() {
        let proxies = ProxyStore::memory();
        proxies
            .call(CreateProxyRequest {
                proxy: ProxyRecord {
                    id:     7,
                    name:   "proxy".into(),
                    url:    "http://127.0.0.1:8080".into(),
                    status: 1,
                    remark: String::new(),
                },
            })
            .await
            .expect("create proxy");
        let management = ManagementData::new(
            1,
            Vec::new(),
            Vec::new(),
            vec![proxied_channel(7)],
            Vec::new(),
        );
        let mut snapshot = management.build_snapshot().expect("snapshot");

        apply_channel_proxies(&mut snapshot, &management, &proxies);

        let channel = snapshot.channels.first().expect("channel");
        assert!(channel.proxy_required);
        assert_eq!(channel.proxy.as_deref(), Some("http://127.0.0.1:8080"));
        assert!(channel.enabled);
    }

    #[tokio::test]
    async fn proxy_enrichment_uses_management_id_not_numeric_route_alias() {
        let proxies = ProxyStore::memory();
        proxies
            .call(CreateProxyRequest {
                proxy: ProxyRecord {
                    id:     7,
                    name:   "proxy".into(),
                    url:    "http://127.0.0.1:8080".into(),
                    status: 1,
                    remark: String::new(),
                },
            })
            .await
            .expect("create proxy");
        let mut first = proxied_channel(0);
        first.id = 1;
        first.snapshot_id = Some("route-a".into());
        first.proxy_id = None;
        let mut second = proxied_channel(7);
        second.id = 2;
        // This route alias looks like the first channel's management id.
        second.snapshot_id = Some("1".into());
        let management =
            ManagementData::new(1, Vec::new(), Vec::new(), vec![first, second], Vec::new());
        let mut snapshot = management.build_snapshot().expect("snapshot");

        apply_channel_proxies(&mut snapshot, &management, &proxies);

        let first = snapshot
            .channels
            .iter()
            .find(|channel| channel.management_id == Some(1))
            .expect("first");
        assert!(first.proxy.is_none());
        assert!(!first.proxy_required);
        let second = snapshot
            .channels
            .iter()
            .find(|channel| channel.management_id == Some(2))
            .expect("second");
        assert_eq!(second.id, "1");
        assert_eq!(second.proxy.as_deref(), Some("http://127.0.0.1:8080"));
        assert!(second.proxy_required);
    }
}
