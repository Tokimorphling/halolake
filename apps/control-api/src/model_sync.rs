use crate::{
    catalog::{
        CatalogData, CatalogStore, CreateModelRequest, CreateVendorRequest, ModelRecord,
        UpdateModelRequest, VendorRecord, missing_models,
    },
    storage::ManagementStore,
};
use halolake_control_plane::ManagementError;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use service_async::Service;
use std::collections::BTreeMap;

const DEFAULT_SYNC_UPSTREAM_BASE: &str = "https://basellm.github.io/llm-metadata";

#[derive(Debug, Clone)]
pub(crate) struct ModelSyncService {
    management: ManagementStore,
    catalog:    CatalogStore,
    client:     reqwest::Client,
}

impl ModelSyncService {
    pub(crate) fn new(management: ManagementStore, catalog: CatalogStore) -> Self {
        Self {
            management,
            catalog,
            client: reqwest::Client::new(),
        }
    }

    async fn fetch_upstream(
        &self,
        locale: &str,
    ) -> Result<(Vec<UpstreamModel>, Vec<UpstreamVendor>, SyncSource), ManagementError> {
        let source = SyncSource::from_locale(locale);
        let models = self.fetch_json::<UpstreamModel>(&source.models_url).await?;
        let vendors = self
            .fetch_json::<UpstreamVendor>(&source.vendors_url)
            .await
            .unwrap_or_default();
        Ok((models, vendors, source))
    }

    async fn fetch_json<T>(&self, url: &str) -> Result<Vec<T>, ManagementError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self.client.get(url).send().await.map_err(storage_err)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(storage_err)?;
        if !status.is_success() {
            return Err(ManagementError::Storage(status.to_string()));
        }
        if let Ok(envelope) = serde_json::from_slice::<UpstreamEnvelope<T>>(&bytes) {
            return Ok(envelope.data);
        }
        serde_json::from_slice::<Vec<T>>(&bytes).map_err(storage_err)
    }
}

impl Service<SyncUpstreamPreviewRequest> for ModelSyncService {
    type Response = SyncUpstreamPreviewResponse;
    type Error = ManagementError;

    async fn call(&self, req: SyncUpstreamPreviewRequest) -> Result<Self::Response, Self::Error> {
        let (models, vendors, source) = self.fetch_upstream(&req.locale).await?;
        let catalog = self.catalog.current_data()?;
        let management = self.management.current_data()?;
        let model_by_name = upstream_model_map(models);
        let vendor_by_id = vendor_names_by_id(&catalog);

        let missing = missing_models(&management, &catalog)
            .into_iter()
            .filter(|name| model_by_name.contains_key(name))
            .collect::<Vec<_>>();

        let mut conflicts = Vec::new();
        for local in catalog.models {
            if local.sync_official == 0 {
                continue;
            }
            let Some(upstream) = model_by_name.get(&local.model_name) else {
                continue;
            };
            let fields = conflict_fields(&local, upstream, vendor_by_id.get(&local.vendor_id));
            if !fields.is_empty() {
                conflicts.push(SyncConflict {
                    model_name: local.model_name,
                    fields,
                });
            }
        }

        Ok(SyncUpstreamPreviewResponse {
            missing,
            conflicts,
            source,
            upstream_vendors: vendors.len(),
        })
    }
}

impl Service<SyncUpstreamModelsRequest> for ModelSyncService {
    type Response = SyncUpstreamModelsResponse;
    type Error = ManagementError;

