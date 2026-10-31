//! Typed Claude accounting for the direct response writer. Provider snapshots
//! stay independent of display usage; the common path never builds a Value tree.
use crate::{Error, dto};
use serde::{Deserialize, Serialize, de};
use serde_json::{Value, value::RawValue};
use std::{fmt, marker::PhantomData, sync::Arc};

#[derive(Clone, Copy)]
struct Field {
    index: Option<usize>,
    canonical: bool,
}
struct FieldSeed(&'static [&'static str]);
impl<'de> de::DeserializeSeed<'de> for FieldSeed {
    type Value = Field;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Field, D::Error> {
        struct Visitor(&'static [&'static str]);
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Field;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a usage field name")
            }
            fn visit_str<E: de::Error>(self, name: &str) -> Result<Field, E> {
                if let Some(index) = self.0.iter().position(|key| *key == name) {
                    return Ok(Field {
                        index: Some(index),
                        canonical: true,
                    });
                }
                Ok(Field {
                    index: self.0.iter().position(|key| key.eq_ignore_ascii_case(name)),
                    canonical: false,
                })
            }
        }
        d.deserialize_identifier(Visitor(self.0))
    }
}
fn seen_field(seen: &mut u16, field: Field, compatibility: &mut bool) {
    if let Some(index) = field.index {
        let bit = 1u16 << index;
        *compatibility |= !field.canonical || *seen & bit != 0;
        *seen |= bit;
    }
}
fn zero(v: &i64) -> bool {
    *v == 0
}

macro_rules! scalar_usage {
    ($name:ident, $(($field:ident, $key:literal)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Default, Serialize)]
        pub(crate) struct $name {
            $(#[serde(rename = $key, skip_serializing_if = "zero")]
              pub(crate) $field: i64,)+
            #[serde(skip)]
            compatibility: bool,
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> de::Visitor<'de> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str(stringify!($name))
                    }
                    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<$name, M::Error> {
                        const KEYS: &[&str] = &[$($key),+];
                        let mut value = $name::default();
                        let mut seen = 0;
                        while let Some(field) = map.next_key_seed(FieldSeed(KEYS))? {
                            seen_field(&mut seen, field, &mut value.compatibility);
                            match field.index.map(|i| KEYS[i]) {
                                $(Some($key) => value.$field = map.next_value::<Option<i64>>()?.unwrap_or_default(),)+
                                _ => { map.next_value::<de::IgnoredAny>()?; }
                            }
                        }
                        Ok(value)
                    }
                }
                d.deserialize_struct(stringify!($name), &[$($key),+], Visitor)
            }
        }
    };
}
scalar_usage!(
    CacheCreation,
    (tokens_5m, "ephemeral_5m_input_tokens"),
    (tokens_1h, "ephemeral_1h_input_tokens"),
);
scalar_usage!(
    ServerToolUse,
    (web_search_requests, "web_search_requests"),
    (web_fetch_requests, "web_fetch_requests"),
    (code_execution_requests, "code_execution_requests"),
    (tool_search_requests, "tool_search_requests"),
);

