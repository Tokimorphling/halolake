//! Optional control-api extensions (admin-extras).
//!
//! Credential import / OpenAI-Codex OAuth helpers live here so core channel CRUD
//! stays thin. Mounted via [`mount`] from `admin_extras`.

mod auth_import;
mod codex_auth_import;
mod http;
mod openai_oauth;
mod sub2api_data_import;

pub(crate) use auth_import::{AuthImportRequest, import_auth};
pub(crate) use codex_auth_import::{
    CHANNEL_TYPE_CODEX, CodexAuthImportItem, CodexAuthImportMessage, CodexAuthImportRequest,
    CodexAuthImportResult, codex_key_to_json, collect_entries, find_existing_channel_id,
    merge_codex_oauth_key, parse_flexible_codex_key,
};
pub(crate) use http::mount;
pub(crate) use openai_oauth::AuthMethod;
pub(crate) use sub2api_data_import::{
    Sub2apiDataImportRequest, import_sub2api_data as run_sub2api_data_import,
};
