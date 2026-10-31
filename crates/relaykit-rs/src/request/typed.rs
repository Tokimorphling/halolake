//! Prepared requests and borrowed protocol output views.
//!
//! Preparation decodes and normalizes the source once. The direct Chat, Claude, Gemini, and
//! Responses views borrow ordinary text and tool results from that canonical
//! source, while tool arguments and opaque transformed islands own their data.
//! Media and hosted continuations use the established compatibility converters.
use super::*;
use crate::ConversionResult;
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use std::io::Write;

/// A request decoded and normalized once for repeated target conversions.
///
/// This retains the caller's original bytes and a canonical JSON source. Its
/// output views borrow that source; it is not a native borrowed JSON decoder.
#[derive(Debug)]
pub struct PreparedRequest<'a> {
    format: Format,
    original: &'a [u8],
    source: Value,
    header: Value,
    borrowed_body: bool,
}
impl<'a> PreparedRequest<'a> {
    pub fn from_slice(format: Format, input: &'a [u8]) -> Result<Self, Error> {
        let value: Value = serde_json::from_slice(input)?;
        if !value.is_object() {
            return Err(Error::message("request must be a JSON object"));
        }
        let source = crate::wire::normalize_request_owned(format, value)?;
        let borrowed_body = eligible(format, &source);
        // Only the small control/configuration fields are duplicated. Histories
        // remain in one tree, and are never copied into this header.
        let body_key = match format {
            Format::OpenAi | Format::Claude => "messages",
            Format::OpenAiResponses => "input",
            Format::Gemini => "contents",
        };
        let header = Value::Object(
            source
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| key.as_str() != body_key)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        Ok(Self {
            format,
            original: input,
            source,
            header,
            borrowed_body,
        })
    }
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn original_json(&self) -> &'a [u8] {
        self.original
    }
    /// Whether the output body is serialized as a borrowed protocol view.
    pub fn uses_typed_path(&self, target: Format) -> bool {
        self.borrowed_body
            && matches!(
                (self.format, target),
                (Format::Claude, Format::OpenAi | Format::OpenAiResponses)
                    | (
                        Format::OpenAiResponses,
                        Format::OpenAi | Format::Claude | Format::Gemini
                    )
            )
    }
}

