//! Pure protocol conversion for OpenAI Chat Completions, Responses, Anthropic
//! Messages and Gemini. No HTTP client, executor, or global host configuration.
//!
//! ```
//! use relaykit::{convert_request, Context, Format, Request};
//! use serde_json::json;
//! let input = Request::from_value(Format::OpenAi, json!({
//!     "model": "claude-sonnet-4-5", "max_tokens": 1024,
//!     "messages": [{"role":"user", "content":"Hello!"}]
//! }))?;
//! let converted = convert_request(&Context::default(), Format::Claude, &input)?;
//! assert_eq!(converted.value.as_value()["messages"][0]["role"], "user");
//! # Ok::<(), relaykit::Error>(())
//! ```
mod core;
mod json;
pub use core::*;
pub mod dto;
mod dto_values;
pub mod media;
pub mod image;
pub mod reasoning;
pub mod reasonmap;
mod request;
pub use request::responses_tool_state;
mod response;
pub use request::typed::{
    PreparedRequest, PreparedRequestResult, RequestView, RequestWriteResult,
    convert_prepared_request, convert_request_to_writer,
};
pub use response::typed::{
    PreparedResponse, PreparedResponseResult, ResponseUsage, ResponseView, ResponseWriteResult,
    convert_prepared_response, convert_response_to_writer,
};
pub use response::typed_usage::CanonicalUsage as CanonicalResponseUsage;
pub mod stream;
pub mod wire;
pub use stream::{PreparedStreamChunk, ResponseStreamState, StreamOptions};
mod registry;
pub use registry::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A validated request paired with its protocol. Use a DTO's `TryFrom`
/// implementation, or `from_value` when handling dynamically decoded JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    format: Format,
    value: Value,
}
impl Request {
    pub fn from_value(format: Format, value: Value) -> Result<Self, Error> {
        if !value.is_object() {
            return Err(Error::message("request must be a JSON object"));
        }
        wire::validate_request(format, &value)?;
        Ok(Self { format, value })
    }
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn as_value(&self) -> &Value {
        &self.value
    }
    pub fn into_value(self) -> Value {
        self.value
    }
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T, Error> {
        Ok(serde_json::from_value(self.value.clone())?)
    }
}
impl Serialize for Request {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(s)
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    format: Format,
    value: Value,
}
impl Response {
    pub fn from_value(format: Format, value: Value) -> Result<Self, Error> {
        if !value.is_object() {
            return Err(Error::message("response must be a JSON object"));
        }
        wire::validate_response(format, &value)?;
        Ok(Self { format, value })
    }
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn as_value(&self) -> &Value {
        &self.value
    }
    pub fn into_value(self) -> Value {
        self.value
    }
    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T, Error> {
        Ok(serde_json::from_value(self.value.clone())?)
    }
}
impl Serialize for Response {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(s)
    }
}
macro_rules! dto_input {
    ($target:ident,$dto:ident,$format:ident) => {
        impl TryFrom<dto::$dto> for $target {
            type Error = Error;
            fn try_from(v: dto::$dto) -> Result<Self, Error> {
                Self::from_value(Format::$format, serde_json::to_value(v)?)
            }
        }
    };
}
dto_input!(Request, GeneralOpenAIRequest, OpenAi);
dto_input!(Request, OpenAIResponsesRequest, OpenAiResponses);
dto_input!(Request, ClaudeRequest, Claude);
dto_input!(Request, GeminiChatRequest, Gemini);
dto_input!(Response, OpenAITextResponse, OpenAi);
dto_input!(Response, OpenAIResponsesResponse, OpenAiResponses);
dto_input!(Response, ClaudeResponse, Claude);
dto_input!(Response, GeminiChatResponse, Gemini);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionStep {
    pub converter: String,
    pub from: Format,
    pub to: Format,
}
#[derive(Debug, Clone, Serialize)]
pub struct ConversionResult<T> {
    pub value: T,
    pub from: Format,
    pub to: Format,
    pub converter: String,
    pub quality: Option<Quality>,
    pub steps: Vec<ConversionStep>,
    pub diagnostics: Vec<Diagnostic>,
    pub usage: Option<Value>,
    pub stream: bool,
}
pub type RequestResult = ConversionResult<Request>;
pub type ResponseResult = ConversionResult<Response>;

