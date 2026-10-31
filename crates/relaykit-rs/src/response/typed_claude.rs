//! Borrowed Claude response input for the writer-oriented response path.
//!
//! Canonical field names decode directly. Ambiguous known-field casing and
//! duplicate known keys request the existing compatibility path. Dynamic `any`
//! islands are decoded once, like Go's interface{} fields; RawMessage islands
//! remain borrowed and retain explicit JSON nulls.
use super::typed_usage::ClaudeUsage;
use crate::Error;
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, value::RawValue};
use std::{borrow::Cow, fmt, marker::PhantomData};

#[derive(Debug, Default)]
pub(crate) struct ClaudeInput<'a> {
    pub id: Cow<'a, str>,
    pub kind: Cow<'a, str>,
    pub role: Cow<'a, str>,
    pub content: Vec<ClaudeBlock<'a>>,
    pub completion: Cow<'a, str>,
    pub stop_reason: Cow<'a, str>,
    pub model: Cow<'a, str>,
    pub error: Option<Value>,
    pub usage: Option<ClaudeUsage<'a>>,
    pub index: Option<i64>,
    pub content_block: Option<Box<ClaudeBlock<'a>>>,
    pub delta: Option<Box<ClaudeBlock<'a>>>,
    pub message: Option<Box<ClaudeBlock<'a>>>,
    compatibility: bool,
}
impl<'a> ClaudeInput<'a> {
    pub fn parse(input: &'a [u8]) -> Result<Option<Self>, Error> {
        let parsed: Self = serde_json::from_slice(input)?;
        Ok((!parsed.needs_compatibility()).then_some(parsed))
    }
    pub fn needs_compatibility(&self) -> bool {
        self.compatibility
            || self
                .usage
                .as_ref()
                .is_some_and(ClaudeUsage::needs_compatibility)
            || self.content.iter().any(ClaudeBlock::needs_compatibility)
            || [&self.content_block, &self.delta, &self.message]
                .into_iter()
                .any(|block| {
                    block
                        .as_ref()
                        .is_some_and(|block| block.needs_compatibility())
                })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_headers_borrow_input() {
        let bytes = br#"{"id":"msg_a","model":"claude","type":"message","content":[{"type":"text","text":"hello world"}]}"#;
        let input = ClaudeInput::parse(bytes).unwrap().unwrap();
        assert!(matches!(input.id, Cow::Borrowed("msg_a")));
        assert!(matches!(input.model, Cow::Borrowed("claude")));
        assert!(matches!(
            input.content[0].text,
            Some(Cow::Borrowed("hello world"))
        ));
    }

    #[test]
    fn escaped_strings_own_only_decoded_text() {
        let input = ClaudeInput::parse(br#"{"content":[{"type":"text","text":"hello\nworld"}]}"#)
            .unwrap()
            .unwrap();
        assert!(matches!(input.content[0].kind, Cow::Borrowed("text")));
        assert!(
            matches!(&input.content[0].text, Some(Cow::Owned(value)) if value == "hello\nworld")
        );
    }

    #[test]
    fn nulls_preserve_zero_values_and_optional_string_presence() {
        let input = ClaudeInput::parse(br#"{"id":null,"model":null,"usage":null,"content":[null,{"text":null,"thinking":"","source":null,"input":null,"citations":null}]}"#).unwrap().unwrap();
        assert!(input.id.is_empty() && input.model.is_empty() && input.usage.is_none());
        assert_eq!(input.content.len(), 2);
        assert!(input.content[0].kind.is_empty());
        assert!(input.content[1].text.is_none());
        assert_eq!(input.content[1].thinking.as_deref(), Some(""));
        assert!(input.content[1].source.is_none() && input.content[1].input.is_none());
        assert_eq!(input.content[1].citations.unwrap().get(), "null");
        assert!(
            ClaudeInput::parse(br#"{"content":null}"#)
                .unwrap()
                .unwrap()
                .content
                .is_empty()
        );
    }

    #[test]
    fn known_field_case_and_duplicates_use_compatibility_path() {
        for bytes in [
            br#"{"MODEL":"claude"}"#.as_slice(),
            br#"{"id":"first","id":"second"}"#,
            br#"{"content":[{"type":"text","Text":"hello"}]}"#,
            br#"{"content":[{"source":{"URL":"https://example.com"}}]}"#,
            br#"{"usage":{"INPUT_TOKENS":1}}"#,
        ] {
            assert!(ClaudeInput::parse(bytes).unwrap().is_none());
        }
    }

    #[test]
    fn unused_known_fields_are_still_validated() {
        for bytes in [
            br#"{"index":"wrong"}"#.as_slice(),
            br#"{"content":[{"type":"text","signature":false}]}"#,
            br#"{"content":[{"type":"text","source":{"media_type":3}}]}"#,
            br#"{"content":[{"type":"text","is_error":1}]}"#,
            br#"{"content":[{"type":"text","input":{"n":1e400}}]}"#,
        ] {
            assert!(ClaudeInput::parse(bytes).is_err());
        }
    }

    #[test]
    fn any_islands_coerce_numbers_once_but_raw_messages_keep_lexemes() {
        let input = ClaudeInput::parse(br#"{"content":[{"type":"tool_use","input":{"large":9007199254740993,"small":1.0},"citations":[{"n":9007199254740993}]}]}"#).unwrap().unwrap();
        let arguments = input.content[0].input.as_ref().unwrap();
        assert_eq!(arguments["large"], 9007199254740992i64);
        assert_eq!(arguments["small"], 1);
        assert_eq!(
            input.content[0].citations.unwrap().get(),
            r#"[{"n":9007199254740993}]"#
        );
    }

    #[test]
    fn negative_zero_keeps_go_tool_argument_spelling() {
        let input = ClaudeInput::parse(
            br#"{"content":[{"type":"tool_use","input":{"negative_zero":-0.0}}]}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            crate::json::stringify(input.content[0].input.as_ref().unwrap()),
            r#"{"negative_zero":-0}"#,
        );
    }

    #[test]
    fn unknown_fields_are_ignored_and_hosted_blocks_remain_typed() {
        let input = ClaudeInput::parse(br#"{"unknown":{"nested":[1,true]},"content":[{"type":"mcp_tool_use","id":"hosted","name":"lookup","server_name":"server","input":{"query":"x"},"caller":{"type":"direct"}}]}"#).unwrap().unwrap();
        let block = &input.content[0];
        assert!(block.is_hosted());
        assert_eq!(block.name, "lookup");
        assert_eq!(block.server_name, "server");
        assert_eq!(block.caller.unwrap().get(), r#"{"type":"direct"}"#);
    }
}

#[derive(Debug, Default)]
pub(crate) struct ClaudeBlock<'a> {
    pub kind: Cow<'a, str>,
    pub text: Option<Cow<'a, str>>,
    pub model: Cow<'a, str>,
    pub source: Option<ClaudeSource<'a>>,
    pub usage: Option<ClaudeUsage<'a>>,
    pub stop_reason: Option<Cow<'a, str>>,
    pub partial_json: Option<Cow<'a, str>>,
    pub role: Cow<'a, str>,
    pub thinking: Option<Cow<'a, str>>,
    pub data: Cow<'a, str>,
    pub signature: Cow<'a, str>,
    pub delta: Cow<'a, str>,
    pub cache_control: Option<&'a RawValue>,
    pub citations: Option<&'a RawValue>,
    pub citation: Option<&'a RawValue>,
    pub caller: Option<&'a RawValue>,
    pub server_name: Cow<'a, str>,
    pub is_error: Option<bool>,
    pub error_code: Cow<'a, str>,
    pub id: Cow<'a, str>,
    pub name: Cow<'a, str>,
    pub input: Option<Value>,
    pub content: Option<Value>,
    pub tool_use_id: Cow<'a, str>,
    compatibility: bool,
}
impl ClaudeBlock<'_> {
    pub fn needs_compatibility(&self) -> bool {
        self.compatibility
            || self
                .usage
                .as_ref()
                .is_some_and(ClaudeUsage::needs_compatibility)
            || self
                .source
                .as_ref()
                .is_some_and(ClaudeSource::needs_compatibility)
    }
    pub fn is_hosted(&self) -> bool {
        let kind = self.kind.trim();
        kind == "server_tool_use" || kind == "mcp_tool_use" || kind.ends_with("_tool_result")
    }
}

#[derive(Debug, Default)]
pub(crate) struct ClaudeSource<'a> {
    pub kind: Cow<'a, str>,
    pub media_type: Cow<'a, str>,
    pub data: Option<Value>,
    pub url: Cow<'a, str>,
    compatibility: bool,
}
impl ClaudeSource<'_> {
    pub fn needs_compatibility(&self) -> bool {
        self.compatibility
    }
}

pub(crate) struct BorrowedString<'a>(pub(crate) Cow<'a, str>);
impl<'de: 'a, 'a> Deserialize<'de> for BorrowedString<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StringVisitor<'a>(PhantomData<&'a str>);
        impl<'de: 'a, 'a> Visitor<'de> for StringVisitor<'a> {
            type Value = BorrowedString<'a>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON string")
            }
            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                value: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(BorrowedString(Cow::Borrowed(value)))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(BorrowedString(Cow::Owned(value.to_owned())))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(BorrowedString(Cow::Owned(value)))
            }
        }
        deserializer.deserialize_string(StringVisitor(PhantomData))
    }
}
fn string<'de: 'a, 'a, A: MapAccess<'de>>(map: &mut A) -> Result<Cow<'a, str>, A::Error> {
    Ok(map
        .next_value::<Option<BorrowedString<'a>>>()?
        .map(|value| value.0)
        .unwrap_or(Cow::Borrowed("")))
}
fn optional_string<'de: 'a, 'a, A: MapAccess<'de>>(
    map: &mut A,
) -> Result<Option<Cow<'a, str>>, A::Error> {
    Ok(map
        .next_value::<Option<BorrowedString<'a>>>()?
        .map(|value| value.0))
}
fn any<'de, A: MapAccess<'de>>(map: &mut A) -> Result<Option<Value>, A::Error> {
    let mut value = map.next_value::<Option<Value>>()?;
    if let Some(value) = &mut value {
        crate::wire::validate_interface_numbers(value).map_err(serde::de::Error::custom)?;
        coerce_go_numbers(value);
    }
    Ok(value)
}
pub(crate) fn coerce_go_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let float = number.as_f64().unwrap_or(0.);
            *value = if float == 0. && float.is_sign_negative() {
                Value::Number(serde_json::Number::from_f64(float).unwrap())
            } else if float.fract() == 0. && float >= i64::MIN as f64 && float < i64::MAX as f64 {
                Value::Number((float as i64).into())
            } else {
                serde_json::Number::from_f64(float)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            };
        }
        Value::Array(values) => values.iter_mut().for_each(coerce_go_numbers),
        Value::Object(values) => values.values_mut().for_each(coerce_go_numbers),
        _ => {}
    }
}

