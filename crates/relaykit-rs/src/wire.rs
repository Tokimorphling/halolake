//! Go wire-contract validation and normalization at protocol boundaries.
mod projection;
use crate::{Error, Format};
pub(crate) use projection::normalized_view;
use serde_json::{Map, Value, json};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

struct WireSchema {
    names: BTreeMap<String, usize>,
    definitions: Vec<Definition>,
}
struct Definition {
    name: String,
    fields: Vec<WireField>,
    canonical_keys: BTreeSet<String>,
    lowercase: bool,
    movable: bool,
}
enum WireField {
    Embedded(usize),
    Field {
        key: String,
        ty: WireType,
        omit: bool,
    },
}
struct WireType {
    name: String,
    kind: Kind,
}
enum Kind {
    Pointer(Box<WireType>),
    Slice(Box<WireType>),
    Map(Box<WireType>),
    Named(usize),
    Any,
    Raw,
    Opaque,
    String,
    Bool,
    Signed,
    Unsigned,
    Float,
    IntValue,
    StringValue,
    BoolValue,
}
impl WireType {
    fn compile(name: &str, names: &BTreeMap<String, usize>) -> Self {
        let kind = if let Some(inner) = name.strip_prefix('*') {
            Kind::Pointer(Box::new(Self::compile(inner, names)))
        } else if let Some(inner) = name.strip_prefix("[]") {
            Kind::Slice(Box::new(Self::compile(inner, names)))
        } else if let Some(inner) = name.strip_prefix("map[string]") {
            Kind::Map(Box::new(Self::compile(inner, names)))
        } else if let Some(index) = names.get(name) {
            Kind::Named(*index)
        } else {
            match name {
                "any" | "interface{}" => Kind::Any,
                "json.RawMessage" => Kind::Raw,
                "string" | "MediaResolution" | "FunctionCallingConfigMode" | "AwsApiType" => {
                    Kind::String
                }
                "bool" => Kind::Bool,
                "int" | "int64" | "int32" => Kind::Signed,
                "uint" | "uint64" | "uint32" => Kind::Unsigned,
                "float64" | "float32" => Kind::Float,
                "IntValue" => Kind::IntValue,
                "StringValue" => Kind::StringValue,
                "BoolValue" => Kind::BoolValue,
                _ => Kind::Opaque,
            }
        };
        Self {
            name: name.to_owned(),
            kind,
        }
    }
    fn omitted(&self, existing: bool, normalized: &Value) -> bool {
        match self.kind {
            Kind::Raw => !existing,
            Kind::Any | Kind::Pointer(_) => normalized.is_null(),
            Kind::Named(_) => false,
            _ => empty(normalized),
        }
    }
}
fn schema() -> &'static WireSchema {
    static SCHEMA: OnceLock<WireSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let raw: BTreeMap<String, Value> =
            serde_json::from_str(include_str!("dto_schema.json")).expect("bundled DTO schema");
        let names: BTreeMap<_, _> = raw
            .keys()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect();
        let definitions = raw
            .into_iter()
            .map(|(name, value)| {
                let fields: Vec<_> = value
                    .as_array()
                    .expect("DTO field list")
                    .iter()
                    .map(|field| {
                        if let Some(embed) = field["embed"].as_str() {
                            WireField::Embedded(names[embed])
                        } else {
                            WireField::Field {
                                key: field["json"].as_str().expect("DTO field key").to_owned(),
                                ty: WireType::compile(
                                    field["type"].as_str().expect("DTO field type"),
                                    &names,
                                ),
                                omit: field["omit"].as_bool().unwrap_or(false),
                            }
                        }
                    })
                    .collect();
                let canonical_keys: BTreeSet<_> = fields
                    .iter()
                    .filter_map(|field| match field {
                        WireField::Field { key, .. } => Some(key.clone()),
                        _ => None,
                    })
                    .collect();
                let lowercase = canonical_keys
                    .iter()
                    .all(|key| !key.bytes().any(|b| b.is_ascii_uppercase()));
                let folded: BTreeSet<_> = canonical_keys
                    .iter()
                    .map(|key| key.to_ascii_lowercase())
                    .collect();
                let movable = !fields.iter().any(|f| matches!(f, WireField::Embedded(_)))
                    && folded.len() == fields.len();
                Definition {
                    name,
                    fields,
                    canonical_keys,
                    lowercase,
                    movable,
                }
            })
            .collect();
        WireSchema { names, definitions }
    })
}
fn request_name(format: Format) -> &'static str {
    match format {
        Format::OpenAi => "GeneralOpenAIRequest",
        Format::OpenAiResponses => "OpenAIResponsesRequest",
        Format::Claude => "ClaudeRequest",
        Format::Gemini => "GeminiChatRequest",
    }
}
fn response_name(format: Format) -> &'static str {
    match format {
        Format::OpenAi => "OpenAITextResponse",
        Format::OpenAiResponses => "OpenAIResponsesResponse",
        Format::Claude => "ClaudeResponse",
        Format::Gemini => "GeminiChatResponse",
    }
}
fn stream_name(format: Format) -> &'static str {
    match format {
        Format::OpenAi => "ChatCompletionsStreamResponse",
        Format::OpenAiResponses => "ResponsesStreamResponse",
        Format::Claude => "ClaudeResponse",
        Format::Gemini => "GeminiChatResponse",
    }
}
pub fn normalize_request(format: Format, value: &Value) -> Result<Value, Error> {
    let mut out = normalize_named(request_name(format), value)?;
    preserve_claude_tool_schemas(format, value, &mut out);
    Ok(out)
}
pub fn normalize_response(format: Format, value: &Value) -> Result<Value, Error> {
    normalize_named(response_name(format), value)
}
pub fn normalize_stream(format: Format, value: &Value) -> Result<Value, Error> {
    normalize_named(stream_name(format), value)
}
/// Normalize an owned payload by moving fields and strings into its wire shape.
pub fn normalize_request_owned(format: Format, value: Value) -> Result<Value, Error> {
    if format == Format::Claude {
        return normalize_request(format, &value);
    }
    normalize_named_owned(request_name(format), value)
}

