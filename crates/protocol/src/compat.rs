use crate::{
    PromptCacheMode, ProtocolError, RelaySseTranslator, ResponsesToolMapping,
    apply_claude_prompt_cache, convert_request_json, convert_response_json_with_options,
    responses_tool_mapping,
};
use halolake_api_contract::{JsonValue, claude, gemini, openai};
use relaykit::Format;
use serde::{Serialize, de::DeserializeOwned};

fn request<S: Serialize, T: DeserializeOwned>(
    from: Format,
    to: Format,
    input: &S,
    model: &str,
) -> Result<T, ProtocolError> {
    let (value, _) = convert_request_json(from, to, serde_json::to_value(input)?, model)?;
    Ok(serde_json::from_value(value)?)
}

fn response<S: Serialize, T: DeserializeOwned>(
    from: Format,
    to: Format,
    input: S,
    model: &str,
    include: bool,
    tools: &ResponsesToolMapping,
) -> T {
    let value = convert_response_json_with_options(
        from, to,
        serde_json::to_value(input).expect("protocol DTO serialization is infallible"),
        model, tools, include,
    )
    .expect("a typed successful response must be convertible");
    serde_json::from_value(value).expect("RelayKit output must match the destination DTO")
}

pub fn openai_chat_to_claude_messages(
    input: &openai::ChatCompletionRequest,
    model: impl Into<String>,
) -> Result<claude::MessagesRequest, ProtocolError> {
    if input.messages.is_empty() {
        return Err(ProtocolError::EmptyMessages);
    }
    request(Format::OpenAi, Format::Claude, input, &model.into())
}

pub fn claude_messages_to_openai_chat_request(
    input: &claude::MessagesRequest,
    model: impl Into<String>,
) -> Result<openai::ChatCompletionRequest, ProtocolError> {
    if input.messages.is_empty() {
        return Err(ProtocolError::EmptyMessages);
    }
    request(Format::Claude, Format::OpenAi, input, &model.into())
}

pub fn openai_chat_to_claude_messages_response(
    input: openai::ChatCompletionResponse,
    model: impl Into<String>,
) -> claude::MessagesResponse {
    response(
        Format::OpenAi, Format::Claude, input, &model.into(), true,
        &ResponsesToolMapping::default(),
    )
}

pub fn claude_messages_to_openai_chat(
    input: claude::MessagesResponse,
    model: impl Into<String>,
) -> openai::ChatCompletionResponse {
    response(
        Format::Claude, Format::OpenAi, input, &model.into(), true,
        &ResponsesToolMapping::default(),
    )
}

pub fn openai_chat_to_gemini_request(
    input: &openai::ChatCompletionRequest,
) -> Result<gemini::GeminiChatRequest, ProtocolError> {
    request(Format::OpenAi, Format::Gemini, input, &input.model)
}

pub fn gemini_request_to_openai_chat(
    input: &gemini::GeminiChatRequest,
    model: impl Into<String>,
) -> Result<openai::ChatCompletionRequest, ProtocolError> {
    request(Format::Gemini, Format::OpenAi, input, &model.into())
}

pub fn gemini_response_to_openai_chat(
    input: gemini::GeminiChatResponse,
    model: impl Into<String>,
) -> openai::ChatCompletionResponse {
    let mut output: openai::ChatCompletionResponse = response(
        Format::Gemini, Format::OpenAi, input, &model.into(), true,
        &ResponsesToolMapping::default(),
    );
    if let Some(usage) = output.usage.as_mut() {
        usage.cached_tokens = usage.prompt_tokens_details.cached_tokens;
    }
    output
}

pub fn openai_chat_to_gemini_response(
    input: openai::ChatCompletionResponse,
    model: impl Into<String>,
) -> gemini::GeminiChatResponse {
    response(
        Format::OpenAi, Format::Gemini, input, &model.into(), true,
        &ResponsesToolMapping::default(),
    )
}

pub fn openai_image_to_gemini_imagen_request(
    input: &openai::ImageRequest,
    model: &str,
) -> Result<gemini::GeminiImageRequest, ProtocolError> {
    if !model.starts_with("imagen") {
        return Err(ProtocolError::UnsupportedImageModel);
    }
    let mut value = serde_json::to_value(input)?;
    value["model"] = JsonValue::String(model.to_string());
    Ok(serde_json::from_value(
        relaykit::image::openai_image_to_gemini_imagen(&value)?,
    )?)
}

pub fn gemini_imagen_to_openai_image_response(
    input: gemini::GeminiImageResponse,
    created: u64,
) -> Result<openai::ImageResponse, ProtocolError> {
    let value = relaykit::image::gemini_imagen_to_openai_image(
        &serde_json::to_value(input)?,
        created.min(i64::MAX as u64) as i64,
    )
    .map_err(|error| {
        if error.to_string().contains("no images") {
            ProtocolError::NoImagesGenerated
        } else {
            error.into()
        }
    })?;
    Ok(serde_json::from_value(value)?)
}

pub fn openai_responses_to_claude_messages_with_tools_and_cache_mode(
    raw: &[u8],
    model: impl Into<String>,
    _tools: &ResponsesToolMapping,
    mode: PromptCacheMode,
) -> Result<claude::MessagesRequest, ProtocolError> {
    let original: JsonValue = serde_json::from_slice(raw)?;
    let (mut value, _) = convert_request_json(
        Format::OpenAiResponses, Format::Claude, original.clone(), &model.into(),
    )?;
    apply_claude_prompt_cache(&mut value, &original, mode);
    Ok(serde_json::from_value(value)?)
}