/// A protocol output that can be written without constructing a target JSON tree.
#[derive(Debug)]
pub struct RequestView<'a> {
    format: Format,
    inner: ViewInner<'a>,
}
#[derive(Debug)]
enum ViewInner<'a> {
    Borrowed {
        header: Value,
        fields: Vec<(&'static str, Node<'a>)>,
    },
    Compatibility(Value),
}
impl RequestView<'_> {
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn is_typed(&self) -> bool {
        matches!(self.inner, ViewInner::Borrowed { .. })
    }
    pub fn to_value(&self) -> Result<Value, Error> {
        Ok(serde_json::to_value(self)?)
    }
    pub fn to_vec(&self) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(self)?)
    }
    pub fn write_to<W: Write>(&self, writer: W) -> Result<(), Error> {
        Ok(serde_json::to_writer(writer, self)?)
    }
}
impl Serialize for RequestView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.inner {
            ViewInner::Compatibility(value) => value.serialize(serializer),
            ViewInner::Borrowed { header, fields } => {
                let header = header.as_object().expect("normalized request object");
                let mut map = serializer.serialize_map(Some(header.len() + fields.len()))?;
                for (key, value) in header {
                    map.serialize_entry(key, value)?;
                }
                for (key, value) in fields {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}
pub type PreparedRequestResult<'a> = ConversionResult<RequestView<'a>>;
pub type RequestWriteResult = ConversionResult<()>;

/// Convert an already-prepared source while preserving tool state, callbacks,
/// reasoning options, diagnostics, and the caller's conversion chain.
pub fn convert_prepared_request<'a>(
    ctx: &Context,
    target: Format,
    input: &'a PreparedRequest<'_>,
) -> Result<PreparedRequestResult<'a>, Error> {
    let path = crate::conversion_path(input.format, target, false);
    let (inner, diagnostics) = if input.uses_typed_path(target) && !ctx.options.preserve_message_structure {
        let (source_header, set) = tools::extract(input.format, &input.header, ctx)?;
        let mut diagnostics = vec![];
        let mut header =
            convert_body_direct(input.format, target, &source_header, ctx, &mut diagnostics)?;
        let fields = match (input.format, target) {
            (Format::Claude, Format::OpenAi) => {
                let mut body = take_array(&mut header, "messages");
                body.extend(claude_chat(&input.source));
                if body.is_empty() {
                    vec![]
                } else {
                    vec![("messages", Node::Array(body))]
                }
            }
            (Format::Claude, Format::OpenAiResponses) => {
                header.as_object_mut().unwrap().remove("input");
                vec![("input", Node::Array(claude_responses(&input.source)))]
            }
            (Format::OpenAiResponses, Format::OpenAi) => {
                let mut body = take_array(&mut header, "messages");
                body.extend(responses_chat(&input.source));
                if body.is_empty() {
                    vec![]
                } else {
                    vec![("messages", Node::Array(body))]
                }
            }
            (Format::OpenAiResponses, Format::Claude) => {
                responses_claude(&input.source, &mut header, &mut diagnostics)
            }
            (Format::OpenAiResponses, Format::Gemini) => {
                responses_gemini(&input.source, &mut header, ctx, &mut diagnostics)?
            }
            _ => unreachable!("typed eligibility only admits implemented direct routes"),
        };
        finish_direct(
            input.format,
            target,
            &source_header,
            &mut header,
            ctx,
            &mut diagnostics,
        )?;
        let tool_diagnostics = tools::attach(target, &mut header, set, ctx)?;
        crate::core::reject_loss(ctx.options.tool_loss_policy, &tool_diagnostics)?;
        diagnostics.extend(tool_diagnostics);
        for diagnostic in &mut diagnostics {
            diagnostic.from = input.format;
            diagnostic.to = target;
        }
        let mut header = crate::wire::normalize_converted_request_owned(target, header)?;
        // Required native fields (for example Gemini contents) are defaulted
        // by the header normalizer. The view supplies them exactly once.
        for (key, _) in &fields {
            header.as_object_mut().unwrap().remove(*key);
        }
        (ViewInner::Borrowed { header, fields }, diagnostics)
    } else {
        let (value, diagnostics) = if ctx.options.preserve_message_structure {
            let source = serde_json::from_slice(input.original)?;
            convert_via(input.format, &path[1..], &source, ctx)?
        } else {
            convert_normalized_via(input.format, &path[1..], &input.source, ctx)?
        };
        let value = if input.format == target && !ctx.options.preserve_message_structure {
            // Source preparation already produced this native wire contract.
            value
        } else {
            let source = value;
            let mut value = if input.format == target {
                crate::wire::normalize_request_owned(target, source.clone())?
            } else {
                crate::wire::normalize_converted_request_owned(target, source.clone())?
            };
            if ctx.options.preserve_message_structure {
                preserve_request_extensions(target, &source, &mut value);
            }
            value
        };
        (ViewInner::Compatibility(value), diagnostics)
    };
    let result = crate::make_result(
        RequestView {
            format: target,
            inner,
        },
        input.format,
        target,
        false,
        diagnostics,
        None,
    );
    let mut chain = ctx
        .conversion_chain
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for step in &result.steps {
        if chain.last() != Some(&step.to) {
            chain.push(step.to);
        }
    }
    Ok(result)
}
/// Write the wire payload directly to a caller-owned buffer or stream.
pub fn convert_request_to_writer<W: Write>(
    ctx: &Context,
    target: Format,
    input: &PreparedRequest<'_>,
    writer: W,
) -> Result<RequestWriteResult, Error> {
    let result = convert_prepared_request(ctx, target, input)?;
    result.value.write_to(writer)?;
    Ok(ConversionResult {
        value: (),
        from: result.from,
        to: result.to,
        converter: result.converter,
        quality: result.quality,
        steps: result.steps,
        diagnostics: result.diagnostics,
        usage: result.usage,
        stream: result.stream,
    })
}