fn preserve_claude_tool_schemas(format: Format, source: &Value, out: &mut Value) {
    if format != Format::Claude {
        return;
    }
    for (index, tool) in source["tools"].as_array().into_iter().flatten().enumerate() {
        if tool.get("type").is_none_or(Value::is_null)
            && let Some(schema) = tool.get("input_schema")
        {
            out["tools"][index]["input_schema"] = schema.clone();
        }
    }
}
pub fn normalize_response_owned(format: Format, value: Value) -> Result<Value, Error> {
    normalize_named_owned(response_name(format), value)
}
pub fn normalize_stream_owned(format: Format, value: Value) -> Result<Value, Error> {
    normalize_named_owned(stream_name(format), value)
}
/// Validate the source contract without constructing and discarding a wire tree.
pub fn validate_request(format: Format, value: &Value) -> Result<(), Error> {
    validate_named(request_name(format), value)
}
pub fn validate_response(format: Format, value: &Value) -> Result<(), Error> {
    validate_named(response_name(format), value)
}
pub fn validate_stream(format: Format, value: &Value) -> Result<(), Error> {
    validate_named(stream_name(format), value)
}

/// Go decodes interface values as float64, so values outside that range fail
/// decoding instead of silently becoming zero during interface coercion.
pub fn validate_interface_numbers(value: &Value) -> Result<(), Error> {
    match value {
        Value::Number(number) => {
            if number.as_f64().is_none_or(|number| !number.is_finite()) {
                return Err(Error::message(
                    "JSON number is outside the float64 range for an interface value",
                ));
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_interface_numbers(value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_interface_numbers(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(v) => !*v,
        Value::Number(v) => v.as_f64() == Some(0.),
        Value::String(v) => v.is_empty(),
        Value::Array(v) => v.is_empty(),
        Value::Object(v) => v.is_empty(),
    }
}
enum Input<'a> {
    Missing,
    Borrowed(&'a Value),
    Owned(Value),
}
impl Input<'_> {
    fn value(&self) -> Option<&Value> {
        match self {
            Self::Missing => None,
            Self::Borrowed(v) => Some(v),
            Self::Owned(v) => Some(v),
        }
    }
    fn take(self) -> Value {
        match self {
            Self::Missing => Value::Null,
            Self::Borrowed(v) => v.clone(),
            Self::Owned(v) => v,
        }
    }
    fn is_null(&self) -> bool {
        self.value().is_none_or(Value::is_null)
    }
}
fn index(name: &str) -> Result<usize, Error> {
    schema()
        .names
        .get(name)
        .copied()
        .ok_or_else(|| Error::message(format!("unknown DTO {name}")))
}
pub fn normalize_named(name: &str, value: &Value) -> Result<Value, Error> {
    check_object(name, value)?;
    normalize_definition(index(name)?, Input::Borrowed(value))
}
pub fn normalize_named_owned(name: &str, value: Value) -> Result<Value, Error> {
    check_object(name, &value)?;
    normalize_definition(index(name)?, Input::Owned(value))
}
pub fn validate_named(name: &str, value: &Value) -> Result<(), Error> {
    check_object(name, value)?;
    validate_definition(index(name)?, value)
}
fn check_object(name: &str, value: &Value) -> Result<(), Error> {
    if value.is_null() || value.is_object() {
        Ok(())
    } else {
        Err(Error::message(format!("expected JSON object for {name}")))
    }
}
fn needs_fold(definition: &Definition, map: &Map<String, Value>) -> bool {
    if definition.lowercase
        && map
            .keys()
            .all(|key| !key.bytes().any(|b| b.is_ascii_uppercase()))
    {
        return false;
    }
    !map.keys()
        .all(|key| definition.canonical_keys.contains(key))
}
fn get<'a>(map: &'a Map<String, Value>, key: &str, fold: bool) -> Option<&'a Value> {
    map.get(key).or_else(|| {
        if fold {
            map.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
        } else {
            None
        }
    })
}
fn take_field(map: &mut Map<String, Value>, key: &str, fold: bool) -> Option<(String, Value)> {
    if let Some(value) = map.remove_entry(key) {
        return Some(value);
    }
    if fold {
        let actual = map
            .keys()
            .find(|candidate| candidate.eq_ignore_ascii_case(key))
            .cloned()?;
        let value = map.remove(&actual).expect("located field");
        return Some((key.to_owned(), value));
    }
    None
}
#[derive(Clone, Copy)]
enum Special {
    None,
    Custom,
    Web,
    Mcp,
}
fn normalize_definition(which: usize, input: Input<'_>) -> Result<Value, Error> {
    let definition = &schema().definitions[which];
    let mut source = match input {
        Input::Borrowed(Value::Object(map)) => Cow::Borrowed(map),
        Input::Owned(Value::Object(map)) => Cow::Owned(map),
        Input::Missing | Input::Borrowed(Value::Null) | Input::Owned(Value::Null) => {
            Cow::Owned(Map::new())
        }
        _ => {
            return Err(Error::message(format!(
                "expected JSON object for {}",
                definition.name
            )));
        }
    };
    apply_aliases(&definition.name, &mut source);
    let fold = needs_fold(definition, &source);
    // Go's Thinking keeps unknown raw fields, including differently cased
    // spellings of known fields, in its custom JSON marshal implementation.
    let extra: Map<String, Value> = if definition.name == "Thinking" {
        source
            .iter()
            .filter(|(key, _)| !definition.canonical_keys.contains(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    } else {
        Map::new()
    };
    let special = match source.get("type").and_then(Value::as_str) {
        Some("custom_tool_call") => Special::Custom,
        Some("web_search_call") => Special::Web,
        Some("mcp_call") => Special::Mcp,
        _ => Special::None,
    };
    let mut out = Map::new();
    for field in &definition.fields {
        match field {
            WireField::Embedded(embed) => {
                let embedded = normalize_fields_borrowed(*embed, &source)?;
                for (key, value) in embedded {
                    out.entry(key).or_insert(value);
                }
            }
            WireField::Field { key, ty, omit } => {
                let (owned_key, existing, normalized) = match &mut source {
                    Cow::Owned(map) if definition.movable => {
                        let entry = take_field(map, key, fold);
                        if entry.is_none() && *omit && !matches!(ty.kind, Kind::Named(_)) {
                            continue;
                        }
                        let existing = entry.is_some();
                        let (owned_key, input) = match entry {
                            Some((key, value)) => (Some(key), Input::Owned(value)),
                            None => (None, Input::Missing),
                        };
                        (owned_key, existing, normalize_type(ty, input)?)
                    }
                    _ => {
                        let value = get(&source, key, fold);
                        if value.is_none() && *omit && !matches!(ty.kind, Kind::Named(_)) {
                            continue;
                        }
                        (
                            None,
                            value.is_some(),
                            normalize_type(
                                ty,
                                value.map(Input::Borrowed).unwrap_or(Input::Missing),
                            )?,
                        )
                    }
                };
                if !omit || !ty.omitted(existing, &normalized) {
                    out.insert(owned_key.unwrap_or_else(|| key.clone()), normalized);
                }
            }
        }
    }
    finish(&definition.name, &mut out, special);
    out.extend(extra);
    Ok(Value::Object(out))
}
fn normalize_fields_borrowed(
    which: usize,
    source: &Map<String, Value>,
) -> Result<Map<String, Value>, Error> {
    // Embedded schemas are uncommon; keep independent field lookups so their
    // precedence is identical even if schemas expose overlapping field names.
    let value = normalize_definition(which, Input::Borrowed(&Value::Object(source.clone())))?;
    let Value::Object(value) = value else {
        unreachable!("normalized object")
    };
    Ok(value)
}
fn normalize_type(ty: &WireType, input: Input<'_>) -> Result<Value, Error> {
    match &ty.kind {
        Kind::Pointer(inner) => {
            if input.is_null() {
                Ok(Value::Null)
            } else {
                normalize_type(inner, input)
            }
        }
        Kind::Named(which) => normalize_definition(*which, input),
        Kind::Slice(inner) => match input {
            Input::Missing | Input::Borrowed(Value::Null) | Input::Owned(Value::Null) => {
                Ok(Value::Null)
            }
            Input::Borrowed(Value::Array(values)) => values
                .iter()
                .map(|value| normalize_type(inner, Input::Borrowed(value)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Input::Owned(Value::Array(values)) => values
                .into_iter()
                .map(|value| normalize_type(inner, Input::Owned(value)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            _ => Err(Error::message(format!(
                "expected JSON array for {}",
                ty.name
            ))),
        },
        Kind::Map(inner) => match input {
            Input::Missing | Input::Borrowed(Value::Null) | Input::Owned(Value::Null) => {
                Ok(Value::Null)
            }
            Input::Borrowed(Value::Object(values)) => values
                .iter()
                .map(|(key, value)| {
                    Ok((key.clone(), normalize_type(inner, Input::Borrowed(value))?))
                })
                .collect::<Result<Map<_, _>, Error>>()
                .map(Value::Object),
            Input::Owned(Value::Object(values)) => values
                .into_iter()
                .map(|(key, value)| Ok((key, normalize_type(inner, Input::Owned(value))?)))
                .collect::<Result<Map<_, _>, Error>>()
                .map(Value::Object),
            _ => Err(Error::message(format!(
                "expected JSON object for {}",
                ty.name
            ))),
        },
        Kind::Any => match input {
            Input::Borrowed(value) => {
                validate_interface_numbers(value)?;
                Ok(go_interface_value(value))
            }
            input => {
                let mut value = input.take();
                validate_interface_numbers(&value)?;
                go_interface_owned(&mut value);
                Ok(value)
            }
        },
        Kind::Raw | Kind::Opaque => Ok(input.take()),
        _ => {
            validate_scalar(ty, input.value())?;
            if input.is_null() {
                return Ok(match ty.kind {
                    Kind::String | Kind::StringValue => Value::String(String::new()),
                    Kind::Bool | Kind::BoolValue => Value::Bool(false),
                    _ => json!(0),
                });
            }
            match (&ty.kind, input.value().unwrap()) {
                (Kind::IntValue, Value::Number(number)) => Ok(json!(
                    number
                        .as_i64()
                        .unwrap_or_else(|| number.as_f64().unwrap_or(0.) as i64)
                )),
                (Kind::IntValue, Value::String(text)) => {
                    Ok(json!(text.parse::<i64>().expect("validated integer")))
                }
                (Kind::StringValue, Value::Number(number)) => Ok(Value::String(number.to_string())),
                (Kind::BoolValue, Value::String(text)) => Ok(Value::Bool(text == "true")),
                _ => Ok(input.take()),
            }
        }
    }
}
fn validate_scalar(ty: &WireType, input: Option<&Value>) -> Result<(), Error> {
    let Some(value) = input.filter(|v| !v.is_null()) else {
        return Ok(());
    };
    match &ty.kind {
        Kind::String => {
            if value.is_string() {
                Ok(())
            } else {
                Err(Error::message(format!(
                    "expected JSON string for {}",
                    ty.name
                )))
            }
        }
        Kind::Bool => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err(Error::message("expected JSON boolean"))
            }
        }
        Kind::Signed | Kind::Unsigned | Kind::Float => {
            if !value.is_number() {
                return Err(Error::message(format!(
                    "expected JSON number for {}",
                    ty.name
                )));
            }
            if matches!(ty.kind, Kind::Signed) && value.as_i64().is_none() {
                return Err(Error::message("expected integer"));
            }
            if matches!(ty.kind, Kind::Unsigned) && value.as_u64().is_none() {
                return Err(Error::message("expected unsigned integer"));
            }
            Ok(())
        }
        Kind::IntValue => match value {
            Value::Number(_) => Ok(()),
            Value::String(text) => text
                .parse::<i64>()
                .map(|_| ())
                .map_err(|_| Error::message("invalid integer string")),
            _ => Err(Error::message("expected integer or integer string")),
        },
        Kind::StringValue => {
            if value.is_string() || value.is_number() {
                Ok(())
            } else {
                Err(Error::message("expected string or number"))
            }
        }
        Kind::BoolValue => {
            if value.is_boolean() || value.as_str().is_some_and(|s| s == "true" || s == "false") {
                Ok(())
            } else {
                Err(Error::message("expected boolean or boolean string"))
            }
        }
        _ => Ok(()),
    }
}
fn validate_type(ty: &WireType, value: &Value) -> Result<(), Error> {
    if value.is_null() {
        return Ok(());
    }
    match &ty.kind {
        Kind::Pointer(inner) => validate_type(inner, value),
        Kind::Named(which) => validate_definition(*which, value),
        Kind::Slice(inner) => match value {
            Value::Array(values) => {
                for value in values {
                    validate_type(inner, value)?;
                }
                Ok(())
            }
            _ => Err(Error::message(format!(
                "expected JSON array for {}",
                ty.name
            ))),
        },
        Kind::Map(inner) => match value {
            Value::Object(values) => {
                for value in values.values() {
                    validate_type(inner, value)?;
                }
                Ok(())
            }
            _ => Err(Error::message(format!(
                "expected JSON object for {}",
                ty.name
            ))),
        },
        Kind::Any => validate_interface_numbers(value),
        Kind::Raw | Kind::Opaque => Ok(()),
        _ => validate_scalar(ty, Some(value)),
    }
}
fn validate_definition(which: usize, value: &Value) -> Result<(), Error> {
    let definition = &schema().definitions[which];
    if value.is_null() {
        return Ok(());
    }
    let Some(map) = value.as_object() else {
        return Err(Error::message(format!(
            "expected JSON object for {}",
            definition.name
        )));
    };
    if map.is_empty() {
        return Ok(());
    }
    let mut source = Cow::Borrowed(map);
    apply_aliases(&definition.name, &mut source);
    let fold = needs_fold(definition, &source);
    for field in &definition.fields {
        match field {
            WireField::Embedded(embed) => validate_definition(*embed, value)?,
            WireField::Field { key, ty, .. } => {
                if let Some(value) = get(&source, key, fold) {
                    validate_type(ty, value)?;
                }
            }
        }
    }
    Ok(())
}
fn allows_thinking_budget(model: &str) -> bool {
    let model = model.trim().as_bytes();
    [b"qwen".as_slice(), b"qwq".as_slice()]
        .iter()
        .any(|prefix| {
            model
                .get(..prefix.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(prefix))
                || model
                    .windows(prefix.len() + 1)
                    .any(|s| s[0] == b'/' && s[1..].eq_ignore_ascii_case(prefix))
        })
}
fn finish(name: &str, out: &mut Map<String, Value>, special: Special) {
    if name == "FunctionRequest" && out.get("parameters").is_some_and(Value::is_null) {
        out.remove("parameters");
    }
    if name == "GeneralOpenAIRequest" || name == "OpenAIResponsesRequest" {
        let model = out.get("model").and_then(Value::as_str).unwrap_or("");
        if !allows_thinking_budget(model) {
            out.remove("thinking_budget");
        }
        if name == "GeneralOpenAIRequest" {
            if let Some(Value::Array(messages)) = out.get_mut("messages") {
                for message in messages {
                    if message["tools"].as_array().is_some_and(|v| !v.is_empty())
                        && message["content"].is_null()
                    {
                        message.as_object_mut().unwrap().remove("content");
                    }
                }
            }
        }
    }
    if name == "ResponsesOutput" {
        if out.get("type").and_then(Value::as_str) == Some("reasoning") {
            out.entry("summary").or_insert_with(|| json!([]));
        }
        let (required, optional): (&[&str], &[&str]) = match special {
            Special::Custom => (
                &["type", "call_id", "name", "input"],
                &["id", "status", "namespace"],
            ),
            Special::Web => (&["type", "id"], &["status", "action"]),
            Special::Mcp => (
                &["type", "id", "name", "server_label", "arguments"],
                &["status", "output", "error", "approval_request_id"],
            ),
            _ => (&[], &[]),
        };
        if !required.is_empty() {
            out.retain(|k, v| {
                required.contains(&k.as_str()) || optional.contains(&k.as_str()) && !empty(v)
            });
            for &k in required {
                if !out.contains_key(k) {
                    out.insert(
                        k.into(),
                        if k == "arguments" {
                            Value::Null
                        } else {
                            json!("")
                        },
                    );
                }
            }
        }
    }
}
fn apply_aliases(name: &str, m: &mut Cow<'_, Map<String, Value>>) {
    let aliases: &[(&str, &str)] = match name {
        "GeminiChatRequest" => &[("system_instruction", "systemInstruction")],
        "GeminiThinkingConfig" => &[
            ("include_thoughts", "includeThoughts"),
            ("thinking_budget", "thinkingBudget"),
            ("thinking_level", "thinkingLevel"),
        ],
        "GeminiInlineData" => &[("mime_type", "mimeType")],
        "GeminiPart" => &[("inline_data", "inlineData")],
        "GeminiChatGenerationConfig" => &[
            ("top_p", "topP"),
            ("top_k", "topK"),
            ("max_output_tokens", "maxOutputTokens"),
            ("candidate_count", "candidateCount"),
            ("stop_sequences", "stopSequences"),
            ("response_mime_type", "responseMimeType"),
            ("response_schema", "responseSchema"),
            ("response_json_schema", "responseJsonSchema"),
            ("presence_penalty", "presencePenalty"),
            ("frequency_penalty", "frequencyPenalty"),
            ("response_logprobs", "responseLogprobs"),
            (
                "enable_enhanced_civic_answers",
                "enableEnhancedCivicAnswers",
            ),
            ("media_resolution", "mediaResolution"),
            ("response_modalities", "responseModalities"),
            ("thinking_config", "thinkingConfig"),
            ("speech_config", "speechConfig"),
            ("image_config", "imageConfig"),
        ],
        _ => &[],
    };
    for &(snake, camel) in aliases {
        if let Some(v) = m.get(snake) {
            if !v.is_null()
                && !v.as_str().is_some_and(str::is_empty)
                && !v.as_array().is_some_and(Vec::is_empty)
            {
                let value = match m {
                    Cow::Borrowed(map) => map[snake].clone(),
                    Cow::Owned(map) => map.remove(snake).expect("located alias"),
                };
                m.to_mut().insert(camel.into(), value);
            }
        }
    }
}

// encoding/json decodes interface{} numbers through float64. RawMessage fields
// deliberately bypass this conversion and retain their original numeric value.
pub(crate) fn go_interface_value(value: &Value) -> Value {
    let mut value = value.clone();
    go_interface_owned(&mut value);
    value
}
fn go_interface_owned(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let f = number.as_f64().unwrap_or(0.);
            *value = if f == 0. && f.is_sign_negative() {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            } else if f.fract() == 0. && f >= i64::MIN as f64 && f < i64::MAX as f64 {
                json!(f as i64)
            } else {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            };
        }
        Value::Array(values) => values.iter_mut().for_each(go_interface_owned),
        Value::Object(values) => {
            values.values_mut().for_each(go_interface_owned);
            values.sort_keys();
        }
        _ => {}
    }
}
pub(crate) fn normalize_converted_request_owned(
    format: Format,
    mut value: Value,
) -> Result<Value, Error> {
    let native = if format == Format::OpenAi {
        value
            .as_object_mut()
            .and_then(|map| map.remove("__relaykit_native_chat_tools"))
    } else {
        None
    };
    let mut out = normalize_request_owned(format, value)?;
    if let Some(Value::Object(native)) = native {
        if let Some(tools) = out["tools"].as_array_mut() {
            for (index, value) in native {
                if let Ok(index) = index.parse::<usize>() {
                    if let Some(tool) = tools.get_mut(index) {
                        *tool = value;
                    }
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn normalize_converted_response_owned(
    format: Format,
    mut value: Value,
) -> Result<Value, Error> {
    // Converted Claude tool inputs contain RawMessage values, not Go float64
    // interface values. Native Claude inputs still use the ordinary contract.
    let inputs: Vec<_> = if format == Format::Claude {
        value["content"]
            .as_array_mut()
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, block)| block["type"] == "tool_use")
            .map(|(index, block)| (index, block["input"].take()))
            .collect()
    } else {
        Vec::new()
    };
    let mut out = normalize_response_owned(format, value)?;
    for (index, input) in inputs {
        if !input.is_null() {
            out["content"][index]["input"] = input;
        }
    }
    Ok(out)
}
