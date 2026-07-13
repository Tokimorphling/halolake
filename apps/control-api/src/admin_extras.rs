//! Optional admin tooling beyond the proxy management core.
//!
//! Mounted when Cargo feature `admin-extras` is enabled (default).
//! See `docs/CONTROL_API_MODULES_CN.md`.

use super::*;

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    let router = crate::control_api_ext::mount(router);
    router
        .route("/api/pricing", get(api_pricing))
        .route("/api/rankings", get(api_rankings))
        .route("/api/perf-metrics", get(api_perf_metrics))
        .route("/api/perf-metrics/summary", get(api_perf_metrics_summary))
        .route(
            "/api/option/channel_affinity_cache",
            get(get_channel_affinity_cache_stats).delete(clear_channel_affinity_cache),
        )
        .route("/api/ratio_sync/channels", get(get_syncable_channels))
        .route("/api/ratio_sync/fetch", post(fetch_upstream_ratios))
        .route(
            "/api/models",
            get(api_models)
                .post(create_model_meta)
                .put(update_model_meta),
        )
        .route(
            "/api/models/",
            get(list_model_meta)
                .post(create_model_meta)
                .put(update_model_meta),
        )
        .route("/api/models/search", get(search_model_meta))
        .route("/api/models/missing", get(get_missing_models))
        .route(
            "/api/models/sync_upstream/preview",
            get(sync_upstream_preview),
        )
        .route("/api/models/sync_upstream", post(sync_upstream_models))
        .route(
            "/api/models/{id}",
            get(get_model_meta).delete(delete_model_meta),
        )
        .route(
            "/api/vendors",
            get(list_vendors).post(create_vendor).put(update_vendor),
        )
        .route(
            "/api/vendors/",
            get(list_vendors).post(create_vendor).put(update_vendor),
        )
        .route("/api/vendors/search", get(search_vendors))
        .route("/api/vendors/{id}", get(get_vendor).delete(delete_vendor))
        .route(
            "/api/user/checkin",
            get(get_checkin_status).post(do_checkin),
        )
        .route(
            "/api/log/channel_affinity_usage_cache",
            get(get_channel_affinity_usage_cache_stats),
        )
        .route(
            "/api/proxy",
            get(list_proxies).post(create_proxy).put(update_proxy),
        )
        .route(
            "/api/proxy/",
            get(list_proxies).post(create_proxy).put(update_proxy),
        )
        .route("/api/proxy/{id}", get(get_proxy).delete(delete_proxy))
        .route("/api/proxy/{id}/test", post(test_proxy))
        .route("/api/proxy/{id}/quality-check", post(quality_check_proxy))
        // Credential import (control_api_ext)
        .route("/api/channel/multi_key/manage", post(manage_multi_keys))
        .route("/api/channel/ollama/pull", post(ollama_pull_model))
        .route(
            "/api/channel/ollama/pull/stream",
            post(ollama_pull_model_stream),
        )
        .route(
            "/api/channel/ollama/delete",
            axum::routing::delete(ollama_delete_model),
        )
        .route("/api/channel/ollama/version/{id}", get(ollama_version))
        .route(
            "/api/channel/upstream_updates/apply",
            post(apply_channel_upstream_model_updates),
        )
        .route(
            "/api/channel/upstream_updates/apply_all",
            post(apply_all_channel_upstream_model_updates),
        )
        .route(
            "/api/channel/upstream_updates/detect",
            post(detect_channel_upstream_model_updates),
        )
        .route(
            "/api/channel/upstream_updates/detect_all",
            post(detect_all_channel_upstream_model_updates),
        )
        .route(
            "/api/channel/{id}/codex/refresh",
            post(refresh_codex_channel_credential),
        )
        .route(
            "/api/channel/{id}/codex/usage",
            get(get_codex_channel_usage),
        )
        .route(
            "/api/channel/{id}/codex/usage/reset-credits",
            get(get_codex_channel_rate_limit_reset_credits),
        )
        .route(
            "/api/channel/{id}/codex/usage/reset",
            post(reset_codex_channel_usage),
        )
}
