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
        let Some(rec) = management.channels.iter().find(|c| {
            c.snapshot_id.as_deref().unwrap_or(&c.id.to_string()) == ch.id.as_str()
                || c.id.to_string() == ch.id
        }) else {
            continue;
        };
        if let Some(pid) = rec.proxy_id {
            if let Some(url) = proxies.resolve_url(Some(pid)) {
                ch.proxy = Some(url);
            }
        }
    }
}
