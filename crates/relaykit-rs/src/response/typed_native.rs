//! Native typed inputs for the direct response views. Dynamic protocol fields
//! are bounded islands, while message/candidate/output lists remain structs.
use super::super::{
    typed_claude::{BorrowedString, coerce_go_numbers},
    typed_usage::normalize_existing_billing,
};
use crate::{Error, Format, dto};
use serde::{
    Deserialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, value::RawValue};
use std::{collections::BTreeMap, fmt, sync::OnceLock};

#[derive(Debug)]
pub(super) enum NativeInput {
    Chat(dto::OpenAITextResponse, bool),
    Responses(dto::OpenAIResponsesResponse),
    Gemini(GeminiInput),
}
impl NativeInput {
    pub(super) fn parse(format: Format, input: &[u8]) -> Result<Option<Self>, Error> {
        let name = match format {
            Format::OpenAi => "OpenAITextResponse",
            Format::OpenAiResponses => "OpenAIResponsesResponse",
            Format::Gemini => "GeminiChatResponse",
            Format::Claude => unreachable!(),
        };
        let raw: &RawValue = serde_json::from_slice(input)?;
        if !canonical(name, raw)? {
            return Ok(None);
        }
        if format == Format::OpenAiResponses
            && raw_field(raw, "type")?.is_some_and(|v| v.get().starts_with("\"response."))
        {
            return Ok(None);
        }
        let mut parsed = match format {
            Format::OpenAi => Self::Chat(
                serde_json::from_slice(input)?,
                raw_field(raw, "usage")?.is_some_and(|v| v.get() != "null"),
            ),
            Format::OpenAiResponses => Self::Responses(serde_json::from_slice(input)?),
            Format::Gemini => Self::Gemini(serde_json::from_slice(input)?),
            Format::Claude => unreachable!(),
        };
        let usage_name = if format == Format::Gemini {
            "usageMetadata"
        } else {
            "usage"
        };
        let usage = raw_field(raw, usage_name)?;
        if let Some(billing) = usage
            .filter(|raw| raw.get() != "null")
            .map(|raw| raw_field(raw, "billing_usage"))
            .transpose()?
            .flatten()
            .filter(|raw| raw.get() != "null")
        {
            let (normalized, compatibility) = normalize_existing_billing(billing)?;
            if compatibility {
                return Ok(None);
            }
            match &mut parsed {
                Self::Chat(source, _) => source.usage.billing_usage = Some(Box::new(normalized)),
                Self::Responses(source) => {
                    if let Some(usage) = &mut source.usage {
                        usage.billing_usage = Some(Box::new(normalized));
                    }
                }
                Self::Gemini(source) => {
                    if let Some(usage) = &mut source.usage_metadata {
                        usage.billing_usage = Some(Box::new(normalized));
                    }
                }
            }
        }
        match &mut parsed {
            Self::Chat(source, _) => {
                coerce_go_numbers(&mut source.created);
                coerce_go_numbers(&mut source.usage.cost);
                for choice in source.choices.as_mut().into_iter().flatten() {
                    coerce_go_numbers(&mut choice.message.content);
                }
            }
            Self::Responses(source) => {
                if let Some(usage) = &mut source.usage {
                    coerce_go_numbers(&mut usage.cost);
                }
            }
            Self::Gemini(source) => {
                for candidate in source.candidates.as_mut().into_iter().flatten() {
                    for part in candidate.content.parts.as_mut().into_iter().flatten() {
                        if let Some(call) = &mut part.function_call {
                            coerce_go_numbers(&mut call.args);
                        }
                    }
                }
            }
        }
        Ok(Some(parsed))
    }
    pub(super) fn has_hosted(&self) -> bool {
        match self {
            Self::Responses(source) => source.output.as_deref().unwrap_or(&[]).iter().any(|o| {
                !matches!(
                    o.r#type.trim(),
                    "" | "message" | "reasoning" | "function_call" | "custom_tool_call"
                )
            }),
            Self::Gemini(source) => source
                .candidates
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .any(|c| c.grounding_metadata.is_some()),
            _ => false,
        }
    }
}
fn null_default<'de, D: serde::Deserializer<'de>, T: Deserialize<'de> + Default>(
    d: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct GeminiInput {
    pub candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "promptFeedback")]
    pub prompt_feedback: Option<dto::GeminiChatPromptFeedback>,
    #[serde(rename = "usageMetadata")]
    pub usage_metadata: Option<dto::GeminiUsageMetadata>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct GeminiCandidate {
    #[serde(deserialize_with = "null_default")]
    pub content: GeminiContent,
    #[serde(rename = "finishReason")]
    pub finish_reason: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub index: i64,
    #[serde(rename = "safetyRatings")]
    pub safety_ratings: Option<Vec<dto::GeminiChatSafetyRating>>,
    #[serde(rename = "groundingMetadata")]
    pub grounding_metadata: Option<dto::GeminiGroundingMetadata>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct GeminiContent {
    #[serde(deserialize_with = "null_default")]
    pub role: String,
    pub parts: Option<Vec<GeminiPart>>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct GeminiPart {
    #[serde(deserialize_with = "null_default")]
    pub text: String,
    #[serde(deserialize_with = "null_default")]
    pub thought: bool,
    #[serde(rename = "inlineData")]
    pub inline_data: Option<dto::GeminiInlineData>,
    #[serde(rename = "functionCall")]
    pub function_call: Option<GeminiCall>,
    #[serde(rename = "thoughtSignature")]
    pub thought_signature: Option<Value>,
    #[serde(rename = "functionResponse")]
    pub function_response: Option<dto::GeminiFunctionResponse>,
    #[serde(rename = "mediaResolution")]
    pub media_resolution: Option<Value>,
    #[serde(rename = "videoMetadata")]
    pub video_metadata: Option<Value>,
    #[serde(rename = "fileData")]
    pub file_data: Option<dto::GeminiFileData>,
    #[serde(rename = "executableCode")]
    pub executable_code: Option<dto::GeminiPartExecutableCode>,
    #[serde(rename = "codeExecutionResult")]
    pub code_execution_result: Option<dto::GeminiPartCodeExecutionResult>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct GeminiCall {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub name: String,
    pub args: Value,
    #[serde(rename = "partialArgs")]
    pub partial_args: Option<Vec<dto::GeminiPartialArg>>,
    #[serde(rename = "willContinue")]
    pub will_continue: Option<bool>,
}

#[derive(Deserialize)]
struct SchemaField {
    #[serde(default)]
    json: String,
    #[serde(rename = "type", default)]
    ty: String,
    #[serde(default)]
    embed: String,
}
fn schema() -> &'static BTreeMap<String, Vec<SchemaField>> {
    static SCHEMA: OnceLock<BTreeMap<String, Vec<SchemaField>>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::from_str(include_str!("../dto_schema.json")).expect("bundled schema")
    })
}
// Inspect only known struct keys. RawMessage/any islands deliberately keep their
// own vocabulary and duplicate-key behavior. No dynamic response map is built.
fn canonical(ty: &str, raw: &RawValue) -> Result<bool, Error> {
    if raw.get() == "null" {
        return Ok(true);
    }
    if let Some(ty) = ty.strip_prefix('*') {
        return canonical(ty, raw);
    }
    if let Some(ty) = ty.strip_prefix("[]") {
        struct Sequence<'a>(&'a str);
        impl<'de> DeserializeSeed<'de> for Sequence<'_> {
            type Value = bool;
            fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
                struct V<'a>(&'a str);
                impl<'de> Visitor<'de> for V<'_> {
                    type Value = bool;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("a protocol array")
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
                        let mut valid = true;
                        while let Some(raw) = seq.next_element::<&RawValue>()? {
                            valid &= canonical(self.0, raw).map_err(de::Error::custom)?;
                        }
                        Ok(valid)
                    }
                }
                d.deserialize_seq(V(self.0))
            }
        }
        return Ok(Sequence(ty).deserialize(&mut serde_json::Deserializer::from_str(raw.get()))?);
    }
    let Some(fields) = schema().get(ty) else {
        return Ok(true);
    };
    if fields.iter().any(|f| !f.embed.is_empty()) {
        return Ok(false);
    }
    let aliases: &[&str] = match ty {
        "GeminiPart" => &["inline_data"],
        "GeminiInlineData" => &["mime_type"],
        _ => &[],
    };
    struct Object<'a>(&'a [SchemaField], &'a [&'a str]);
    impl<'de> DeserializeSeed<'de> for Object<'_> {
        type Value = bool;
        fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
            struct V<'a>(&'a [SchemaField], &'a [&'a str]);
            impl<'de> Visitor<'de> for V<'_> {
                type Value = bool;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a protocol object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
                    let mut valid = true;
                    let mut seen = 0u128;
                    while let Some(BorrowedString(key)) = map.next_key::<BorrowedString<'de>>()? {
                        let raw = map.next_value::<&RawValue>()?;
                        if self.1.contains(&key.as_ref()) {
                            valid = false;
                        }
                        if let Some(index) = self.0.iter().position(|f| f.json == key) {
                            if index >= 128 {
                                valid = false;
                                continue;
                            }
                            let bit = 1u128 << index;
                            valid &= seen & bit == 0;
                            seen |= bit;
                            valid &=
                                canonical(&self.0[index].ty, raw).map_err(de::Error::custom)?;
                        } else if !key.is_ascii()
                            || self.0.iter().any(|f| f.json.eq_ignore_ascii_case(&key))
                        {
                            valid = false;
                        }
                    }
                    Ok(valid)
                }
            }
            d.deserialize_map(V(self.0, self.1))
        }
    }
    Ok(Object(fields, aliases).deserialize(&mut serde_json::Deserializer::from_str(raw.get()))?)
}
fn raw_field<'a>(raw: &'a RawValue, name: &str) -> Result<Option<&'a RawValue>, Error> {
    struct Field<'a>(&'a str);
    impl<'de> DeserializeSeed<'de> for Field<'_> {
        type Value = Option<&'de RawValue>;
        fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            struct V<'a>(&'a str);
            impl<'de> Visitor<'de> for V<'_> {
                type Value = Option<&'de RawValue>;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a protocol object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut found = None;
                    while let Some(BorrowedString(key)) = map.next_key::<BorrowedString<'de>>()? {
                        let raw = map.next_value::<&RawValue>()?;
                        if key == self.0 {
                            found = Some(raw);
                        }
                    }
                    Ok(found)
                }
            }
            d.deserialize_map(V(self.0))
        }
    }
    Ok(Field(name).deserialize(&mut serde_json::Deserializer::from_str(raw.get()))?)
}