pub fn openai_responses_to_claude_messages_with_tools(
    raw: &[u8],
    model: impl Into<String>,
    tools: &ResponsesToolMapping,
) -> Result<claude::MessagesRequest, ProtocolError> {
    openai_responses_to_claude_messages_with_tools_and_cache_mode(
        raw, model, tools, PromptCacheMode::Explicit,
    )
}

pub fn openai_responses_to_claude_messages_with_cache_mode(
    raw: &[u8],
    model: impl Into<String>,
    mode: PromptCacheMode,
) -> Result<claude::MessagesRequest, ProtocolError> {
    openai_responses_to_claude_messages_with_tools_and_cache_mode(
        raw, model, &responses_tool_mapping(raw)?, mode,
    )
}

pub fn openai_responses_to_claude_messages(
    raw: &[u8],
    model: impl Into<String>,
) -> Result<claude::MessagesRequest, ProtocolError> {
    openai_responses_to_claude_messages_with_cache_mode(raw, model, PromptCacheMode::Explicit)
}

pub fn claude_messages_to_openai_responses_with_options_and_tools(
    input: claude::MessagesResponse,
    model: impl Into<String>,
    include: bool,
    tools: &ResponsesToolMapping,
) -> JsonValue {
    response(Format::Claude, Format::OpenAiResponses, input, &model.into(), include, tools)
}

pub fn claude_messages_to_openai_responses_with_options(
    input: claude::MessagesResponse,
    model: impl Into<String>,
    include: bool,
) -> JsonValue {
    claude_messages_to_openai_responses_with_options_and_tools(
        input, model, include, &ResponsesToolMapping::default(),
    )
}

pub fn claude_messages_to_openai_responses(
    input: claude::MessagesResponse,
    model: impl Into<String>,
) -> JsonValue {
    claude_messages_to_openai_responses_with_options(input, model, true)
}

struct StringSseTranslator {
    inner: RelaySseTranslator,
    chat_target: bool,
    done_sent: bool,
}

impl StringSseTranslator {
    fn new(from: Format, to: Format, model: impl Into<String>) -> Self {
        Self {
            inner: RelaySseTranslator::new(from, to, model, ResponsesToolMapping::default()),
            chat_target: to == Format::OpenAi,
            done_sent: false,
        }
    }
    fn translate(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
        let mut events = self.inner.translate_sse_payload(payload)?
            .into_iter()
            .map(|event| serde_json::to_string(&event).map_err(ProtocolError::from))
            .collect::<Result<Vec<_>, _>>()?;
        if self.chat_target && self.inner.is_complete() && !self.done_sent {
            events.push("[DONE]".to_string());
            self.done_sent = true;
        }
        Ok(events)
    }
    fn finish(&mut self) -> Result<Vec<String>, ProtocolError> {
        let mut events = if self.chat_target && self.inner.from_format() == Format::Claude {
            self.inner.finish_at_eof()?
        } else {
            self.inner.finish()?
        }
            .into_iter()
            .map(|event| serde_json::to_string(&event).map_err(ProtocolError::from))
            .collect::<Result<Vec<_>, _>>()?;
        if self.chat_target && self.inner.is_complete() && !self.done_sent {
            events.push("[DONE]".to_string());
            self.done_sent = true;
        }
        Ok(events)
    }
}

macro_rules! string_translator {
    ($name:ident, $from:ident, $to:ident) => {
        pub struct $name(StringSseTranslator);
        impl $name {
            pub fn new(model: impl Into<String>) -> Self {
                Self(StringSseTranslator::new(Format::$from, Format::$to, model))
            }
            pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
                self.0.translate(payload)
            }
            pub fn finish(&mut self) -> Result<Vec<String>, ProtocolError> {
                self.0.finish()
            }
        }
    };
}

string_translator!(ClaudeSseTranslator, Claude, OpenAi);
string_translator!(OpenAiSseToClaudeTranslator, OpenAi, Claude);
string_translator!(GeminiSseToOpenAiTranslator, Gemini, OpenAi);

pub struct OpenAiSseToGeminiTranslator(StringSseTranslator);
impl Default for OpenAiSseToGeminiTranslator {
    fn default() -> Self { Self::new() }
}
impl OpenAiSseToGeminiTranslator {
    pub fn new() -> Self {
        Self(StringSseTranslator::new(Format::OpenAi, Format::Gemini, ""))
    }
    pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<String>, ProtocolError> {
        self.0.translate(payload)
    }
    pub fn finish(&mut self) -> Result<Vec<String>, ProtocolError> { self.0.finish() }
}

pub struct ClaudeSseToResponsesTranslator(RelaySseTranslator);
impl ClaudeSseToResponsesTranslator {
    pub fn new(model: impl Into<String>) -> Self {
        Self::with_encrypted_reasoning(model, true)
    }
    pub fn with_encrypted_reasoning(model: impl Into<String>, include: bool) -> Self {
        Self::with_options(model, include, ResponsesToolMapping::default())
    }
    pub fn with_options(model: impl Into<String>, include: bool, tools: ResponsesToolMapping) -> Self {
        Self(RelaySseTranslator::new(Format::Claude, Format::OpenAiResponses, model, tools)
            .with_encrypted_reasoning(include))
    }
    pub fn translate_sse_payload(&mut self, payload: &str) -> Result<Vec<JsonValue>, ProtocolError> {
        self.0.translate_sse_payload(payload)
    }
    pub fn finish(&mut self) -> Vec<JsonValue> {
        match self.0.finish() {
            Ok(events) => events,
            Err(error) => self.0.fail("translation_error", error.to_string()),
        }
    }
    pub fn fail(&mut self, code: impl Into<String>, message: impl Into<String>) -> Vec<JsonValue> {
        self.0.fail(code, message)
    }
}