// Source content slices may contain null struct elements: encoding/json leaves
// those elements at the ClaudeMediaMessage zero value.
struct NullableBlock<'a>(ClaudeBlock<'a>);
impl<'de: 'a, 'a> Deserialize<'de> for NullableBlock<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            Option::<ClaudeBlock<'a>>::deserialize(deserializer)?.unwrap_or_default(),
        ))
    }
}
fn blocks<'de: 'a, 'a, A: MapAccess<'de>>(map: &mut A) -> Result<Vec<ClaudeBlock<'a>>, A::Error> {
    struct Blocks<'a>(Vec<ClaudeBlock<'a>>);
    impl<'de: 'a, 'a> Deserialize<'de> for Blocks<'a> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct BlocksVisitor<'a>(PhantomData<&'a str>);
            impl<'de: 'a, 'a> Visitor<'de> for BlocksVisitor<'a> {
                type Value = Blocks<'a>;
                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("an array of Claude content blocks or null")
                }
                fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                    Ok(Blocks(Vec::new()))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                    self.visit_none()
                }
                fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                    d.deserialize_seq(self)
                }
                fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                    let mut blocks = Vec::new();
                    while let Some(NullableBlock(block)) =
                        seq.next_element::<NullableBlock<'a>>()?
                    {
                        if blocks.len() == blocks.capacity() {
                            // Most responses contain one large block. Grow from
                            // one rather than Vec's four-element minimum.
                            let additional = blocks.capacity().max(1);
                            blocks.reserve_exact(additional);
                        }
                        blocks.push(block);
                    }
                    Ok(Blocks(blocks))
                }
            }
            deserializer.deserialize_option(BlocksVisitor(PhantomData))
        }
    }
    Ok(map.next_value::<Blocks<'a>>()?.0)
}
fn field_index(
    key: &str,
    fields: &[&str],
    seen: &mut u32,
    compatibility: &mut bool,
) -> Option<usize> {
    if let Some(index) = fields.iter().position(|field| *field == key) {
        let bit = 1u32 << index;
        if *seen & bit != 0 {
            *compatibility = true;
        }
        *seen |= bit;
        return Some(index);
    }
    // Unknown ASCII fields are ignored. Non-ASCII keys conservatively use the
    // compatibility path, including Unicode case-fold aliases encoding/json may accept.
    if !key.is_ascii() || fields.iter().any(|field| field.eq_ignore_ascii_case(key)) {
        *compatibility = true;
    }
    None
}

