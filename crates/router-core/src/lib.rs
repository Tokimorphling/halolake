use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewaySnapshot {
    #[serde(default = "default_version")]
    pub version: u64,
    #[serde(default)]
    pub tokens: Vec<TokenConfig>,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub model_mappings: Vec<ModelMapping>,
}

impl GatewaySnapshot {
    pub fn index(self) -> Result<IndexedSnapshot, SnapshotError> {
        IndexedSnapshot::try_from(self)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenConfig {
    #[serde(default)]
    pub id: String,
    pub token: String,
    pub user_id: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelConfig {
    pub id: String,
    pub provider: Provider,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    OpenAi,
    Gemini,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelMapping {
    pub requested_model: String,
    pub channel_id: String,
    pub upstream_model: String,
}

#[derive(Debug, Clone)]
pub struct IndexedSnapshot {
    version: u64,
    tokens: HashMap<String, TokenConfig>,
    channels: HashMap<String, ChannelConfig>,
    mappings: HashMap<String, ModelMapping>,
}

impl IndexedSnapshot {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn authenticate(&self, bearer: &str) -> Result<AuthContext<'_>, RouteError> {
        let token = self.tokens.get(bearer).ok_or(RouteError::Unauthorized)?;
        if !token.enabled {
            return Err(RouteError::Unauthorized);
        }
        Ok(AuthContext { token })
    }

    pub fn route<'a>(
        &'a self,
        auth: &AuthContext<'a>,
        requested_model: &'a str,
    ) -> Result<RouteDecision<'a>, RouteError> {
        if !auth.token.allowed_models.is_empty()
            && !auth
                .token
                .allowed_models
                .iter()
                .any(|model| model == requested_model)
        {
            return Err(RouteError::ModelForbidden);
        }

        let mapping = self
            .mappings
            .get(requested_model)
            .ok_or(RouteError::ModelNotFound)?;
        let channel = self
            .channels
            .get(&mapping.channel_id)
            .ok_or(RouteError::ChannelNotFound)?;
        if !channel.enabled {
            return Err(RouteError::ChannelDisabled);
        }
        if !channel.models.is_empty()
            && !channel
                .models
                .iter()
                .any(|model| model == &mapping.upstream_model)
        {
            return Err(RouteError::ChannelModelMismatch);
        }

        Ok(RouteDecision {
            user_id: &auth.token.user_id,
            channel,
            requested_model,
            upstream_model: &mapping.upstream_model,
        })
    }
}

impl TryFrom<GatewaySnapshot> for IndexedSnapshot {
    type Error = SnapshotError;

    fn try_from(snapshot: GatewaySnapshot) -> Result<Self, Self::Error> {
        let tokens = snapshot
            .tokens
            .into_iter()
            .map(|mut token| {
                if token.id.is_empty() {
                    token.id = token.user_id.clone();
                }
                (token.token.clone(), token)
            })
            .collect();
        let channels = snapshot
            .channels
            .into_iter()
            .map(|channel| (channel.id.clone(), channel))
            .collect();
        let mappings = snapshot
            .model_mappings
            .into_iter()
            .map(|mapping| (mapping.requested_model.clone(), mapping))
            .collect();

        Ok(Self {
            version: snapshot.version,
            tokens,
            channels,
            mappings,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AuthContext<'a> {
    pub token: &'a TokenConfig,
}

#[derive(Debug, Clone, Copy)]
pub struct RouteDecision<'a> {
    pub user_id: &'a str,
    pub channel: &'a ChannelConfig,
    pub requested_model: &'a str,
    pub upstream_model: &'a str,
}

#[derive(Debug, Error)]
pub enum SnapshotError {}

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("model is not allowed for this token")]
    ModelForbidden,
    #[error("model is not configured")]
    ModelNotFound,
    #[error("channel is not configured")]
    ChannelNotFound,
    #[error("channel is disabled")]
    ChannelDisabled,
    #[error("channel does not serve mapped upstream model")]
    ChannelModelMismatch,
}

fn default_version() -> u64 {
    1
}

fn default_weight() -> u32 {
    1
}
