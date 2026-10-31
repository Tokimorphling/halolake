use crate::{Format, Quality};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConverterSpec {
    pub id: &'static str,
    pub from: Format,
    pub to: Format,
    pub quality: Quality,
    pub response_alias: &'static str,
}
use Format::*;
use Quality::*;
pub const CONVERTERS: &[ConverterSpec] = &[
    ConverterSpec {
        id: "anthropic_messages_to_openai_chat_completions",
        from: Claude,
        to: OpenAi,
        quality: Fair,
        response_alias: "claude_messages_to_oai_chat_resp",
    },
    ConverterSpec {
        id: "openai_chat_completions_to_anthropic_messages",
        from: OpenAi,
        to: Claude,
        quality: Fair,
        response_alias: "oai_chat_to_claude_messages_resp",
    },
    ConverterSpec {
        id: "gemini_generate_content_to_openai_chat_completions",
        from: Gemini,
        to: OpenAi,
        quality: Fair,
        response_alias: "gemini_chat_to_oai_chat_resp",
    },
    ConverterSpec {
        id: "openai_chat_completions_to_gemini_generate_content",
        from: OpenAi,
        to: Gemini,
        quality: Fair,
        response_alias: "oai_chat_to_gemini_chat_resp",
    },
    ConverterSpec {
        id: "openai_chat_completions_to_openai_responses",
        from: OpenAi,
        to: OpenAiResponses,
        quality: Good,
        response_alias: "oai_chat_to_oai_responses_resp",
    },
    ConverterSpec {
        id: "openai_responses_to_openai_chat_completions",
        from: OpenAiResponses,
        to: OpenAi,
        quality: Good,
        response_alias: "oai_responses_to_oai_chat_resp",
    },
    ConverterSpec {
        id: "claude_messages_to_gemini_generate_content",
        from: Claude,
        to: Gemini,
        quality: Discouraged,
        response_alias: "claude_messages_to_gemini_chat_resp",
    },
    ConverterSpec {
        id: "claude_messages_to_openai_responses",
        from: Claude,
        to: OpenAiResponses,
        quality: Fair,
        response_alias: "claude_messages_to_oai_responses_resp",
    },
    ConverterSpec {
        id: "gemini_generate_content_to_claude_messages",
        from: Gemini,
        to: Claude,
        quality: Discouraged,
        response_alias: "gemini_chat_to_claude_messages_resp",
    },
    ConverterSpec {
        id: "gemini_generate_content_to_openai_responses",
        from: Gemini,
        to: OpenAiResponses,
        quality: Fair,
        response_alias: "gemini_chat_to_oai_responses_resp",
    },
    ConverterSpec {
        id: "openai_responses_to_claude_messages",
        from: OpenAiResponses,
        to: Claude,
        quality: Fair,
        response_alias: "oai_responses_to_claude_messages_resp",
    },
    ConverterSpec {
        id: "openai_responses_to_gemini_generate_content",
        from: OpenAiResponses,
        to: Gemini,
        quality: Fair,
        response_alias: "oai_responses_to_gemini_chat_resp",
    },
];
pub fn lookup_converter(id: &str) -> Option<&'static ConverterSpec> {
    let id = id.trim();
    CONVERTERS
        .iter()
        .find(|c| c.id == id || c.response_alias == id)
}
pub fn converter(from: Format, to: Format) -> Option<&'static ConverterSpec> {
    CONVERTERS.iter().find(|c| c.from == from && c.to == to)
}
pub fn conversion_path(from: Format, to: Format, response: bool) -> Vec<Format> {
    if from == to {
        return vec![from];
    }
    let via = if response {
        from != OpenAi && to != OpenAi && !(from == OpenAiResponses && to == Claude)
    } else {
        matches!(
            (from, to),
            (Claude, Gemini) | (Gemini, Claude) | (Gemini, OpenAiResponses)
        )
    };
    if via {
        vec![from, OpenAi, to]
    } else {
        vec![from, to]
    }
}