#[derive(Debug)]
enum Node<'a> {
    Str(Cow<'a, str>),
    Any(&'a Value),
    Owned(Value),
    Array(Vec<Node<'a>>),
    Object(Vec<(&'static str, Node<'a>)>),
    Null,
}
impl Serialize for Node<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Str(value) => value.serialize(serializer),
            Self::Any(value) => value.serialize(serializer),
            Self::Owned(value) => value.serialize(serializer),
            Self::Null => serializer.serialize_none(),
            Self::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::Object(fields) => {
                let mut map = serializer.serialize_map(Some(fields.len()))?;
                for (key, value) in fields {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}
fn text(value: &str) -> Node<'_> {
    Node::Str(Cow::Borrowed(value))
}
fn owned_text(value: String) -> Node<'static> {
    Node::Str(Cow::Owned(value))
}
fn obj<'a, const N: usize>(fields: [(&'static str, Node<'a>); N]) -> Node<'a> {
    Node::Object(fields.into())
}
fn field_if_text<'a>(
    fields: &mut Vec<(&'static str, Node<'a>)>,
    key: &'static str,
    value: &'a str,
) {
    if !value.is_empty() {
        fields.push((key, text(value)));
    }
}
fn tool_call<'a>(
    id: &'a str,
    name: Cow<'a, str>,
    args: Cow<'a, str>,
    omit_empty_args: bool,
) -> Node<'a> {
    let mut function = vec![];
    if !name.is_empty() {
        function.push(("name", Node::Str(name)));
    }
    if !omit_empty_args || !args.is_empty() {
        function.push(("arguments", Node::Str(args)));
    }
    let mut call = vec![
        ("type", text("function")),
        ("function", Node::Object(function)),
    ];
    field_if_text(&mut call, "id", id);
    Node::Object(call)
}
fn eligible(format: Format, source: &Value) -> bool {
    // The web-search encoder receives a mutable complete payload and may read
    // or replace history. Its public contract requires the compatibility tree.
    fn has_web(tools: &Value) -> bool {
        arr(tools).iter().any(|tool| {
            s(&tool["type"]).trim().starts_with("web_search")
                || s(&tool["type"]).trim() == "namespace" && has_web(&tool["tools"])
        })
    }
    if has_web(&source["tools"]) {
        return false;
    }
    match format {
        Format::Claude => arr(&source["messages"]).iter().all(|message| {
            let content = &message["content"];
            content.is_string()
                || content.is_null()
                || content.is_array()
                    && arr(content).iter().all(|block| {
                        block.is_object()
                            && !matches!(
                                s(&block["type"]).trim(),
                                "image" | "document" | "server_tool_use" | "mcp_tool_use"
                            )
                            && !s(&block["type"]).trim().ends_with("_tool_result")
                            && (s(&block["type"]) != "tool_result"
                                || !arr(&block["content"])
                                    .iter()
                                    .any(|part| matches!(s(&part["type"]), "image" | "document")))
                    })
        }),
        Format::OpenAiResponses => {
            let input = &source["input"];
            input.is_null()
                || input.is_string()
                || input.is_array()
                    && arr(input).iter().all(|item| {
                        if !item.is_object() {
                            return false;
                        }
                        let kind = s(&item["type"]).trim();
                        if kind == "additional_tools" || tools::history::responses_hosted(kind) {
                            return false;
                        }
                        match kind {
                            "function_call" | "custom_tool_call" => !call_name(item).is_empty(),
                            "function_call_output" | "custom_tool_call_output" => {
                                !arr(&item["output"]).iter().any(|part| {
                                    matches!(
                                        s(&part["type"]).trim(),
                                        "input_image"
                                            | "input_file"
                                            | "input_audio"
                                            | "input_video"
                                    )
                                })
                            }
                            _ => {
                                let content = &item["content"];
                                content.is_null()
                                    || content.is_string()
                                    || content.is_array()
                                        && arr(content).iter().all(|part| {
                                            part.is_object()
                                                && matches!(
                                                    s(&part["type"]).trim(),
                                                    "text" | "input_text" | "output_text"
                                                )
                                                && (part["text"].is_string()
                                                    || part["text"].is_null())
                                        })
                            }
                        }
                    })
        }
        _ => false,
    }
}
fn claude_chat(source: &Value) -> Vec<Node<'_>> {
    let mut messages = vec![];
    let mut names = BTreeMap::<&str, &str>::new();
    let mut unnamed = vec![];
    for message in arr(&source["messages"]) {
        let role = s(&message["role"]);
        let content = &message["content"];
        if let Some(value) = content.as_str() {
            // chat_parts creates a text part even for an empty string.
            messages.push(obj([("role", text(role)), ("content", text(value))]));
            continue;
        }
        let mut calls = vec![];
        let mut parts = vec![];
        for block in arr(content) {
            names.entry(s(&block["id"])).or_insert(s(&block["name"]));
            match s(&block["type"]) {
                "text" | "input_text" => {
                    let mut part = vec![("type", text("text"))];
                    field_if_text(&mut part, "text", s(&block["text"]));
                    if !block["cache_control"].is_null() {
                        part.push(("cache_control", Node::Any(&block["cache_control"])));
                    }
                    parts.push(Node::Object(part));
                }
                "tool_use" => calls.push(tool_call(
                    s(&block["id"]),
                    Cow::Borrowed(s(&block["name"])),
                    Cow::Owned(stringify(&block["input"])),
                    false,
                )),
                "tool_result" => {
                    let name = s(&block["name"]);
                    if name.is_empty() {
                        unnamed.push((messages.len(), s(&block["tool_use_id"])));
                    }
                    let content = if let Some(value) = block["content"].as_str() {
                        text(value)
                    } else {
                        owned_text(claude::tool_result_chat(&block["content"]).0)
                    };
                    let mut result = vec![
                        ("role", text("tool")),
                        ("name", text(name)),
                        ("content", content),
                    ];
                    field_if_text(&mut result, "tool_call_id", s(&block["tool_use_id"]));
                    messages.push(Node::Object(result));
                }
                _ => {}
            }
        }
        if !calls.is_empty() {
            let content = if parts.is_empty() { Node::Null } else { Node::Array(parts) };
            messages.push(obj([("role", text(role)), ("content", content), ("tool_calls", Node::Array(calls))]));
        } else if parts.iter().any(|part| matches!(part, Node::Object(fields) if fields.iter().any(|(key,_)| *key == "text"))) {
            messages.push(obj([("role", text(role)), ("content", Node::Array(parts))]));
        }
    }
    for (index, id) in unnamed {
        if let Node::Object(fields) = &mut messages[index] {
            *fields.iter_mut().find(|(key, _)| *key == "name").unwrap() =
                ("name", text(names.get(id).copied().unwrap_or("")));
        }
    }
    messages
}
fn claude_responses(source: &Value) -> Vec<Node<'_>> {
    let mut items = vec![];
    for message in arr(&source["messages"]) {
        let role = s(&message["role"]).trim();
        if role.is_empty() {
            continue;
        }
        if let Some(content) = message["content"].as_str() {
            items.push(obj([("role", text(role)), ("content", text(content))]));
            continue;
        }
        let mut parts = vec![];
        for block in arr(&message["content"]) {
            match s(&block["type"]) {
                "text" | "input_text" => parts.push(obj([
                    (
                        "type",
                        text(if role == "assistant" {
                            "output_text"
                        } else {
                            "input_text"
                        }),
                    ),
                    ("text", text(s(&block["text"]))),
                ])),
                "tool_use" => {
                    flush(&mut items, &mut parts, role);
                    let arguments = if block["input"].is_null() {
                        "{}".into()
                    } else {
                        stringify(&block["input"])
                    };
                    items.push(obj([
                        ("type", text("function_call")),
                        ("call_id", text(s(&block["id"]))),
                        ("name", text(s(&block["name"]))),
                        ("arguments", owned_text(arguments)),
                    ]));
                }
                "tool_result" => {
                    flush(&mut items, &mut parts, role);
                    let output = if let Some(content) = block["content"].as_str() {
                        text(content)
                    } else if block["content"].is_null() {
                        text("")
                    } else if !block["content"].is_array() {
                        Node::Any(&block["content"])
                    } else {
                        Node::Owned(claude::tool_result_responses(&block["content"]))
                    };
                    items.push(obj([
                        ("type", text("function_call_output")),
                        ("call_id", text(s(&block["tool_use_id"]))),
                        ("output", output),
                    ]));
                }
                _ => {}
            }
        }
        flush(&mut items, &mut parts, role);
    }
    items
}
fn flush<'a>(items: &mut Vec<Node<'a>>, parts: &mut Vec<Node<'a>>, role: &'a str) {
    if !parts.is_empty() {
        items.push(obj([
            ("role", text(role)),
            ("content", Node::Array(std::mem::take(parts))),
        ]));
    }
}
fn string_node(value: &Value) -> Node<'_> {
    if let Some(value) = value.as_str() {
        text(value)
    } else {
        owned_text(string_value(value))
    }
}
fn joined_text<'a>(values: impl Iterator<Item = Cow<'a, str>>) -> Node<'a> {
    let mut values = values.filter(|value| !value.is_empty());
    let first = values.next().unwrap_or(Cow::Borrowed(""));
    match values.next() {
        None => Node::Str(first),
        Some(second) => {
            let mut value = String::with_capacity(first.len() + second.len());
            value.push_str(&first);
            value.push_str(&second);
            for part in values {
                value.push_str(&part);
            }
            owned_text(value)
        }
    }
}
fn responses_chat(source: &Value) -> Vec<Node<'_>> {
    if let Some(value) = source["input"].as_str() {
        return vec![obj([("role", text("user")), ("content", text(value))])];
    }
    let mut messages = vec![];
    for item in arr(&source["input"]) {
        let kind = s(&item["type"]).trim();
        match kind {
            "function_call" | "custom_tool_call" => {
                let name = call_name(item);
                let args = if kind == "custom_tool_call" {
                    Cow::Owned(stringify(&json!({"input":string_value(&item["input"])})))
                } else if let Some(arguments) = item["arguments"].as_str() {
                    Cow::Borrowed(arguments)
                } else {
                    Cow::Owned(string_value(&item["arguments"]))
                };
                let call = tool_call(call_id(item), Cow::Owned(name), args, true);
                let append = match messages.last_mut() {
                    Some(Node::Object(fields))
                        if fields.iter().any(|(key, value)| {
                            *key == "role"
                                && matches!(value, Node::Str(value) if value == "assistant")
                        }) =>
                    {
                        Some(fields)
                    }
                    _ => None,
                };
                if let Some(fields) = append {
                    match fields.iter_mut().find(|(key, _)| *key == "tool_calls") {
                        Some((_, Node::Array(calls))) => calls.push(call),
                        _ => fields.push(("tool_calls", Node::Array(vec![call]))),
                    }
                } else {
                    messages.push(obj([
                        ("role", text("assistant")),
                        ("content", Node::Null),
                        ("tool_calls", Node::Array(vec![call])),
                    ]));
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                let output = &item["output"];
                let content = if output.is_array() && !arr(output).is_empty() {
                    let recognized = arr(output).iter().all(|part| {
                        matches!(
                            s(&part["type"]).trim(),
                            "text" | "input_text" | "output_text"
                        )
                    });
                    if recognized {
                        let texts = arr(output)
                            .iter()
                            .map(|part| response_text_value(&part["text"]).into_owned())
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>();
                        owned_text(texts.join("\n"))
                    } else {
                        owned_text(string_value(output))
                    }
                } else {
                    string_node(output)
                };
                let mut fields = vec![("role", text("tool")), ("content", content)];
                field_if_text(&mut fields, "tool_call_id", s(&item["call_id"]).trim());
                messages.push(Node::Object(fields));
            }
            _ => {
                let role = s(&item["role"]).trim();
                let content = &item["content"];
                let content = if content.is_null() {
                    text("")
                } else if let Some(value) = content.as_str() {
                    text(value)
                } else {
                    joined_text(
                        arr(content)
                            .iter()
                            .map(|part| response_text_value(&part["text"])),
                    )
                };
                messages.push(obj([
                    ("role", text(if role.is_empty() { "user" } else { role })),
                    ("content", content),
                ]));
            }
        }
    }
    messages
}
fn take_array(header: &mut Value, key: &str) -> Vec<Node<'static>> {
    match header.as_object_mut().unwrap().remove(key) {
        Some(Value::Array(values)) => values.into_iter().map(Node::Owned).collect(),
        _ => vec![],
    }
}
fn role<'a>(node: &'a Node<'_>) -> &'a str {
    match node {
        Node::Object(fields) => fields
            .iter()
            .find_map(|(key, value)| {
                if *key == "role" {
                    if let Node::Str(value) = value {
                        return Some(value.as_ref());
                    }
                }
                None
            })
            .unwrap_or(""),
        _ => "",
    }
}
fn parts_mut<'a, 'b>(node: &'b mut Node<'a>, key: &str) -> Option<&'b mut Vec<Node<'a>>> {
    if let Node::Object(fields) = node {
        if let Some((_, Node::Array(values))) = fields.iter_mut().find(|(field, _)| *field == key) {
            return Some(values);
        }
    }
    None
}
fn node_has(node: &Node<'_>, key: &str) -> bool {
    matches!(node, Node::Object(fields) if fields.iter().any(|(field,_)| *field == key))
}
fn append<'a>(
    messages: &mut Vec<Node<'a>>,
    target_role: &'static str,
    key: &'static str,
    part: Node<'a>,
    gemini: bool,
) {
    if let Some(last) = messages.last_mut() {
        if role(last) == target_role {
            if let Some(parts) = parts_mut(last, key) {
                let mut at = parts.len();
                if gemini && target_role == "model" && node_has(&part, "functionCall") {
                    at = parts
                        .iter()
                        .take_while(|part| node_has(part, "functionCall"))
                        .count();
                } else if gemini && node_has(&part, "functionResponse") {
                    if let Some(index) = parts
                        .iter()
                        .rposition(|part| node_has(part, "functionResponse"))
                    {
                        at = index + 1;
                    }
                }
                parts.insert(at, part);
                return;
            }
        }
    }
    messages.push(obj([
        ("role", text(target_role)),
        (key, Node::Array(vec![part])),
    ]));
}
fn trim_trailing(messages: &mut Vec<Node<'_>>, gemini: bool, diagnostics: &mut Vec<Diagnostic>) {
    let mut end = messages.len();
    while end > 0 {
        let message = &messages[end - 1];
        let has_tool = if let Node::Object(fields) = message {
            fields.iter().find_map(|(key,node)| {
                if *key == if gemini { "parts" } else { "content" } {
                    if let Node::Array(parts) = node {
                        return Some(parts.iter().any(|part| if gemini { node_has(part,"functionCall") }
                            else { matches!(part,Node::Object(fields) if fields.iter().any(|(key,node)| *key == "type" && matches!(node,Node::Str(value) if value == "tool_use"))) }));
                    }
                }
                None
            }).unwrap_or(false)
        } else {
            false
        };
        if role(message) != if gemini { "model" } else { "assistant" } || has_tool {
            break;
        }
        end -= 1;
    }
    if end == 0 || end == messages.len() || role(&messages[end - 1]) != "user" {
        return;
    }
    messages.truncate(end);
    diag(
        diagnostics,
        Severity::Warning,
        "trailing_assistant_omitted",
        "input",
        "assistant output after the last user message or tool result was omitted; the target protocol would continue it as a prefill",
    );
}
fn response_text_parts<'a>(content: &'a Value, gemini: bool) -> Vec<Node<'a>> {
    let mut parts = vec![];
    let texts = if let Some(value) = content.as_str() {
        vec![Cow::Borrowed(value)]
    } else {
        arr(content)
            .iter()
            .map(|part| response_text_value(&part["text"]))
            .collect()
    };
    for value in texts {
        if !value.is_empty() {
            parts.push(if gemini {
                obj([("text", Node::Str(value))])
            } else {
                obj([("type", text("text")), ("text", Node::Str(value))])
            });
        }
    }
    parts
}
fn responses_claude<'a>(
    source: &'a Value,
    header: &mut Value,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<(&'static str, Node<'a>)> {
    header.as_object_mut().unwrap().remove("messages"); // discard the header-only placeholder
    let mut system = take_array(header, "system");
    let mut messages = vec![];
    // A string input is handled separately to avoid borrowing the temporary
    // synthetic item returned by the compatibility helper.
    if let Some(value) = source["input"].as_str() {
        messages.push(obj([
            ("role", text("user")),
            (
                "content",
                Node::Array(if value.is_empty() {
                    vec![obj([("type", text("text")), ("text", text("..."))])]
                } else {
                    vec![obj([("type", text("text")), ("text", text(value))])]
                }),
            ),
        ]));
    } else {
        for item in arr(&source["input"]) {
            match s(&item["type"]).trim() {
                "function_call" | "custom_tool_call" => {
                    let custom = item["type"] == "custom_tool_call";
                    let mut fields = vec![("type", text("tool_use"))];
                    field_if_text(&mut fields, "id", call_id(item));
                    let name = call_name(item);
                    if !name.is_empty() {
                        fields.push(("name", owned_text(name)));
                    }
                    let args = if custom {
                        json!({"input":string_value(&item["input"])})
                    } else {
                        object_value(&item["arguments"], "arguments")
                    };
                    if !args.is_null() {
                        fields.push(("input", Node::Owned(crate::wire::go_interface_value(&args))));
                    }
                    append(
                        &mut messages,
                        "assistant",
                        "content",
                        Node::Object(fields),
                        false,
                    );
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let mut fields = vec![("type", text("tool_result"))];
                    field_if_text(&mut fields, "tool_use_id", call_id(item));
                    let output = &item["output"];
                    let content = if let Some(value) = output.as_str() {
                        text(value)
                    } else if output.is_null() {
                        text("")
                    } else if arr(output).is_empty() {
                        Node::Owned(crate::wire::go_interface_value(output))
                    } else if arr(output).iter().all(|part| {
                        matches!(
                            s(&part["type"]).trim(),
                            "text" | "input_text" | "output_text"
                        )
                    }) {
                        let parts = response_text_parts(output, false);
                        if parts.is_empty() {
                            text("")
                        } else {
                            Node::Array(parts)
                        }
                    } else {
                        owned_text(string_value(output))
                    };
                    fields.push(("content", content));
                    append(
                        &mut messages,
                        "user",
                        "content",
                        Node::Object(fields),
                        false,
                    );
                }
                _ => {
                    let source_role = s(&item["role"]).trim();
                    let role = match source_role {
                        "assistant" => "assistant",
                        "system" | "developer" => "system",
                        _ => "user",
                    };
                    let mut parts = response_text_parts(&item["content"], false);
                    if source_role.is_empty() && parts.is_empty() {
                        continue;
                    }
                    if role == "system" {
                        system.extend(parts);
                        continue;
                    }
                    if parts.is_empty() {
                        parts.push(obj([("type", text("text")), ("text", text("..."))]));
                    }
                    messages.push(obj([("role", text(role)), ("content", Node::Array(parts))]));
                }
            }
        }
    }
    trim_trailing(&mut messages, false, diagnostics);
    if (!messages.is_empty() || !system.is_empty())
        && messages
            .first()
            .is_none_or(|message| role(message) != "user")
    {
        messages.insert(
            0,
            obj([
                ("role", text("user")),
                (
                    "content",
                    Node::Array(vec![obj([("type", text("text")), ("text", text("..."))])]),
                ),
            ]),
        );
    }
    let mut fields = vec![];
    if !messages.is_empty() {
        fields.push(("messages", Node::Array(messages)));
    }
    if !system.is_empty() {
        fields.push(("system", Node::Array(system)));
    }
    fields
}
fn responses_gemini<'a>(
    source: &'a Value,
    header: &mut Value,
    ctx: &Context,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Vec<(&'static str, Node<'a>)>, Error> {
    header.as_object_mut().unwrap().remove("contents");
    // Collect system text from native instructions and ordinary input items in
    // exactly the order used by the existing converter.
    header.as_object_mut().unwrap().remove("systemInstruction");
    let mut systems = vec![];
    if !source["instructions"].is_null() {
        let value = string_value(&source["instructions"]);
        if !value.trim().is_empty() {
            systems.push(value);
        }
    }
    let mut names = BTreeMap::<&str, String>::new();
    let mut contents = vec![];
    if let Some(value) = source["input"].as_str() {
        if !value.is_empty() {
            contents.push(obj([
                ("role", text("user")),
                ("parts", Node::Array(vec![obj([("text", text(value))])])),
            ]));
        }
    } else {
        for item in arr(&source["input"]) {
            match s(&item["type"]).trim() {
                "function_call" | "custom_tool_call" => {
                    let name = call_name(item);
                    let id = call_id(item);
                    let args = if s(&item["type"]).trim() == "custom_tool_call" {
                        json!({"input":string_value(&item["input"])})
                    } else {
                        object_value(&item["arguments"], "arguments")
                    };
                    let mut call = vec![
                        ("name", owned_text(name.clone())),
                        ("args", Node::Owned(crate::wire::go_interface_value(&args))),
                    ];
                    field_if_text(&mut call, "id", id);
                    let mut part = vec![("functionCall", Node::Object(call))];
                    if ctx.options.gemini.function_call_thought_signature_enabled {
                        part.push(("thoughtSignature", text(SIGNATURE)));
                    }
                    if !id.is_empty() {
                        names.insert(id, name);
                    }
                    append(&mut contents, "model", "parts", Node::Object(part), true);
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let id = call_id(item);
                    let name = if s(&item["name"]).trim().is_empty() {
                        names.get(id).cloned().unwrap_or_default()
                    } else {
                        s(&item["name"]).trim().to_owned()
                    };
                    let output = &item["output"];
                    let output = if !arr(output).is_empty()
                        && arr(output).iter().all(|part| {
                            matches!(
                                s(&part["type"]).trim(),
                                "text" | "input_text" | "output_text"
                            )
                        }) {
                        let texts = arr(output)
                            .iter()
                            .map(|part| response_text_value(&part["text"]).into_owned())
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>();
                        response_map(&Value::String(texts.join("\n")))
                    } else {
                        response_map(output)
                    };
                    let mut response = vec![
                        ("name", owned_text(name)),
                        (
                            "response",
                            Node::Owned(crate::wire::go_interface_value(&output)),
                        ),
                    ];
                    field_if_text(&mut response, "id", id);
                    append(
                        &mut contents,
                        "user",
                        "parts",
                        obj([("functionResponse", Node::Object(response))]),
                        true,
                    );
                }
                _ => {
                    let role = match s(&item["role"]).trim() {
                        "assistant" | "model" => "model",
                        "system" | "developer" => "system",
                        _ => "user",
                    };
                    let parts = response_text_parts(&item["content"], true);
                    if role == "system" {
                        if let Some(value) = item["content"].as_str() {
                            if !value.is_empty() {
                                systems.push(value.into());
                            }
                        } else {
                            systems.extend(
                                arr(&item["content"])
                                    .iter()
                                    .map(|part| s(&part["text"]))
                                    .filter(|value| !value.is_empty())
                                    .map(str::to_owned),
                            );
                        }
                    } else if !parts.is_empty() {
                        contents.push(obj([("role", text(role)), ("parts", Node::Array(parts))]));
                    }
                }
            }
        }
    }
    trim_trailing(&mut contents, true, diagnostics);
    let mut fields = vec![(
        "contents",
        if contents.is_empty() {
            Node::Null
        } else {
            Node::Array(contents)
        },
    )];
    if !systems.is_empty() {
        fields.push((
            "systemInstruction",
            obj([(
                "parts",
                Node::Array(vec![obj([("text", owned_text(systems.join("\n")))])]),
            )]),
        ));
    }
    Ok(fields)
}