    async fn call(&self, req: SyncUpstreamModelsRequest) -> Result<Self::Response, Self::Error> {
        let management = self.management.current_data()?;
        let catalog = self.catalog.current_data()?;
        let missing = missing_models(&management, &catalog);
        if missing.is_empty() && req.overwrite.is_empty() {
            return Ok(SyncUpstreamModelsResponse {
                source: SyncSource::from_locale(&req.locale),
                ..SyncUpstreamModelsResponse::default()
            });
        }

        let (models, vendors, source) = self.fetch_upstream(&req.locale).await?;
        let model_by_name = upstream_model_map(models);
        let vendor_by_name = upstream_vendor_map(vendors);
        let mut vendor_id_cache = BTreeMap::new();
        let mut response = SyncUpstreamModelsResponse {
            source,
            ..SyncUpstreamModelsResponse::default()
        };

        for name in missing {
            let Some(upstream) = model_by_name.get(&name) else {
                response.skipped_models.push(name);
                continue;
            };
            if catalog
                .models
                .iter()
                .any(|model| model.model_name == name && model.sync_official == 0)
            {
                response.skipped_models.push(name);
                continue;
            }
            let vendor_id = ensure_vendor_id(
                &self.catalog,
                &upstream.vendor_name,
                &vendor_by_name,
                &mut vendor_id_cache,
                &mut response.created_vendors,
            )
            .await?;
            let model = model_from_upstream(upstream, vendor_id);
            match self.catalog.call(CreateModelRequest { model }).await {
                Ok(_) => {
                    response.created_models = response.created_models.saturating_add(1);
                    response.created_list.push(name);
                }
                Err(_) => response.skipped_models.push(name),
            }
        }

        if !req.overwrite.is_empty() {
            let fresh_catalog = self.catalog.current_data()?;
            for overwrite in req.overwrite {
                let Some(upstream) = model_by_name.get(&overwrite.model_name) else {
                    continue;
                };
                let Some(mut local) = fresh_catalog
                    .models
                    .iter()
                    .find(|model| {
                        model.model_name == overwrite.model_name && model.sync_official != 0
                    })
                    .cloned()
                else {
                    continue;
                };
                let vendor_id = ensure_vendor_id(
                    &self.catalog,
                    &upstream.vendor_name,
                    &vendor_by_name,
                    &mut vendor_id_cache,
                    &mut response.created_vendors,
                )
                .await?;
                if apply_overwrite_fields(&mut local, upstream, vendor_id, &overwrite.fields) {
                    self.catalog
                        .call(UpdateModelRequest {
                            model:       local,
                            status_only: false,
                        })
                        .await?;
                    response.updated_models = response.updated_models.saturating_add(1);
                    response.updated_list.push(overwrite.model_name);
                }
            }
        }

        Ok(response)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct UpstreamEnvelope<T> {
    data: Vec<T>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UpstreamModel {
    #[serde(default)]
    description: String,
    #[serde(default)]
    endpoints:   JsonValue,
    #[serde(default)]
    icon:        String,
    #[serde(default)]
    model_name:  String,
    #[serde(default)]
    name_rule:   i32,
    #[serde(default)]
    status:      i32,
    #[serde(default)]
    tags:        String,
    #[serde(default)]
    vendor_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UpstreamVendor {
    #[serde(default)]
    description: String,
    #[serde(default)]
    icon:        String,
    #[serde(default)]
    name:        String,
    #[serde(default)]
    status:      i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct SyncUpstreamPreviewRequest {
    #[serde(default)]
    pub(crate) locale: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct SyncUpstreamModelsRequest {
    #[serde(default)]
    pub(crate) overwrite: Vec<OverwriteField>,
    #[serde(default)]
    pub(crate) locale:    String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct OverwriteField {
    pub(crate) model_name: String,
    #[serde(default)]
    pub(crate) fields:     Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SyncUpstreamPreviewResponse {
    pub(crate) missing:          Vec<String>,
    pub(crate) conflicts:        Vec<SyncConflict>,
    pub(crate) source:           SyncSource,
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub(crate) upstream_vendors: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncConflict {
    pub(crate) model_name: String,
    pub(crate) fields:     Vec<SyncConflictField>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncConflictField {
    pub(crate) field:    String,
    pub(crate) local:    JsonValue,
    pub(crate) upstream: JsonValue,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SyncUpstreamModelsResponse {
    pub(crate) created_models:  usize,
    pub(crate) created_vendors: usize,
    pub(crate) updated_models:  usize,
    pub(crate) skipped_models:  Vec<String>,
    pub(crate) created_list:    Vec<String>,
    pub(crate) updated_list:    Vec<String>,
    pub(crate) source:          SyncSource,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SyncSource {
    pub(crate) locale:      String,
    pub(crate) models_url:  String,
    pub(crate) vendors_url: String,
}

impl SyncSource {
    fn from_locale(locale: &str) -> Self {
        let locale = normalize_locale(locale).unwrap_or_default();
        let base = std::env::var("SYNC_UPSTREAM_BASE")
            .unwrap_or_else(|_| DEFAULT_SYNC_UPSTREAM_BASE.to_string())
            .trim_end_matches('/')
            .to_string();
        let (models_url, vendors_url) = if locale.is_empty() {
            (
                format!("{base}/api/newapi/models.json"),
                format!("{base}/api/newapi/vendors.json"),
            )
        } else {
            (
                format!("{base}/api/i18n/{locale}/newapi/models.json"),
                format!("{base}/api/i18n/{locale}/newapi/vendors.json"),
            )
        };
        Self {
            locale,
            models_url,
            vendors_url,
        }
    }
}

async fn ensure_vendor_id(
    catalog: &CatalogStore,
    vendor_name: &str,
    vendor_by_name: &BTreeMap<String, UpstreamVendor>,
    cache: &mut BTreeMap<String, u64>,
    created_vendors: &mut usize,
) -> Result<u64, ManagementError> {
    let vendor_name = vendor_name.trim();
    if vendor_name.is_empty() {
        return Ok(0);
    }
    if let Some(id) = cache.get(vendor_name).copied() {
        return Ok(id);
    }
    let data = catalog.current_data()?;
    if let Some(vendor) = data
        .vendors
        .iter()
        .find(|vendor| vendor.name == vendor_name)
    {
        cache.insert(vendor_name.to_string(), vendor.id);
        return Ok(vendor.id);
    }
    let upstream = vendor_by_name.get(vendor_name).cloned().unwrap_or_default();
    let vendor = catalog
        .call(CreateVendorRequest {
            vendor: VendorRecord {
                id:           0,
                name:         vendor_name.to_string(),
                description:  upstream.description,
                icon:         upstream.icon,
                status:       choose_status(upstream.status, 1),
                created_time: 0,
                updated_time: 0,
            },
        })
        .await?;
    *created_vendors = created_vendors.saturating_add(1);
    cache.insert(vendor_name.to_string(), vendor.id);
    Ok(vendor.id)
}

fn model_from_upstream(upstream: &UpstreamModel, vendor_id: u64) -> ModelRecord {
    ModelRecord {
        id: 0,
        model_name: upstream.model_name.trim().to_string(),
        description: upstream.description.clone(),
        icon: upstream.icon.clone(),
        tags: upstream.tags.clone(),
        vendor_id,
        endpoints: endpoints_string(&upstream.endpoints),
        status: choose_status(upstream.status, 1),
        sync_official: 1,
        created_time: 0,
        updated_time: 0,
        bound_channels: Vec::new(),
        enable_groups: Vec::new(),
        quota_types: Vec::new(),
        name_rule: upstream.name_rule,
        matched_models: Vec::new(),
        matched_count: 0,
    }
}

fn apply_overwrite_fields(
    local: &mut ModelRecord,
    upstream: &UpstreamModel,
    vendor_id: u64,
    fields: &[String],
) -> bool {
    let mut changed = false;
    for field in fields {
        match field.trim().to_ascii_lowercase().as_str() {
            "description" => {
                local.description.clone_from(&upstream.description);
                changed = true;
            }
            "icon" => {
                local.icon.clone_from(&upstream.icon);
                changed = true;
            }
            "tags" => {
                local.tags.clone_from(&upstream.tags);
                changed = true;
            }
            "vendor" => {
                local.vendor_id = vendor_id;
                changed = true;
            }
            "name_rule" => {
                local.name_rule = upstream.name_rule;
                changed = true;
            }
            "status" => {
                local.status = choose_status(upstream.status, local.status);
                changed = true;
            }
            "endpoints" => {
                local.endpoints = endpoints_string(&upstream.endpoints);
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

fn conflict_fields(
    local: &ModelRecord,
    upstream: &UpstreamModel,
    local_vendor_name: Option<&String>,
) -> Vec<SyncConflictField> {
    let mut fields = Vec::new();
    push_string_conflict(
        &mut fields,
        "description",
        &local.description,
        &upstream.description,
    );
    push_string_conflict(&mut fields, "icon", &local.icon, &upstream.icon);
    push_string_conflict(&mut fields, "tags", &local.tags, &upstream.tags);
    push_string_conflict(
        &mut fields,
        "vendor",
        local_vendor_name.map(String::as_str).unwrap_or_default(),
        &upstream.vendor_name,
    );
    if local.name_rule != upstream.name_rule {
        fields.push(SyncConflictField {
            field:    "name_rule".to_string(),
            local:    JsonValue::from(local.name_rule),
            upstream: JsonValue::from(upstream.name_rule),
        });
    }
    let upstream_status = choose_status(upstream.status, local.status);
    if local.status != upstream_status {
        fields.push(SyncConflictField {
            field:    "status".to_string(),
            local:    JsonValue::from(local.status),
            upstream: JsonValue::from(upstream.status),
        });
    }
    fields
}

fn push_string_conflict(
    fields: &mut Vec<SyncConflictField>,
    name: &str,
    local: &str,
    upstream: &str,
) {
    if local.trim() == upstream.trim() {
        return;
    }
    fields.push(SyncConflictField {
        field:    name.to_string(),
        local:    JsonValue::from(local),
        upstream: JsonValue::from(upstream),
    });
}

fn upstream_model_map(models: Vec<UpstreamModel>) -> BTreeMap<String, UpstreamModel> {
    models
        .into_iter()
        .filter(|model| !model.model_name.trim().is_empty())
        .map(|model| (model.model_name.trim().to_string(), model))
        .collect()
}

fn upstream_vendor_map(vendors: Vec<UpstreamVendor>) -> BTreeMap<String, UpstreamVendor> {
    vendors
        .into_iter()
        .filter(|vendor| !vendor.name.trim().is_empty())
        .map(|vendor| (vendor.name.trim().to_string(), vendor))
        .collect()
}

fn vendor_names_by_id(catalog: &CatalogData) -> BTreeMap<u64, String> {
    catalog
        .vendors
        .iter()
        .map(|vendor| (vendor.id, vendor.name.clone()))
        .collect()
}

fn choose_status(primary: i32, fallback: i32) -> i32 {
    if primary != 0 {
        primary
    } else if fallback != 0 {
        fallback
    } else {
        1
    }
}

fn endpoints_string(value: &JsonValue) -> String {
    if value.is_null() {
        String::new()
    } else if let Some(text) = value.as_str() {
        text.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_default()
    }
}

fn normalize_locale(locale: &str) -> Option<String> {
    match locale.trim() {
        "en" => Some("en".to_string()),
        "zh-CN" => Some("zh-CN".to_string()),
        "zh-TW" => Some("zh-TW".to_string()),
        "ja" => Some("ja".to_string()),
        _ => None,
    }
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

fn storage_err(err: impl std::fmt::Display) -> ManagementError {
    ManagementError::Storage(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_default_and_i18n_source_urls() {
        let source = SyncSource::from_locale("");
        assert!(source.models_url.ends_with("/api/newapi/models.json"));

        let source = SyncSource::from_locale("zh-CN");
        assert!(
            source
                .models_url
                .ends_with("/api/i18n/zh-CN/newapi/models.json")
        );
    }

    #[test]
    fn conflict_detects_vendor_by_name() {
        let local = ModelRecord {
            id:             1,
            model_name:     "gpt-a".to_string(),
            description:    "old".to_string(),
            icon:           String::new(),
            tags:           String::new(),
            vendor_id:      1,
            endpoints:      String::new(),
            status:         1,
            sync_official:  1,
            created_time:   0,
            updated_time:   0,
            bound_channels: Vec::new(),
            enable_groups:  Vec::new(),
            quota_types:    Vec::new(),
            name_rule:      0,
            matched_models: Vec::new(),
            matched_count:  0,
        };
        let upstream = UpstreamModel {
            description: "new".to_string(),
            vendor_name: "OpenAI".to_string(),
            ..UpstreamModel::default()
        };
        let fields = conflict_fields(&local, &upstream, Some(&"Other".to_string()));
        assert_eq!(fields.len(), 2);
    }
}