/// Native usage read once with the prepared Claude response.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClaudeUsage<'de> {
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) cache_creation_input_tokens: i64,
    pub(crate) cache_read_input_tokens: i64,
    pub(crate) cache_creation_5m_tokens: i64,
    pub(crate) cache_creation_1h_tokens: i64,
    pub(crate) cache_creation: Option<CacheCreation>,
    pub(crate) server_tool_use: Option<ServerToolUse>,
    billing: Option<Arc<dto::BillingUsage>>,
    // Retains the input lifetime without owning a second copy of this island.
    billing_raw: Option<&'de RawValue>,
    compatibility: bool,
}
impl ClaudeUsage<'_> {
    pub(crate) fn needs_compatibility(&self) -> bool {
        self.compatibility
            || self.cache_creation.is_some_and(|v| v.compatibility)
            || self.server_tool_use.is_some_and(|v| v.compatibility)
    }
    fn has_tokens(&self) -> bool {
        self.input_tokens != 0
            || self.output_tokens != 0
            || self.cache_creation_input_tokens != 0
            || self.cache_read_input_tokens != 0
            || self.cache_creation_5m_tokens != 0
            || self.cache_creation_1h_tokens != 0
            || self
                .cache_creation
                .is_some_and(|v| v.tokens_5m != 0 || v.tokens_1h != 0)
    }
}
impl<'de: 'a, 'a> Deserialize<'de> for ClaudeUsage<'a> {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> de::Visitor<'de> for Visitor<'a> {
            type Value = ClaudeUsage<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("Claude usage")
            }
            fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                const KEYS: &[&str] = &[
                    "input_tokens",
                    "output_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                    "claude_cache_creation_5_m_tokens",
                    "claude_cache_creation_1_h_tokens",
                    "cache_creation",
                    "server_tool_use",
                    "billing_usage",
                ];
                let mut value = ClaudeUsage::default();
                let mut seen = 0;
                while let Some(field) = map.next_key_seed(FieldSeed(KEYS))? {
                    seen_field(&mut seen, field, &mut value.compatibility);
                    match field.index {
                        Some(0) => {
                            value.input_tokens =
                                map.next_value::<Option<i64>>()?.unwrap_or_default()
                        }
                        Some(1) => {
                            value.output_tokens =
                                map.next_value::<Option<i64>>()?.unwrap_or_default()
                        }
                        Some(2) => {
                            value.cache_creation_input_tokens =
                                map.next_value::<Option<i64>>()?.unwrap_or_default()
                        }
                        Some(3) => {
                            value.cache_read_input_tokens =
                                map.next_value::<Option<i64>>()?.unwrap_or_default()
                        }
                        Some(4) => {
                            value.cache_creation_5m_tokens =
                                map.next_value::<Option<i64>>()?.unwrap_or_default()
                        }
                        Some(5) => {
                            value.cache_creation_1h_tokens =
                                map.next_value::<Option<i64>>()?.unwrap_or_default()
                        }
                        Some(6) => value.cache_creation = map.next_value()?,
                        Some(7) => value.server_tool_use = map.next_value()?,
                        Some(8) => {
                            value.billing_raw = map.next_value::<Option<&'de RawValue>>()?;
                            if let Some(raw) = value.billing_raw {
                                let (billing, compatibility) =
                                    normalize_existing_billing(raw).map_err(de::Error::custom)?;
                                value.billing = Some(Arc::new(billing));
                                value.compatibility |= compatibility;
                            } else {
                                value.billing = None;
                            }
                        }
                        _ => {
                            map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(value)
            }
        }
        d.deserialize_struct("ClaudeUsage", &[], Visitor(PhantomData))
    }
}

// Existing heterogeneous sidecars are uncommon, and their authoritative any
// fields must keep the established normalization and numeric rules. This island
// is prepared once, not rebuilt each time the body or metadata is serialized.
pub(crate) fn normalize_existing_billing(
    raw: &RawValue,
) -> Result<(dto::BillingUsage, bool), Error> {
    let value: Value = serde_json::from_str(raw.get())?;
    let validated = crate::wire::normalize_named("BillingUsage", &value)?;
    fn clone_billing(value: Value) -> Value {
        let source = super::owned_object([("billing_usage", value)]);
        let mut normalized = super::usage::normalize_usage(&source);
        normalized
            .as_object_mut()
            .unwrap()
            .remove("billing_usage")
            .unwrap()
    }
    let original = clone_billing(value);
    let canonical = clone_billing(validated);
    let compatibility = original != canonical
        || serde_json::to_string(&original)? != serde_json::to_string(&canonical)?;
    Ok((serde_json::from_value(original)?, compatibility))
}

