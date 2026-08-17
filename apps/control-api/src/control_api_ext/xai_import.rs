//! Shared xAI/Grok import constants and header construction.

use serde_json::{Map as JsonMap, Value as JsonValue};

pub(super) const XAI_DEFAULT_API_BASE: &str = "https://api.x.ai/v1";
pub(super) const XAI_DEFAULT_CLI_CHAT_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
pub(super) const XAI_DEFAULT_MODELS: &str =
    "grok-build-0.1,grok-4.5,grok-4.3,grok-4.20-0309-reasoning,grok-4.20-0309-non-reasoning,\
     grok-4.20-multi-agent-0309,grok-3-mini,grok-3-mini-fast,grok-composer-2.5-fast";

pub(super) const XAI_TOKEN_AUTH_HEADER: &str = "X-XAI-Token-Auth";
pub(super) const XAI_TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
pub(super) const XAI_CLIENT_VERSION_HEADER: &str = "x-grok-client-version";
pub(super) const XAI_CLIENT_VERSION_VALUE: &str = "0.2.120";
pub(super) const XAI_USER_AGENT: &str = "xai-grok-workspace/0.2.120";
pub(super) const XAI_CLIENT_IDENTIFIER_HEADER: &str = "x-grok-client-identifier";
pub(super) const XAI_CLIENT_IDENTIFIER_VALUE: &str = "grok-shell";
pub(super) const XAI_AUTHENTICATE_RESPONSE_HEADER: &str = "x-authenticateresponse";
pub(super) const XAI_AUTHENTICATE_RESPONSE_VALUE: &str = "authenticate-response";

pub(super) fn insert_xai_cli_identity_headers(map: &mut JsonMap<String, JsonValue>) {
    for (name, value) in [
        (XAI_TOKEN_AUTH_HEADER, XAI_TOKEN_AUTH_VALUE),
        (XAI_CLIENT_VERSION_HEADER, XAI_CLIENT_VERSION_VALUE),
        ("User-Agent", XAI_USER_AGENT),
        (XAI_CLIENT_IDENTIFIER_HEADER, XAI_CLIENT_IDENTIFIER_VALUE),
        (
            XAI_AUTHENTICATE_RESPONSE_HEADER,
            XAI_AUTHENTICATE_RESPONSE_VALUE,
        ),
    ] {
        map.insert(name.into(), JsonValue::String(value.into()));
    }
}
