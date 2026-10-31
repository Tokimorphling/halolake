//! Borrowed serialization of the compiled Go wire contract.
use super::*;
use serde::{
    Serialize, Serializer,
    ser::{Error as _, SerializeMap, SerializeSeq},
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct NormalizedView<'a> {
    name: &'a str,
    value: &'a Value,
}
pub(crate) fn normalized_view<'a>(name: &'a str, value: &'a Value) -> NormalizedView<'a> {
    NormalizedView { name, value }
}
impl Serialize for NormalizedView<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if matches!(self.name, "any" | "interface{}") {
            return AnyView(self.value).serialize(s);
        }
        if let Some(index) = schema().names.get(self.name) {
            return ObjectView {
                which: *index,
                input: Some(self.value),
                tools_content: false,
            }
            .serialize(s);
        }
        let ty = WireType::compile(self.name, &schema().names);
        FieldView {
            ty: &ty,
            input: Some(self.value),
            tools_content: false,
        }
        .serialize(s)
    }
}
struct AnyView<'a>(&'a Value);
impl Serialize for AnyView<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.);
                if !(f == 0. && f.is_sign_negative())
                    && f.fract() == 0.
                    && f >= i64::MIN as f64
                    && f < i64::MAX as f64
                {
                    s.serialize_i64(f as i64)
                } else {
                    s.serialize_f64(f)
                }
            }
            Value::Array(values) => {
                let mut seq = s.serialize_seq(Some(values.len()))?;
                for v in values {
                    seq.serialize_element(&AnyView(v))?;
                }
                seq.end()
            }
            Value::Object(values) => {
                let mut map = s.serialize_map(Some(values.len()))?;
                for (k, v) in values {
                    map.serialize_entry(k, &AnyView(v))?;
                }
                map.end()
            }
            v => v.serialize(s),
        }
    }
}
struct FieldView<'a> {
    ty: &'a WireType,
    input: Option<&'a Value>,
    tools_content: bool,
}
impl FieldView<'_> {
    fn null(&self) -> bool {
        self.input.is_none_or(Value::is_null)
    }
    fn empty(&self) -> Result<bool, Error> {
        if self.null() {
            return Ok(!matches!(self.ty.kind, Kind::Named(_)));
        }
        let value = self.input.unwrap();
        match &self.ty.kind {
            Kind::Pointer(inner) => FieldView {
                ty: inner,
                input: self.input,
                tools_content: false,
            }
            .empty(),
            Kind::Named(which) => {
                // Only special Responses optional fields consult struct emptiness;
                // ordinary omitempty never omits a value struct.
                Ok(normalize_definition(*which, Input::Borrowed(value))?
                    .as_object()
                    .is_some_and(Map::is_empty))
            }
            Kind::Slice(_) => value
                .as_array()
                .map(Vec::is_empty)
                .ok_or_else(|| Error::message(format!("expected JSON array for {}", self.ty.name))),
            Kind::Map(_) => value.as_object().map(Map::is_empty).ok_or_else(|| {
                Error::message(format!("expected JSON object for {}", self.ty.name))
            }),
            Kind::Any | Kind::Raw | Kind::Opaque => Ok(empty(value)),
            _ => {
                validate_scalar(self.ty, self.input)?;
                Ok(match (&self.ty.kind, value) {
                    (Kind::IntValue, Value::String(text)) => text.parse::<i64>().unwrap() == 0,
                    (Kind::IntValue, Value::Number(number)) => {
                        number
                            .as_i64()
                            .unwrap_or_else(|| number.as_f64().unwrap_or(0.) as i64)
                            == 0
                    }
                    (Kind::BoolValue, Value::String(text)) => text == "false",
                    (Kind::StringValue, Value::Number(_)) => false,
                    _ => empty(value),
                })
            }
        }
    }
    fn omitted(&self) -> Result<bool, Error> {
        match self.ty.kind {
            Kind::Raw => Ok(self.input.is_none()),
            Kind::Pointer(_) | Kind::Any => Ok(self.null()),
            Kind::Named(_) => Ok(false),
            _ => self.empty(),
        }
    }
}
impl Serialize for FieldView<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match &self.ty.kind {
            Kind::Pointer(inner) => {
                if self.null() {
                    s.serialize_none()
                } else {
                    FieldView { ty: inner, ..*self }.serialize(s)
                }
            }
            Kind::Named(which) => ObjectView {
                which: *which,
                input: self.input,
                tools_content: self.tools_content,
            }
            .serialize(s),
            Kind::Slice(inner) => {
                if self.null() {
                    return s.serialize_none();
                }
                let values = self.input.unwrap().as_array().ok_or_else(|| {
                    S::Error::custom(format!("expected JSON array for {}", self.ty.name))
                })?;
                let mut seq = s.serialize_seq(Some(values.len()))?;
                for v in values {
                    seq.serialize_element(&FieldView {
                        ty: inner,
                        input: Some(v),
                        tools_content: self.tools_content,
                    })?;
                }
                seq.end()
            }
            Kind::Map(inner) => {
                if self.null() {
                    return s.serialize_none();
                }
                let values = self.input.unwrap().as_object().ok_or_else(|| {
                    S::Error::custom(format!("expected JSON object for {}", self.ty.name))
                })?;
                let mut map = s.serialize_map(Some(values.len()))?;
                for (k, v) in values {
                    map.serialize_entry(
                        k,
                        &FieldView {
                            ty: inner,
                            input: Some(v),
                            tools_content: false,
                        },
                    )?;
                }
                map.end()
            }
            Kind::Any => AnyView(self.input.unwrap_or(&Value::Null)).serialize(s),
            Kind::Raw | Kind::Opaque => self.input.unwrap_or(&Value::Null).serialize(s),
            _ => {
                validate_scalar(self.ty, self.input).map_err(S::Error::custom)?;
                if self.null() {
                    return match self.ty.kind {
                        Kind::String | Kind::StringValue => s.serialize_str(""),
                        Kind::Bool | Kind::BoolValue => s.serialize_bool(false),
                        _ => s.serialize_i64(0),
                    };
                }
                match (&self.ty.kind, self.input.unwrap()) {
                    (Kind::IntValue, Value::Number(n)) => s.serialize_i64(
                        n.as_i64()
                            .unwrap_or_else(|| n.as_f64().unwrap_or(0.) as i64),
                    ),
                    (Kind::IntValue, Value::String(v)) => {
                        s.serialize_i64(v.parse::<i64>().unwrap())
                    }
                    (Kind::StringValue, Value::Number(n)) => s.serialize_str(&n.to_string()),
                    (Kind::BoolValue, Value::String(v)) => s.serialize_bool(v == "true"),
                    (_, value) => value.serialize(s),
                }
            }
        }
    }
}
struct ObjectView<'a> {
    which: usize,
    input: Option<&'a Value>,
    tools_content: bool,
}
impl Serialize for ObjectView<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let definition = &schema().definitions[self.which];
        let empty_map = Map::new();
        let input = self.input.unwrap_or(&Value::Null);
        let source = if input.is_null() {
            &empty_map
        } else {
            input.as_object().ok_or_else(|| {
                S::Error::custom(format!("expected JSON object for {}", definition.name))
            })?
        };
        // No bundled DTO uses embedding. Keep its exact established precedence
        // if a future schema adds it, without complicating the hot scalar views.
        if definition
            .fields
            .iter()
            .any(|f| matches!(f, WireField::Embedded(_)))
        {
            return normalize_definition(self.which, Input::Borrowed(input))
                .map_err(S::Error::custom)?
                .serialize(s);
        }
        let mut source = Cow::Borrowed(source);
        apply_aliases(&definition.name, &mut source);
        let fold = needs_fold(definition, &source);
        let (required, optional): (&[&str], &[&str]) = if definition.name == "ResponsesOutput" {
            match source.get("type").and_then(Value::as_str) {
                Some("custom_tool_call") => (
                    &["type", "call_id", "name", "input"],
                    &["id", "status", "namespace"],
                ),
                Some("web_search_call") => (&["type", "id"], &["status", "action"]),
                Some("mcp_call") => (
                    &["type", "id", "name", "server_label", "arguments"],
                    &["status", "output", "error", "approval_request_id"],
                ),
                _ => (&[], &[]),
            }
        } else {
            (&[], &[])
        };
        let suppress_budget = matches!(
            definition.name.as_str(),
            "GeneralOpenAIRequest" | "OpenAIResponsesRequest"
        ) && !get(&source, "model", fold)
            .and_then(Value::as_str)
            .is_some_and(allows_thinking_budget);
        let suppress_content = self.tools_content
            && source
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty())
            && get(&source, "content", fold).is_none_or(Value::is_null);
        let mut map = s.serialize_map(None)?;
        for field in &definition.fields {
            let WireField::Field { key, ty, omit } = field else {
                unreachable!()
            };
            let value = get(&source, key, fold);
            let projected = FieldView {
                ty,
                input: value,
                tools_content: definition.name == "GeneralOpenAIRequest" && key == "messages",
            };
            if definition.name == "ResponsesOutput"
                && get(&source, "type", fold).and_then(Value::as_str) == Some("reasoning")
                && key == "summary"
                && value.is_none_or(|value| {
                    value.is_null() || value.as_array().is_some_and(Vec::is_empty)
                })
            {
                map.serialize_entry(key, &Vec::<Value>::new())?;
                continue;
            }
            // Validation precedes Go's final filtering, including fields that
            // special output variants subsequently discard.
            if !required.is_empty() {
                if let Some(value) = value {
                    validate_type(ty, value).map_err(S::Error::custom)?;
                }
                if required.contains(&key.as_str()) {
                    if *omit && projected.omitted().map_err(S::Error::custom)? {
                        if key == "arguments" {
                            map.serialize_entry(key, &Value::Null)?;
                        } else {
                            map.serialize_entry(key, "")?;
                        }
                    } else {
                        map.serialize_entry(key, &projected)?;
                    }
                } else if optional.contains(&key.as_str())
                    && !projected.empty().map_err(S::Error::custom)?
                {
                    map.serialize_entry(key, &projected)?;
                }
                continue;
            }
            if (suppress_budget && key == "thinking_budget")
                || (suppress_content && key == "content")
            {
                if let Some(value) = value {
                    validate_type(ty, value).map_err(S::Error::custom)?;
                }
                continue;
            }
            if *omit && projected.omitted().map_err(S::Error::custom)? {
                continue;
            }
            map.serialize_entry(key, &projected)?;
        }
        if definition.name == "Thinking" {
            for (key, value) in source.iter() {
                if !definition.canonical_keys.contains(key) {
                    map.serialize_entry(key, value)?;
                }
            }
        }
        map.end()
    }
}