impl<'de: 'a, 'a> Deserialize<'de> for ClaudeInput<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<'a>(PhantomData<&'a str>);
        impl<'de: 'a, 'a> Visitor<'de> for ObjectVisitor<'a> {
            type Value = ClaudeInput<'a>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a ClaudeInput JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                const FIELDS: &[&str] = &[
                    "id",
                    "type",
                    "role",
                    "content",
                    "completion",
                    "stop_reason",
                    "model",
                    "error",
                    "usage",
                    "index",
                    "content_block",
                    "delta",
                    "message",
                ];
                let mut out = ClaudeInput::default();
                let mut seen = 0u32;
                while let Some(BorrowedString(key)) = map.next_key::<BorrowedString<'a>>()? {
                    match field_index(&key, FIELDS, &mut seen, &mut out.compatibility) {
                        Some(0) => out.id = string(&mut map)?,
                        Some(1) => out.kind = string(&mut map)?,
                        Some(2) => out.role = string(&mut map)?,
                        Some(3) => out.content = blocks(&mut map)?,
                        Some(4) => out.completion = string(&mut map)?,
                        Some(5) => out.stop_reason = string(&mut map)?,
                        Some(6) => out.model = string(&mut map)?,
                        Some(7) => out.error = any(&mut map)?,
                        Some(8) => out.usage = map.next_value()?,
                        Some(9) => out.index = map.next_value()?,
                        Some(10) => out.content_block = map.next_value()?,
                        Some(11) => out.delta = map.next_value()?,
                        Some(12) => out.message = map.next_value()?,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for ClaudeBlock<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<'a>(PhantomData<&'a str>);
        impl<'de: 'a, 'a> Visitor<'de> for ObjectVisitor<'a> {
            type Value = ClaudeBlock<'a>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a ClaudeBlock JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                const FIELDS: &[&str] = &[
                    "type",
                    "text",
                    "model",
                    "source",
                    "usage",
                    "stop_reason",
                    "partial_json",
                    "role",
                    "thinking",
                    "data",
                    "signature",
                    "delta",
                    "cache_control",
                    "citations",
                    "citation",
                    "caller",
                    "server_name",
                    "is_error",
                    "error_code",
                    "id",
                    "name",
                    "input",
                    "content",
                    "tool_use_id",
                ];
                let mut out = ClaudeBlock::default();
                let mut seen = 0u32;
                while let Some(BorrowedString(key)) = map.next_key::<BorrowedString<'a>>()? {
                    match field_index(&key, FIELDS, &mut seen, &mut out.compatibility) {
                        Some(0) => out.kind = string(&mut map)?,
                        Some(1) => out.text = optional_string(&mut map)?,
                        Some(2) => out.model = string(&mut map)?,
                        Some(3) => out.source = map.next_value()?,
                        Some(4) => out.usage = map.next_value()?,
                        Some(5) => out.stop_reason = optional_string(&mut map)?,
                        Some(6) => out.partial_json = optional_string(&mut map)?,
                        Some(7) => out.role = string(&mut map)?,
                        Some(8) => out.thinking = optional_string(&mut map)?,
                        Some(9) => out.data = string(&mut map)?,
                        Some(10) => out.signature = string(&mut map)?,
                        Some(11) => out.delta = string(&mut map)?,
                        Some(12) => out.cache_control = Some(map.next_value()?),
                        Some(13) => out.citations = Some(map.next_value()?),
                        Some(14) => out.citation = Some(map.next_value()?),
                        Some(15) => out.caller = Some(map.next_value()?),
                        Some(16) => out.server_name = string(&mut map)?,
                        Some(17) => out.is_error = map.next_value()?,
                        Some(18) => out.error_code = string(&mut map)?,
                        Some(19) => out.id = string(&mut map)?,
                        Some(20) => out.name = string(&mut map)?,
                        Some(21) => out.input = any(&mut map)?,
                        Some(22) => out.content = any(&mut map)?,
                        Some(23) => out.tool_use_id = string(&mut map)?,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for ClaudeSource<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<'a>(PhantomData<&'a str>);
        impl<'de: 'a, 'a> Visitor<'de> for ObjectVisitor<'a> {
            type Value = ClaudeSource<'a>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a ClaudeSource JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                const FIELDS: &[&str] = &["type", "media_type", "data", "url"];
                let mut out = ClaudeSource::default();
                let mut seen = 0u32;
                while let Some(BorrowedString(key)) = map.next_key::<BorrowedString<'a>>()? {
                    match field_index(&key, FIELDS, &mut seen, &mut out.compatibility) {
                        Some(0) => out.kind = string(&mut map)?,
                        Some(1) => out.media_type = string(&mut map)?,
                        Some(2) => out.data = any(&mut map)?,
                        Some(3) => out.url = string(&mut map)?,
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}