#[derive(Clone, Debug, Serialize)]
struct NativeClaudeSnapshot<'a> {
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    claude_cache_creation_5_m_tokens: i64,
    claude_cache_creation_1_h_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_creation: Option<&'a CacheCreation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_tool_use: Option<&'a ServerToolUse>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum Billing<'a> {
    ConvertedChat {
        source: &'static str,
        semantic: &'static str,
        openai_usage: Box<CanonicalUsage<'a>>,
    },
    OpenAi {
        source: &'static str,
        semantic: &'static str,
        openai_usage: NativeOpenAiSnapshot<'a>,
    },
    Gemini {
        source: &'static str,
        semantic: &'static str,
        gemini_usage_metadata: NativeGeminiSnapshot<'a>,
    },
    Existing(&'a dto::BillingUsage),
    Claude {
        source: &'static str,
        semantic: &'static str,
        claude_usage: NativeClaudeSnapshot<'a>,
    },
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
struct InputDetails<'a> {
    cached_tokens: i64,
    text_tokens: i64,
    audio_tokens: i64,
    image_tokens: i64,
    #[serde(skip_serializing_if = "zero")]
    cached_creation_tokens: i64,
    #[serde(skip_serializing_if = "zero")]
    cache_write_tokens: i64,
    #[serde(skip_serializing_if = "zero")]
    cache_creation_input_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_tokens_details: Option<&'a dto::CachedTokenDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write_tokens_details: Option<&'a dto::CacheWriteTokenDetails>,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
struct OutputDetails {
    text_tokens: i64,
    audio_tokens: i64,
    image_tokens: i64,
    reasoning_tokens: i64,
}

/// Shared accounting shape for a typed response. Cloning copies counters and
/// references, including an authoritative already-prepared billing snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct CanonicalUsage<'a> {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    prompt_tokens_details: InputDetails<'a>,
    completion_tokens_details: OutputDetails,
    pub input_tokens: i64,
    pub output_tokens: i64,
    input_tokens_details: Option<InputDetails<'a>>,
    pub claude_cache_creation_5_m_tokens: i64,
    pub claude_cache_creation_1_h_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_semantic: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_source: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    billing_usage: Option<Billing<'a>>,
    #[serde(skip_serializing_if = "zero")]
    prompt_cache_hit_tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens_details: Option<OutputDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<&'a Value>,
}
impl<'a> CanonicalUsage<'a> {
    pub(crate) fn from_claude(source: Option<&'a ClaudeUsage<'_>>) -> Self {
        let mut output = Self {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            prompt_tokens_details: InputDetails::default(),
            completion_tokens_details: OutputDetails::default(),
            input_tokens: 0,
            output_tokens: 0,
            input_tokens_details: None,
            claude_cache_creation_5_m_tokens: 0,
            claude_cache_creation_1_h_tokens: 0,
            usage_semantic: None,
            usage_source: None,
            billing_usage: None,
            prompt_cache_hit_tokens: 0,
            output_tokens_details: None,
            cost: None,
        };
        let Some(source) = source else {
            return output;
        };
        let creation = source.cache_creation.unwrap_or_default();
        let split = creation.tokens_5m.wrapping_add(creation.tokens_1h);
        let writes = source.cache_creation_input_tokens.max(split);
        let prompt = source
            .input_tokens
            .wrapping_add(source.cache_read_input_tokens)
            .wrapping_add(writes);
        output.prompt_tokens = prompt;
        output.input_tokens = prompt;
        output.completion_tokens = source.output_tokens;
        output.total_tokens = prompt.wrapping_add(source.output_tokens);
        // Go's Claude bridge leaves output_tokens unset in its shared Usage.
        output.prompt_tokens_details.cached_tokens = source.cache_read_input_tokens;
        output.prompt_tokens_details.cached_creation_tokens = source.cache_creation_input_tokens;
        output.prompt_tokens_details.cache_write_tokens = writes;
        output.claude_cache_creation_5_m_tokens = creation.tokens_5m.wrapping_add(
            source
                .cache_creation_input_tokens
                .wrapping_sub(split)
                .max(0),
        );
        output.claude_cache_creation_1_h_tokens = creation.tokens_1h;
        output.usage_semantic = Some("openai");
        output.usage_source = Some("anthropic");
        output.billing_usage = if let Some(billing) = &source.billing {
            Some(Billing::Existing(billing.as_ref()))
        } else if source.has_tokens() {
            Some(Billing::Claude {
                source: "claude_messages",
                semantic: "anthropic",
                claude_usage: NativeClaudeSnapshot {
                    input_tokens: source.input_tokens,
                    output_tokens: source.output_tokens,
                    cache_creation_input_tokens: source.cache_creation_input_tokens,
                    cache_read_input_tokens: source.cache_read_input_tokens,
                    claude_cache_creation_5_m_tokens: source.cache_creation_5m_tokens,
                    claude_cache_creation_1_h_tokens: source.cache_creation_1h_tokens,
                    cache_creation: source.cache_creation.as_ref(),
                    server_tool_use: source.server_tool_use.as_ref(),
                },
            })
        } else {
            None
        };
        output
    }
    pub fn usage_semantic(&self) -> Option<&str> {
        self.usage_semantic
    }
    pub fn usage_source(&self) -> Option<&str> {
        self.usage_source
    }
    pub fn has_billing_snapshot(&self) -> bool {
        self.billing_usage.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn check(raw: &str) {
        let source: Option<ClaudeUsage<'_>> = serde_json::from_str(raw).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        let canonical = CanonicalUsage::from_claude(source.as_ref());
        assert_eq!(
            serde_json::to_value(&canonical).unwrap(),
            super::super::usage::usage_from_claude(&original)
        );
        assert_eq!(
            serde_json::to_value(canonical.clone()).unwrap(),
            serde_json::to_value(canonical).unwrap()
        );
    }
    #[test]
    fn native_counter_presence_and_cache_snapshots_match_existing_bridge() {
        for raw in [
            "null",
            "{}",
            r#"{"input_tokens":null,"output_tokens":0}"#,
            r#"{"input_tokens":10,"output_tokens":3}"#,
            r#"{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":7,"cache_creation_input_tokens":8,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":4}}"#,
            r#"{"cache_creation_input_tokens":3,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":5},"server_tool_use":{"web_search_requests":2}}"#,
            r#"{"claude_cache_creation_1_h_tokens":8,"cache_creation":{},"server_tool_use":{}}"#,
            r#"{"server_tool_use":{"web_search_requests":3}}"#,
            r#"{"input_tokens":5,"cost":0,"usage_semantic":"ignored","usage_source":"ignored"}"#,
        ] {
            check(raw);
        }
    }
    #[test]
    fn existing_billing_snapshot_keeps_any_cost_and_cache_detail_presence() {
        for snapshot in [
            json!({}),
            json!({"source":"oai_chat","semantic":"openai","estimated":true,"openai_usage":{"prompt_tokens":10,"cost":0,"prompt_tokens_details":{"cached_tokens_details":{"text_tokens":0,"image_tokens":3}},"billing_usage":{"source":"must_be_removed"}}}),
            json!({"source":"claude_messages","claude_usage":{"input_tokens":0,"cache_creation":{},"server_tool_use":{"web_search_requests":0},"billing_usage":{"source":"must_be_removed"}}}),
            json!({"source":"gemini_chat","gemini_usage_metadata":{"promptTokenCount":1,"promptTokensDetails":null,"candidatesTokensDetails":[],"billing_usage":{"source":"must_be_removed"}}}),
        ] {
            let raw =
                json!({"input_tokens":2,"output_tokens":3,"billing_usage":snapshot}).to_string();
            check(&raw);
            let parsed: ClaudeUsage<'_> = serde_json::from_str(&raw).unwrap();
            assert!(!parsed.needs_compatibility());
        }
    }
    #[test]
    fn noncanonical_duplicate_and_ambiguous_any_inputs_request_compatibility() {
        for raw in [
            r#"{"Input_Tokens":4}"#,
            r#"{"input_tokens":3,"input_tokens":4}"#,
            r#"{"cache_creation":{"Ephemeral_1h_input_tokens":2}}"#,
            r#"{"server_tool_use":{"web_search_requests":1,"web_search_requests":2}}"#,
            r#"{"billing_usage":{"openai_usage":{"cost":9007199254740993}}}"#,
        ] {
            let parsed: ClaudeUsage<'_> = serde_json::from_str(raw).unwrap();
            assert!(parsed.needs_compatibility(), "{raw}");
        }
        for raw in [
            r#"{"input_tokens":1.0}"#,
            r#"{"billing_usage":{"openai_usage":{"prompt_tokens":"bad"}}}"#,
        ] {
            assert!(serde_json::from_str::<ClaudeUsage<'_>>(raw).is_err());
        }
    }
}

#[derive(Clone, Debug)]
struct NativeOpenAiSnapshot<'a>(&'a dto::Usage);
impl Serialize for NativeOpenAiSnapshot<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let source = self.0;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("prompt_tokens", &source.prompt_tokens)?;
        map.serialize_entry("completion_tokens", &source.completion_tokens)?;
        map.serialize_entry("total_tokens", &source.total_tokens)?;
        map.serialize_entry("prompt_tokens_details", &source.prompt_tokens_details)?;
        map.serialize_entry(
            "completion_tokens_details",
            &source.completion_token_details,
        )?;
        map.serialize_entry("input_tokens", &source.input_tokens)?;
        map.serialize_entry("output_tokens", &source.output_tokens)?;
        map.serialize_entry("input_tokens_details", &source.input_tokens_details)?;
        map.serialize_entry(
            "claude_cache_creation_5_m_tokens",
            &source.claude_cache_creation5m_tokens,
        )?;
        map.serialize_entry(
            "claude_cache_creation_1_h_tokens",
            &source.claude_cache_creation1h_tokens,
        )?;
        if source.prompt_cache_hit_tokens != 0 {
            map.serialize_entry("prompt_cache_hit_tokens", &source.prompt_cache_hit_tokens)?;
        }
        if !source.usage_semantic.is_empty() {
            map.serialize_entry("usage_semantic", &source.usage_semantic)?;
        }
        if !source.usage_source.is_empty() {
            map.serialize_entry("usage_source", &source.usage_source)?;
        }
        if !source.cost.is_null() {
            map.serialize_entry("cost", &source.cost)?;
        }
        if let Some(value) = &source.output_tokens_details {
            map.serialize_entry("output_tokens_details", value)?;
        }
        map.end()
    }
}
#[derive(Clone, Debug)]
struct NativeGeminiSnapshot<'a>(&'a dto::GeminiUsageMetadata);
impl Serialize for NativeGeminiSnapshot<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let source = self.0;
        let mut map = serializer.serialize_struct("GeminiUsageMetadata", 9)?;
        map.serialize_field("promptTokenCount", &source.prompt_token_count)?;
        map.serialize_field(
            "toolUsePromptTokenCount",
            &source.tool_use_prompt_token_count,
        )?;
        map.serialize_field("candidatesTokenCount", &source.candidates_token_count)?;
        map.serialize_field("totalTokenCount", &source.total_token_count)?;
        map.serialize_field("thoughtsTokenCount", &source.thoughts_token_count)?;
        map.serialize_field(
            "cachedContentTokenCount",
            &source.cached_content_token_count,
        )?;
        map.serialize_field(
            "promptTokensDetails",
            source.prompt_tokens_details.as_deref().unwrap_or(&[]),
        )?;
        map.serialize_field(
            "toolUsePromptTokensDetails",
            source
                .tool_use_prompt_tokens_details
                .as_deref()
                .unwrap_or(&[]),
        )?;
        map.serialize_field(
            "candidatesTokensDetails",
            source.candidates_tokens_details.as_deref().unwrap_or(&[]),
        )?;
        map.end()
    }
}
impl<'a> InputDetails<'a> {
    fn from_native(source: &'a dto::InputTokenDetails) -> Self {
        Self {
            cached_tokens: source.cached_tokens,
            text_tokens: source.text_tokens,
            audio_tokens: source.audio_tokens,
            image_tokens: source.image_tokens,
            cached_creation_tokens: source.cached_creation_tokens,
            cache_write_tokens: source.cache_write_tokens,
            cache_creation_input_tokens: source.cache_creation_input_tokens,
            cached_tokens_details: source.cached_tokens_details.as_ref(),
            cache_write_tokens_details: source.cache_write_tokens_details.as_ref(),
        }
    }
    fn any_counter(&self) -> bool {
        [
            self.cached_tokens,
            self.cached_creation_tokens,
            self.cache_write_tokens,
            self.cache_creation_input_tokens,
            self.text_tokens,
            self.audio_tokens,
            self.image_tokens,
        ]
        .iter()
        .any(|v| *v != 0)
    }
}
impl OutputDetails {
    fn from_native(source: &dto::OutputTokenDetails) -> Self {
        Self {
            text_tokens: source.text_tokens,
            audio_tokens: source.audio_tokens,
            image_tokens: source.image_tokens,
            reasoning_tokens: source.reasoning_tokens,
        }
    }
}
fn native_has_tokens(source: &dto::Usage) -> bool {
    [
        source.prompt_tokens,
        source.completion_tokens,
        source.total_tokens,
        source.input_tokens,
        source.output_tokens,
        source.prompt_cache_hit_tokens,
        source.claude_cache_creation5m_tokens,
        source.claude_cache_creation1h_tokens,
    ]
    .iter()
    .any(|v| *v != 0)
        || [
            &source.prompt_tokens_details,
            source
                .input_tokens_details
                .as_ref()
                .unwrap_or(&source.prompt_tokens_details),
        ]
        .iter()
        .any(|details| {
            InputDetails::from_native(details).any_counter()
                || details.cached_tokens_details.as_ref().is_some_and(|d| {
                    [d.text_tokens, d.audio_tokens, d.image_tokens]
                        .iter()
                        .any(|v| v.is_some_and(|n| n != 0))
                })
                || details.cache_write_tokens_details.as_ref().is_some_and(|d| {
                    d.ttl30m_tokens != 0 || d.ttl12h_tokens != 0
                })
        })
        || [
            source.completion_token_details.text_tokens,
            source.completion_token_details.audio_tokens,
            source.completion_token_details.image_tokens,
            source.completion_token_details.reasoning_tokens,
        ]
        .iter()
        .any(|v| *v != 0)
}
impl<'a> CanonicalUsage<'a> {
    pub(crate) fn normalized_openai(source: &'a dto::Usage) -> Self {
        let mut output = Self::from_openai(source, false);
        output.prompt_tokens = source.prompt_tokens;
        output.completion_tokens = source.completion_tokens;
        output.total_tokens = source.total_tokens;
        output.input_tokens = source.input_tokens;
        output.output_tokens = source.output_tokens;
        output.prompt_tokens_details = InputDetails::from_native(&source.prompt_tokens_details);
        output.input_tokens_details = source
            .input_tokens_details
            .as_ref()
            .map(InputDetails::from_native);
        output.output_tokens_details = source
            .output_tokens_details
            .as_ref()
            .map(OutputDetails::from_native);
        output.prompt_cache_hit_tokens = source.prompt_cache_hit_tokens;
        output
    }
    pub(crate) fn from_openai(source: &'a dto::Usage, responses: bool) -> Self {
        let mut output = Self::from_claude(None);
        let (prompt, completion) = if responses {
            (source.input_tokens, source.output_tokens)
        } else {
            (source.prompt_tokens, source.completion_tokens)
        };
        output.prompt_tokens = prompt;
        output.input_tokens = prompt;
        output.completion_tokens = completion;
        output.output_tokens = completion;
        output.total_tokens = if source.total_tokens == 0 {
            prompt.wrapping_add(completion)
        } else {
            source.total_tokens
        };
        output.completion_tokens_details =
            OutputDetails::from_native(&source.completion_token_details);
        output.claude_cache_creation_5_m_tokens = source.claude_cache_creation5m_tokens;
        output.claude_cache_creation_1_h_tokens = source.claude_cache_creation1h_tokens;
        output.usage_semantic =
            (!source.usage_semantic.is_empty()).then_some(&source.usage_semantic);
        output.usage_source = (!source.usage_source.is_empty()).then_some(&source.usage_source);
        output.cost = (!source.cost.is_null()).then_some(&source.cost);
        if responses {
            output.prompt_tokens_details = source
                .input_tokens_details
                .as_ref()
                .map(InputDetails::from_native)
                .unwrap_or_default();
        } else {
            let details = InputDetails::from_native(&source.prompt_tokens_details);
            if details.any_counter() {
                output.input_tokens_details = Some(details);
            }
        }
        output.billing_usage = if let Some(billing) = source.billing_usage.as_deref() {
            Some(Billing::Existing(billing))
        } else if native_has_tokens(source) {
            Some(Billing::OpenAi {
                source: if responses {
                    "oai_responses"
                } else {
                    "oai_chat"
                },
                semantic: "openai",
                openai_usage: NativeOpenAiSnapshot(source),
            })
        } else {
            None
        };
        output
    }
    pub(crate) fn from_gemini(
        source: Option<&'a dto::GeminiUsageMetadata>,
        fallback: i64,
    ) -> Option<Self> {
        let mut output = Self::from_claude(None);
        let Some(source) = source else {
            if fallback <= 0 {
                return None;
            }
            output.prompt_tokens = fallback;
            output.prompt_tokens_details.text_tokens = fallback;
            return Some(output);
        };
        let mut prompt = source
            .prompt_token_count
            .wrapping_add(source.tool_use_prompt_token_count);
        if prompt <= 0 && fallback > 0 {
            prompt = fallback;
        }
        let mut completion = source
            .candidates_token_count
            .wrapping_add(source.thoughts_token_count);
        if source.total_token_count > 0 && completion <= 0 {
            completion = source.total_token_count.wrapping_sub(prompt);
        }
        output.prompt_tokens = prompt;
        output.completion_tokens = completion;
        output.total_tokens = source.total_token_count;
        output.completion_tokens_details.reasoning_tokens = source.thoughts_token_count;
        output.prompt_tokens_details.cached_tokens = source.cached_content_token_count;
        for (details, input) in [
            (source.prompt_tokens_details.as_deref(), true),
            (source.tool_use_prompt_tokens_details.as_deref(), true),
            (source.candidates_tokens_details.as_deref(), false),
        ] {
            for part in details.unwrap_or(&[]) {
                let counter = match (input, part.modality.as_str()) {
                    (true, "TEXT") => &mut output.prompt_tokens_details.text_tokens,
                    (true, "IMAGE") => &mut output.prompt_tokens_details.image_tokens,
                    (true, "AUDIO") => &mut output.prompt_tokens_details.audio_tokens,
                    (false, "TEXT") => &mut output.completion_tokens_details.text_tokens,
                    (false, "IMAGE") => &mut output.completion_tokens_details.image_tokens,
                    (false, "AUDIO") => &mut output.completion_tokens_details.audio_tokens,
                    _ => continue,
                };
                *counter = counter.wrapping_add(part.token_count);
            }
        }
        if prompt > 0
            && output.prompt_tokens_details.text_tokens == 0
            && output.prompt_tokens_details.audio_tokens == 0
        {
            output.prompt_tokens_details.text_tokens = prompt;
        }
        let tokens = [
            source.prompt_token_count,
            source.tool_use_prompt_token_count,
            source.candidates_token_count,
            source.total_token_count,
            source.thoughts_token_count,
            source.cached_content_token_count,
        ]
        .iter()
        .any(|v| *v != 0)
            || [
                source.prompt_tokens_details.as_deref(),
                source.tool_use_prompt_tokens_details.as_deref(),
                source.candidates_tokens_details.as_deref(),
            ]
            .iter()
            .any(|a| a.unwrap_or(&[]).iter().any(|d| d.token_count != 0));
        output.billing_usage = if let Some(billing) = source.billing_usage.as_deref() {
            Some(Billing::Existing(billing))
        } else if tokens {
            Some(Billing::Gemini {
                source: "gemini_chat",
                semantic: "gemini",
                gemini_usage_metadata: NativeGeminiSnapshot(source),
            })
        } else {
            None
        };
        Some(output)
    }
    // A composed route applies Go's Chat usage bridge a second time. The native
    // snapshot is retained; presentation counters follow that bridge precisely.
    pub(crate) fn through_chat(&self) -> Self {
        let mut output = self.clone();
        if output.billing_usage.is_none()
            && (self.prompt_tokens != 0
                || self.completion_tokens != 0
                || self.total_tokens != 0
                || self.prompt_tokens_details.any_counter()
                || self.completion_tokens_details.reasoning_tokens != 0)
        {
            output.billing_usage = Some(Billing::ConvertedChat {
                source: "oai_chat",
                semantic: "openai",
                openai_usage: Box::new(self.clone()),
            });
        }
        output.input_tokens = self.prompt_tokens;
        output.output_tokens = self.completion_tokens;
        if output.total_tokens == 0 {
            output.total_tokens = self.prompt_tokens.wrapping_add(self.completion_tokens);
        }
        output.input_tokens_details = self
            .prompt_tokens_details
            .any_counter()
            .then_some(self.prompt_tokens_details);
        output.prompt_tokens_details = InputDetails::default();
        output.output_tokens_details = None;
        output.prompt_cache_hit_tokens = 0;
        output
    }
    fn billing_for_chat(&self) -> Option<Billing<'a>> {
        if self.billing_usage.is_some() {
            return self.billing_usage.clone();
        }
        if self.prompt_tokens != 0
            || self.completion_tokens != 0
            || self.total_tokens != 0
            || self.prompt_tokens_details.any_counter()
            || [
                self.completion_tokens_details.text_tokens,
                self.completion_tokens_details.audio_tokens,
                self.completion_tokens_details.image_tokens,
                self.completion_tokens_details.reasoning_tokens,
            ]
            .iter()
            .any(|v| *v != 0)
        {
            Some(Billing::ConvertedChat {
                source: "oai_chat",
                semantic: "openai",
                openai_usage: Box::new(self.clone()),
            })
        } else {
            None
        }
    }
    pub(crate) fn claude_display(&self) -> ClaudeDisplayUsage<'a> {
        if let Some(Billing::Existing(b)) = &self.billing_usage {
            if let Some(native) = b
                .claude_usage
                .as_ref()
                .filter(|_| b.source == "claude_messages" || b.semantic == "anthropic")
            {
                return ClaudeDisplayUsage::Native {
                    native,
                    billing: self.billing_for_chat(),
                };
            }
        }
        if let Some(Billing::Claude {
            claude_usage: native,
            ..
        }) = &self.billing_usage
        {
            return ClaudeDisplayUsage::Snapshot {
                native: native.clone(),
                billing: self.billing_for_chat(),
            };
        }
        let details = self.prompt_tokens_details;
        let writes = details
            .cached_creation_tokens
            .max(details.cache_write_tokens)
            .max(details.cache_creation_input_tokens)
            .max(0);
        let input = if self.usage_semantic == Some("anthropic") {
            self.prompt_tokens
        } else {
            self.prompt_tokens
                .wrapping_sub(details.cached_tokens)
                .wrapping_sub(writes)
                .max(0)
        };
        let c1 = self.claude_cache_creation_1_h_tokens;
        let c5 = self.claude_cache_creation_5_m_tokens.wrapping_add(
            details
                .cached_creation_tokens
                .wrapping_sub(self.claude_cache_creation_5_m_tokens)
                .wrapping_sub(c1)
                .max(0),
        );
        ClaudeDisplayUsage::Computed {
            input_tokens: input,
            output_tokens: self.completion_tokens,
            cache_creation_input_tokens: writes,
            cache_read_input_tokens: details.cached_tokens,
            claude_cache_creation_5_m_tokens: 0,
            claude_cache_creation_1_h_tokens: 0,
            cache_creation: (c5 > 0 || c1 > 0).then_some(CacheCreation {
                tokens_5m: c5,
                tokens_1h: c1,
                compatibility: false,
            }),
            billing_usage: self.billing_for_chat(),
        }
    }
    pub(crate) fn gemini_display(&self) -> GeminiDisplayUsage<'a> {
        let native = match &self.billing_usage {
            Some(Billing::Existing(b)) if b.source == "gemini_chat" || b.semantic == "gemini" => {
                b.gemini_usage_metadata.as_ref()
            }
            Some(Billing::Gemini {
                gemini_usage_metadata,
                ..
            }) => {
                return GeminiDisplayUsage {
                    native: Some(gemini_usage_metadata.0),
                    prompt: 0,
                    completion: 0,
                    total: 0,
                    billing: self.billing_for_chat(),
                };
            }
            _ => None,
        };
        GeminiDisplayUsage {
            native,
            prompt: self.prompt_tokens,
            completion: self.completion_tokens,
            total: if self.total_tokens == 0 {
                self.prompt_tokens.wrapping_add(self.completion_tokens)
            } else {
                self.total_tokens
            },
            billing: self.billing_for_chat(),
        }
    }
}
#[derive(Debug, Serialize)]
#[serde(untagged)]
#[allow(
    private_interfaces,
    reason = "only the opaque display value leaves this accounting module"
)]
pub(crate) enum ClaudeDisplayUsage<'a> {
    Native {
        #[serde(flatten)]
        native: &'a dto::ClaudeUsage,
        #[serde(rename = "billing_usage")]
        billing: Option<Billing<'a>>,
    },
    Snapshot {
        #[serde(flatten)]
        native: NativeClaudeSnapshot<'a>,
        #[serde(rename = "billing_usage")]
        billing: Option<Billing<'a>>,
    },
    Computed {
        input_tokens: i64,
        output_tokens: i64,
        cache_creation_input_tokens: i64,
        cache_read_input_tokens: i64,
        claude_cache_creation_5_m_tokens: i64,
        claude_cache_creation_1_h_tokens: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_creation: Option<CacheCreation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        billing_usage: Option<Billing<'a>>,
    },
}
#[derive(Debug)]
pub(crate) struct GeminiDisplayUsage<'a> {
    native: Option<&'a dto::GeminiUsageMetadata>,
    prompt: i64,
    completion: i64,
    total: i64,
    billing: Option<Billing<'a>>,
}
impl Serialize for GeminiDisplayUsage<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        if let Some(native) = self.native {
            let snapshot = NativeGeminiSnapshot(native);
            // Snapshot serialization contains no recursive billing sidecar.
            #[derive(Serialize)]
            struct WithBilling<'a, 'b> {
                #[serde(flatten)]
                snapshot: &'b NativeGeminiSnapshot<'a>,
                #[serde(skip_serializing_if = "Option::is_none")]
                billing_usage: &'b Option<Billing<'a>>,
            }
            return WithBilling {
                snapshot: &snapshot,
                billing_usage: &self.billing,
            }
            .serialize(serializer);
        }
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("promptTokenCount", &self.prompt)?;
        map.serialize_entry("toolUsePromptTokenCount", &0)?;
        map.serialize_entry("candidatesTokenCount", &self.completion)?;
        map.serialize_entry("totalTokenCount", &self.total)?;
        map.serialize_entry("thoughtsTokenCount", &0)?;
        map.serialize_entry("cachedContentTokenCount", &0)?;
        let none: Option<&[dto::GeminiPromptTokensDetails]> = None;
        map.serialize_entry("promptTokensDetails", &none)?;
        map.serialize_entry("toolUsePromptTokensDetails", &none)?;
        map.serialize_entry("candidatesTokensDetails", &none)?;
        if let Some(billing) = &self.billing {
            map.serialize_entry("billing_usage", billing)?;
        }
        map.end()
    }
}
