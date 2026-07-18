use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use bytes::Bytes;
use certain_map::{Param, ParamRef, ParamSet};
use halolake_api_contract::{JsonValue, claude, gemini, openai};
use halolake_control_plane::{
    ChannelFeedbackAck, ChannelFeedbackBatch, ChannelFeedbackError, ChannelFeedbackEvent,
    ChannelFeedbackReason,
};
use halolake_domain::{UsageEvent, UsageStatus};
use halolake_protocol::{
    ClaudeSseTranslator, GeminiSseToOpenAiTranslator, OpenAiSseToClaudeTranslator,
    OpenAiSseToGeminiTranslator, claude_messages_to_openai_chat,
    claude_messages_to_openai_chat_request, gemini_imagen_to_openai_image_response,
    gemini_request_to_openai_chat, gemini_response_to_openai_chat, openai_chat_to_claude_messages,
    openai_chat_to_claude_messages_response, openai_chat_to_gemini_request,
    openai_chat_to_gemini_response, openai_image_to_gemini_imagen_request,
};
use halolake_router_core::{
    ChannelAffinityCache, ChannelAffinityCandidate, ChannelAffinityRequest, ChannelConfig,
    GatewaySnapshot, IndexedSnapshot, Provider, RouteError,
};
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, header,
};
use monoio::net::{ListenerOpts, TcpListener, TcpStream};
use monoio_http::{
    common::{
        body::{Body as MonoioBody, FixedBody, HttpBody, HttpBodyStream},
        error::HttpError,
    },
    h1::payload::{Payload, stream_payload_pair},
};
use monoio_transports::{
    connectors::{Connector, TcpConnector, TcpTlsAddr, TlsConnector, TlsStream},
    http::{HttpConnection, HttpConnector},
};
use serde::{Deserialize, Serialize};
use service_async::Service;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    convert::Infallible,
    error::Error,
    fs,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{Instrument, debug, error, info, warn};
use uuid::Uuid;

type BoxError = Box<dyn Error + Send + Sync>;
/// Downstream/upstream response body (native monoio-http).
pub type GatewayBody = HttpBody;
type HttpUpstream = HttpConnector<TcpConnector, SocketAddr, TcpStream>;
type HttpsUpstream = HttpConnector<TlsConnector<TcpConnector>, TcpTlsAddr, TlsStream<TcpStream>>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResponseUsage {
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: Option<u64>,
    pub(crate) total_tokens: Option<u64>,
    pub(crate) cache_read_tokens: Option<u64>,
    pub(crate) cache_creation_tokens: Option<u64>,
    pub(crate) image_tokens: Option<u64>,
    pub(crate) audio_tokens: Option<u64>,
}

impl ResponseUsage {
    pub(crate) fn from_openai(usage: openai::Usage) -> Self {
        let cache_read_tokens = usage
            .prompt_tokens_details
            .cached_tokens
            .max(usage.cached_tokens)
            .max(
                usage
                    .input_tokens_details
                    .map(|details| details.cached_tokens)
                    .unwrap_or(0),
            );
        let cache_creation_tokens = usage.prompt_tokens_details.cached_creation_tokens.max(
            usage
                .input_tokens_details
                .map(|details| details.cached_creation_tokens)
                .unwrap_or(0),
        );
        let image_tokens = usage.prompt_tokens_details.image_tokens.max(
            usage
                .input_tokens_details
                .map(|details| details.image_tokens)
                .unwrap_or(0),
        );
        let audio_tokens = usage.prompt_tokens_details.audio_tokens.max(
            usage
                .input_tokens_details
                .map(|details| details.audio_tokens)
                .unwrap_or(0),
        );
        Self {
            prompt_tokens: nonzero_u32(usage.prompt_tokens),
            completion_tokens: nonzero_u32(usage.completion_tokens),
            total_tokens: nonzero_u32(usage.total_tokens),
            cache_read_tokens: nonzero_u32(cache_read_tokens),
            cache_creation_tokens: nonzero_u32(cache_creation_tokens),
            image_tokens: nonzero_u32(image_tokens),
            audio_tokens: nonzero_u32(audio_tokens),
        }
    }

    pub(crate) fn from_claude(usage: claude::Usage) -> Self {
        let prompt_tokens = usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.cache_read_input_tokens);
        let total_tokens = prompt_tokens.saturating_add(usage.output_tokens);
        Self {
            prompt_tokens: nonzero_u32(prompt_tokens),
            completion_tokens: nonzero_u32(usage.output_tokens),
            total_tokens: nonzero_u32(total_tokens),
            cache_read_tokens: nonzero_u32(usage.cache_read_input_tokens),
            cache_creation_tokens: nonzero_u32(usage.cache_creation_input_tokens),
            image_tokens: None,
            audio_tokens: None,
        }
    }

    pub(crate) fn from_gemini(usage: gemini::GeminiUsageMetadata) -> Self {
        let prompt_tokens = usage
            .prompt_token_count
            .saturating_add(usage.cached_content_token_count);
        let completion_tokens = usage
            .candidates_token_count
            .saturating_add(usage.thoughts_token_count);
        let total_tokens = if usage.total_token_count > 0 {
            usage.total_token_count
        } else {
            prompt_tokens.saturating_add(completion_tokens)
        };
        Self {
            prompt_tokens: nonzero_u32(prompt_tokens),
            completion_tokens: nonzero_u32(completion_tokens),
            total_tokens: nonzero_u32(total_tokens),
            cache_read_tokens: nonzero_u32(usage.cached_content_token_count),
            cache_creation_tokens: None,
            image_tokens: None,
            audio_tokens: None,
        }
    }

    pub(crate) fn is_empty(self) -> bool {
        self.prompt_tokens.is_none()
            && self.completion_tokens.is_none()
            && self.total_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_creation_tokens.is_none()
            && self.image_tokens.is_none()
            && self.audio_tokens.is_none()
    }
}

fn nonzero_u32(value: u32) -> Option<u64> {
    (value > 0).then_some(value as u64)
}

mod config;
mod context;
mod control;
mod downstream;
mod gateway;
mod image;
mod relay;
mod request;
mod response;
mod services;
mod upstream_proxy;
mod util;

pub use config::{
    AuthConfig, ControlPlaneConfig, GatewayConfig, ProtocolConfig, ServerConfig, UpstreamConfig,
};
pub(crate) use context::*;
pub(crate) use control::*;
pub use gateway::{Gateway, run_from_config_file, serve};
pub(crate) use image::*;
pub(crate) use relay::*;
pub(crate) use request::*;
pub(crate) use response::*;
pub(crate) use services::*;
pub(crate) use util::*;