pub fn convert_request(
    ctx: &Context,
    target: Format,
    input: &Request,
) -> Result<RequestResult, Error> {
    let from = input.format;
    let (value, diagnostics) = request::convert(from, target, &input.value, ctx)?;
    let preserved = ctx.options.preserve_message_structure.then(|| value.clone());
    let mut value = if from == target {
        wire::normalize_request_owned(target, value)?
    } else {
        wire::normalize_converted_request_owned(target, value)?
    };
    if let Some(preserved) = preserved {
        request::preserve_request_extensions(target, &preserved, &mut value);
    }
    let result = make_result(
        Request {
            format: target,
            value,
        },
        from,
        target,
        false,
        diagnostics,
        None,
    );
    {
        let mut chain = ctx
            .conversion_chain
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for step in &result.steps {
            if chain.last() != Some(&step.to) {
                chain.push(step.to)
            }
        }
    }
    Ok(result)
}
pub fn convert_request_by_id(
    ctx: &Context,
    id: &str,
    input: &Request,
) -> Result<RequestResult, Error> {
    let spec = lookup_converter(id)
        .filter(|spec| spec.id == id.trim())
        .ok_or_else(|| Error::message(format!("request converter {id:?} is not registered")))?;
    if spec.from != input.format {
        return Err(Error::message(format!(
            "request converter {:?} expects {} request, got {}",
            spec.id, spec.from, input.format
        )));
    }
    convert_request(ctx, spec.to, input)
}
pub fn convert_request_via(
    ctx: &Context,
    input: &Request,
    path: &[Format],
) -> Result<RequestResult, Error> {
    if path.is_empty() {
        return Err(Error::message("request conversion path is required"));
    }
    let targets = if path[0] == input.format {
        &path[1..]
    } else {
        path
    };
    let mut current = input.format;
    for &to in targets {
        if conversion_path(current, to, false).len() != 2 || converter(current, to).is_none() {
            return Err(Error::message(format!(
                "request direct converter from {current} to {to} is not registered"
            )));
        }
        current = to;
    }
    let target = targets.last().copied().unwrap_or(input.format);
    let (value, diagnostics) = request::convert_via(input.format, targets, &input.value, ctx)?;
    let preserved = ctx.options.preserve_message_structure.then(|| value.clone());
    let mut value = if targets.is_empty() {
        wire::normalize_request_owned(target, value)?
    } else {
        wire::normalize_converted_request_owned(target, value)?
    };
    if let Some(preserved) = preserved {
        request::preserve_request_extensions(target, &preserved, &mut value);
    }
    let mut result = make_result(
        Request {
            format: target,
            value,
        },
        input.format,
        target,
        false,
        diagnostics,
        None,
    );
    result.steps.clear();
    let mut current = input.format;
    for &to in targets {
        let spec = converter(current, to)
            .ok_or_else(|| Error::message("request direct route not registered"))?;
        result.steps.push(ConversionStep {
            converter: spec.id.into(),
            from: current,
            to,
        });
        current = to;
    }
    result.converter = result
        .steps
        .iter()
        .map(|s| s.converter.as_str())
        .collect::<Vec<_>>()
        .join(",");
    result.quality = None;
    let mut chain = ctx
        .conversion_chain
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for step in &result.steps {
        if chain.last() != Some(&step.to) {
            chain.push(step.to)
        }
    }
    Ok(result)
}
pub fn convert_response(
    ctx: &Context,
    target: Format,
    input: &Response,
) -> Result<ResponseResult, Error> {
    let from = input.format;
    let (value, diagnostics, usage) = response::convert(from, target, &input.value, ctx)?;
    let mut result = make_result(
        Response {
            format: target,
            value: if ctx.options.preserve_message_structure {
                value
            } else if from == target {
                wire::normalize_response_owned(target, value)?
            } else {
                wire::normalize_converted_response_owned(target, value)?
            },
        },
        from,
        target,
        true,
        diagnostics,
        usage,
    );
    if ctx.options.preserve_message_structure
        && from == Format::Claude
        && target == Format::OpenAiResponses
    {
        result.steps = vec![ConversionStep {
            converter: converter(from, target).unwrap().id.to_string(),
            from,
            to: target,
        }];
    }
    Ok(result)
}
pub fn convert_response_by_id(
    ctx: &Context,
    id: &str,
    input: &Response,
) -> Result<ResponseResult, Error> {
    let spec = lookup_converter(id)
        .ok_or_else(|| Error::message(format!("response converter {id:?} is not registered")))?;
    if spec.from != input.format {
        return Err(Error::message(format!(
            "response converter {:?} expects {} response, got {}",
            spec.id, spec.from, input.format
        )));
    }
    convert_response(ctx, spec.to, input)
}
fn make_result<T>(
    value: T,
    from: Format,
    to: Format,
    response: bool,
    diagnostics: Vec<Diagnostic>,
    usage: Option<Value>,
) -> ConversionResult<T> {
    let spec = converter(from, to);
    let steps = conversion_path(from, to, response)
        .windows(2)
        .map(|s| ConversionStep {
            converter: converter(s[0], s[1]).unwrap().id.into(),
            from: s[0],
            to: s[1],
        })
        .collect();
    ConversionResult {
        value,
        from,
        to,
        converter: spec.map(|s| s.id.into()).unwrap_or_default(),
        quality: spec.map(|s| s.quality),
        steps,
        diagnostics,
        usage,
        stream: false,
    }
}
