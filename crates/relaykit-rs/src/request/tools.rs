//! Tools travel independently of the message conversion path. This prevents a
//! Chat pivot from turning hosted tools into client-side function definitions.
use crate::response::hosted::diagnostic;
use crate::response::{arr, s};
use crate::{
    Context, Diagnostic, Error, Format, ResponsesNamespaceTool, ResponsesToolState, WebSearchCall,
};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
pub(super) mod history;

#[derive(Clone, Default)]
struct Definition {
    kind: String,
    native: String,
    name: String,
    namespace: String,
    raw: Value,
    function: Option<Value>,
    web: Value,
    openapi: bool,
    group: usize,
}
#[derive(Clone, Default)]
struct Choice {
    mode: String,
    kind: String,
    name: String,
    native: String,
    allowed: Vec<String>,
    raw: Value,
    disabled: Option<bool>,
}
pub(super) struct ToolSet {
    source: Format,
    definitions: Vec<Definition>,
    choice: Option<Choice>,
    parallel: Option<bool>,
    config: Value,
    history: Vec<history::HistoryItem>,
    diagnostics: Vec<Diagnostic>,
    state: ResponsesToolState,
}

fn declared_responses_tools(input: &Value) -> Result<Vec<Value>, Error> {
    let mut declarations = Vec::new();
    let mut push = |tools: &Value| -> Result<(), Error> {
        if tools.is_null() {
            return Ok(());
        }
        let tools = tools
            .as_array()
            .ok_or_else(|| Error::message("Responses tools must be an array"))?;
        for tool in tools {
            if !declarations.contains(tool) {
                declarations.push(tool.clone());
            }
        }
        Ok(())
    };
    push(&input["tools"])?;
    for item in arr(&input["input"]) {
        if item["type"] == "additional_tools" {
            push(&item["tools"])?;
        }
    }
    Ok(declarations)
}

fn namespace_children(tool: &Value) -> &[Value] {
    tool["tools"]
        .as_array()
        .filter(|children| !children.is_empty())
        .or_else(|| tool["children"].as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn flattened_namespace_name(namespace: &str, name: &str) -> String {
    let full = format!("{namespace}__{name}");
    if full.len() <= 64 {
        return full;
    }
    let hash = full.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    let suffix = format!("__{:08x}", hash as u32);
    let mut end = 64 - suffix.len();
    while !full.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &full[..end])
}

pub(super) fn responses_tool_state(input: &Value) -> Result<ResponsesToolState, Error> {
    let declarations = declared_responses_tools(input)?;
    let mut state = ResponsesToolState::default();
    let mut top_level_names = BTreeSet::new();
    for tool in &declarations {
        match s(&tool["type"]) {
            "function" | "custom" => {
                let name = s(&tool["name"]).trim();
                if name.is_empty() {
                    return Err(Error::message("Responses client tool requires a name"));
                }
                if !top_level_names.insert(name.to_owned()) {
                    return Err(Error::message(format!(
                        "Responses client tools conflict at name {name:?}"
                    )));
                }
                if tool["type"] == "custom" {
                    state.custom_tool_names.insert(name.to_owned());
                }
            }
            "tool_search" => state.tool_search = true,
            _ => {}
        }
    }
    if state.tool_search && top_level_names.contains("tool_search") {
        return Err(Error::message(
            "built-in tool_search conflicts with a declared client tool",
        ));
    }
    for tool in &declarations {
        if tool["type"] != "namespace" {
            continue;
        }
        let namespace = s(&tool["name"]).trim();
        if namespace.is_empty() {
            return Err(Error::message("namespace tool requires a name"));
        }
        for child in namespace_children(tool) {
            if !matches!(s(&child["type"]), "function" | "custom") {
                return Err(Error::message(
                    "a namespace can declare only function or custom tools",
                ));
            }
            let name = s(&child["name"]).trim();
            if name.is_empty() {
                return Err(Error::message("namespace child tool requires a name"));
            }
            let flattened = flattened_namespace_name(namespace, name);
            if top_level_names.contains(&flattened)
                || state.tool_search && flattened == "tool_search"
            {
                return Err(Error::message(format!(
                    "namespace tool {namespace:?}/{name:?} conflicts with a top-level tool at {flattened:?}"
                )));
            }
            if let Some(previous) = state.namespace_tools.get(&flattened) {
                if previous.namespace == namespace && previous.name == name {
                    continue;
                }
                return Err(Error::message(format!(
                    "namespace tools {:?}/{:?} and {namespace:?}/{name:?} both flatten to {flattened:?}",
                    previous.namespace, previous.name
                )));
            }
            state.namespaces.insert(flattened.clone(), namespace.to_owned());
            state.namespace_tools.insert(
                flattened.clone(),
                ResponsesNamespaceTool {
                    namespace: namespace.to_owned(),
                    name: name.to_owned(),
                },
            );
            if child["type"] == "custom" {
                state.custom_tool_names.insert(flattened);
            }
        }
    }
    Ok(state)
}

fn preserve_definition(
    mut definition: Definition,
    state: &ResponsesToolState,
) -> Definition {
    if definition.native == "tool_search" {
        definition.kind = "function".into();
        definition.name = "tool_search".into();
        definition.function = Some(json!({
            "name":"tool_search",
            "description":"Search and load tools available to the current task.",
            "parameters":{
                "type":"object",
                "properties":{"query":{"type":"string"},"limit":{"type":"integer"}},
                "required":["query"],
            },
        }));
    }
    if !definition.namespace.is_empty() {
        let original = s(&definition.raw["name"]);
        let original = original
            .strip_prefix(&format!("{}__", definition.namespace))
            .unwrap_or(original);
        if let Some(name) = state.flatten_declared(&definition.namespace, original) {
            definition.name = name.to_owned();
            definition.raw["name"] = json!(name);
            if let Some(function) = &mut definition.function {
                function["name"] = json!(name);
            }
        }
    }
    definition
}
fn diag(
    source: Format,
    to: Format,
    path: impl Into<String>,
    code: &str,
    message: impl Into<String>,
    warning: bool,
) -> Diagnostic {
    diagnostic(source, to, path, code, message, warning)
}
fn web_type(t: &str) -> bool {
    matches!(
        t,
        "web_search"
            | "web_search_2025_08_26"
            | "web_search_preview"
            | "web_search_preview_2025_03_11"
    )
}
fn claude_web(t: &str) -> bool {
    matches!(
        t,
        "web_search_20250305" | "web_search_20260209" | "web_search_20260318"
    )
}
fn native_kind(t: &str) -> &str {
    if t == "file_search" {
        "file_search"
    } else if t.starts_with("web_fetch") {
        "web_fetch"
    } else if t == "code_interpreter" || t.starts_with("code_execution") {
        "code_execution"
    } else if t.contains("computer") {
        "computer_use"
    } else if t == "url_context" {
        "url_context"
    } else if t == "mcp" || t == "mcp_toolset" {
        "mcp"
    } else if t == "image_generation" {
        "image_generation"
    } else {
        "native"
    }
}
fn function(v: &Value, parameters: &str, trim: bool) -> Value {
    let mut f = json!({"name":if trim{s(&v["name"]).trim()}else{s(&v["name"])},"description":s(&v["description"]),"parameters":v[parameters]});
    if let Some(strict) = v["strict"].as_bool() {
        f["strict"] = json!(strict);
    }
    f
}
fn location(v: &Value) -> Value {
    let mut out = json!({});
    for k in ["city", "region", "country", "timezone"] {
        let t = s(&v[k]).trim();
        if !t.is_empty() {
            out[k] = json!(t);
        }
    }
    if out == json!({}) { Value::Null } else { out }
}
fn strings(v: &Value) -> Value {
    json!(
        arr(v)
            .iter()
            .filter_map(Value::as_str)
            .filter(|x| !x.trim().is_empty())
            .collect::<Vec<_>>()
    )
}
fn web_spec(v: &Value, claude: bool) -> Value {
    let mut w = json!({});
    for k in ["search_context_size", "response_inclusion"] {
        if !s(&v[k]).trim().is_empty() {
            w[k] = json!(s(&v[k]).trim());
        }
    }
    for k in ["max_uses", "external_web_access", "return_token_budget"] {
        if let Some(v) = v.get(k) {
            w[k] = v.clone();
        }
    }
    for k in ["allowed_domains", "blocked_domains", "allowed_callers"] {
        let value = if k == "allowed_domains" && !claude {
            strings(&v["filters"][k])
        } else {
            strings(&v[k])
        };
        if !arr(&value).is_empty() {
            w[k] = value;
        }
    }
    let l = location(&v["user_location"]);
    if !l.is_null() {
        w["location"] = l;
    }
    w
}
fn responses_definition(v: &Value) -> Result<Definition, Error> {
    if !v.is_object() {
        return Err(Error::message("tool definition must be an object"));
    }
    let typ = s(&v["type"]).trim();
    let mut d = Definition {
        native: typ.into(),
        raw: v.clone(),
        ..Default::default()
    };
    if typ == "function" {
        d.kind = "function".into();
        d.native.clear();
        d.name = s(&v["name"]).trim().into();
        d.function = Some(function(v, "parameters", true));
    } else if web_type(typ) {
        d.kind = "web_search".into();
        d.web = web_spec(v, false);
    } else {
        d.kind = native_kind(typ).into();
    }
    Ok(d)
}
fn responses_definitions(v: &Value) -> Result<Vec<Definition>, Error> {
    if s(&v["type"]).trim() != "namespace" {
        return Ok(vec![responses_definition(v)?]);
    }
    let namespace = s(&v["name"]).trim();
    if namespace.is_empty() {
        return Err(Error::message("namespace tool requires name"));
    }
    let mut defs = vec![];
    for (i, tool) in namespace_children(v).iter().enumerate() {
        let mut tool = tool.clone();
        let name = s(&tool["name"]).trim();
        if !name.is_empty() {
            tool["name"] = json!(crate::namespaced_tool_name(namespace, name));
        }
        let desc = s(&tool["description"]).trim();
        let nsdesc = s(&v["description"]).trim();
        let desc = if nsdesc.is_empty() {
            desc.to_owned()
        } else {
            format!("{nsdesc}\n\n{desc}").trim().into()
        };
        if !desc.is_empty() {
            tool["description"] = json!(desc);
        }
        let mut def = responses_definition(&tool)
            .map_err(|e| Error::message(format!("namespace {namespace:?} tools[{i}]: {e}")))?;
        def.namespace = namespace.into();
        if def.name.is_empty() {
            def.name = s(&tool["name"]).into();
        }
        defs.push(def);
    }
    Ok(defs)
}
fn claude_definition(v: &Value) -> Result<Definition, Error> {
    if !v.is_object() {
        return Err(Error::message("tool definition must be an object"));
    }
    let typ = s(&v["type"]).trim();
    let mut d = Definition {
        native: typ.into(),
        raw: v.clone(),
        name: s(&v["name"]).trim().into(),
        ..Default::default()
    };
    if typ.is_empty() {
        d.kind = "function".into();
        d.function = Some(function(v, "input_schema", true));
    } else if typ.starts_with("web_search") {
        let version = typ.strip_prefix("web_search_").unwrap_or("");
        if version.len() != 8 || version.parse::<u32>().is_err() {
            return Err(Error::message(format!(
                "invalid Claude web-search tool version {typ:?}"
            )));
        }
        if !claude_web(typ) {
            d.kind = "native".into();
            return Ok(d);
        }
        if d.name != "web_search" {
            return Err(Error::message(
                "Claude web-search tool name must be \"web_search\"",
            ));
        }
        d.kind = "web_search".into();
        d.web = web_spec(v, true);
        if let Some(max) = v.get("max_uses") {
            let Some(max) = max.as_i64() else {
                return Err(Error::message("max_uses must be a JSON integer"));
            };
            if !(1..=1000).contains(&max) {
                return Err(Error::message("max_uses must be between 1 and 1000"));
            }
        }
        if !arr(&d.web["allowed_domains"]).is_empty() && !arr(&d.web["blocked_domains"]).is_empty()
        {
            return Err(Error::message(
                "allowed_domains and blocked_domains are mutually exclusive",
            ));
        }
        if !s(&d.web["response_inclusion"]).is_empty() && typ != "web_search_20260318" {
            return Err(Error::message(
                "response_inclusion requires Claude web_search_20260318",
            ));
        }
    } else {
        d.kind = native_kind(typ).into();
    }
    Ok(d)
}
fn choice_string(v: &str) -> Choice {
    let mode = match v.trim().to_lowercase().as_str() {
        "none" => "none",
        "required" | "any" => "required",
        "auto" => "auto",
        _ => "opaque",
    };
    Choice {
        mode: mode.into(),
        raw: if mode == "opaque" {
            json!(v)
        } else {
            Value::Null
        },
        ..Default::default()
    }
}
fn decode_choice(from: Format, v: &Value, defs: &[Definition]) -> Result<Option<Choice>, Error> {
    if v.is_null() {
        return Ok(None);
    }
    if from == Format::Gemini {
        let f = &v["functionCallingConfig"];
        if f.is_null() {
            return Ok(None);
        }
        let mut c = Choice {
            kind: "function".into(),
            ..Choice::default()
        };
        match s(&f["mode"]).trim().to_uppercase().as_str() {
            "NONE" => c.mode = "none".into(),
            "ANY" => {
                c.allowed = arr(&f["allowedFunctionNames"])
                    .iter()
                    .map(|v| s(v).to_owned())
                    .collect();
                c.mode = "required".into();
                if c.allowed.len() == 1 {
                    c.mode = "named".into();
                    c.name = c.allowed.remove(0);
                }
            }
            "" | "AUTO" => c.mode = "auto".into(),
            _ => {
                c.mode = "opaque".into();
                c.raw = f.clone();
            }
        }
        return Ok(Some(c));
    }
    if let Some(text) = v.as_str() {
        if from != Format::Claude {
            return Ok(Some(choice_string(text)));
        }
    }
    if !v.is_object() {
        return Err(Error::message(format!(
            "invalid {from} tool_choice: expected object"
        )));
    }
    let typ = s(&v["type"]).trim();
    let mut c = Choice {
        mode: "opaque".into(),
        raw: v.clone(),
        ..Default::default()
    };
    match from {
        Format::OpenAi => {
            if typ == "function" {
                let name = s(&v["function"]["name"]).trim();
                if name.is_empty() {
                    return Err(Error::message(
                        "Chat function tool_choice requires function.name",
                    ));
                }
                c.mode = "named".into();
                c.kind = "function".into();
                c.name = name.into();
                c.raw = Value::Null;
            }
        }
        Format::OpenAiResponses => {
            c.native = typ.into();
            if typ == "function" {
                let name = s(&v["name"]).trim();
                if name.is_empty() {
                    return Err(Error::message(
                        "Responses function tool_choice requires name",
                    ));
                }
                c.mode = "named".into();
                c.kind = "function".into();
                c.name = crate::namespaced_tool_name(s(&v["namespace"]).trim(), name);
                c.native.clear();
                c.raw = Value::Null;
            } else if web_type(typ) {
                c.mode = "named".into();
                c.kind = "web_search".into();
                c.name = "web_search".into();
            } else {
                c.kind = native_kind(typ).into();
            }
        }
        Format::Claude => {
            c.disabled = v["disable_parallel_tool_use"].as_bool();
            match typ {
                "auto" | "none" => c.mode = typ.into(),
                "any" => c.mode = "required".into(),
                "tool" => {
                    c.mode = "named".into();
                    c.name = s(&v["name"]).trim().into();
                    let matches: Vec<_> = defs
                        .iter()
                        .filter(|d| {
                            if let Some(f) = &d.function {
                                s(&f["name"]) == c.name
                            } else {
                                d.name == c.name
                            }
                        })
                        .collect();
                    if matches.len() > 1 {
                        return Err(Error::message(format!(
                            "Claude tool_choice name {:?} is ambiguous across {} definitions",
                            c.name,
                            matches.len()
                        )));
                    }
                    c.kind = matches
                        .first()
                        .map(|d| d.kind.clone())
                        .unwrap_or("native".into());
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(Some(c))
}
fn strip_fields(out: &mut Cow<'_, Value>, keys: &[&str]) {
    if keys.iter().any(|key| out.get(*key).is_some()) {
        let map = out.to_mut().as_object_mut().unwrap();
        for key in keys {
            map.remove(*key);
        }
    }
}
pub(super) fn extract<'a>(
    from: Format,
    input: &'a Value,
    ctx: &Context,
) -> Result<(Cow<'a, Value>, ToolSet), Error> {
    let mut out = Cow::Borrowed(input);
    let mut set = ToolSet {
        source: from,
        definitions: vec![],
        choice: None,
        parallel: None,
        config: Value::Null,
        history: vec![],
        diagnostics: vec![],
        state: ResponsesToolState::default(),
    };
    if from == Format::OpenAi {
        set.parallel = input["parallel_tool_calls"].as_bool();
        if !input["functions"].is_null() && !input["functions"].is_array() {
            return Err(Error::message("invalid legacy functions: expected array"));
        }
        for f in arr(&input["functions"]) {
            set.definitions.push(Definition {
                kind: "function".into(),
                function: Some(function(f, "parameters", false)),
                ..Default::default()
            });
        }
        for (i, t) in arr(&input["tools"]).iter().enumerate() {
            let typ = s(&t["type"]);
            if typ.is_empty() || typ == "function" {
                let name = s(&t["function"]["name"]);
                let kind = match name {
                    "googleSearch" => "web_search",
                    "codeExecution" => "code_execution",
                    "urlContext" => "url_context",
                    _ => "function",
                };
                set.definitions.push(if kind == "function" {
                    Definition {
                        kind: kind.into(),
                        function: Some(function(&t["function"], "parameters", false)),
                        ..Default::default()
                    }
                } else {
                    Definition {
                        kind: kind.into(),
                        name: name.into(),
                        native: name.into(),
                        web: json!({}),
                        ..Default::default()
                    }
                });
            } else {
                let native = &t["custom"];
                if native.is_null() {
                    return Err(Error::message(format!(
                        "tools[{i}] has unsupported type {typ:?} without a native payload"
                    )));
                }
                set.definitions.push(responses_definition(native)?);
            }
        }
        if !input["web_search_options"].is_null() {
            let v = &input["web_search_options"];
            let mut web = json!({});
            if !s(&v["search_context_size"]).is_empty() {
                web["search_context_size"] = v["search_context_size"].clone();
            }
            if !v["user_location"].is_null() && !v["user_location"].is_object() {
                return Err(Error::message(
                    "invalid web_search_options.user_location: expected object",
                ));
            }
            let l = location(&v["user_location"]["approximate"]);
            if !l.is_null() {
                web["location"] = l;
            }
            set.definitions.push(Definition {
                kind: "web_search".into(),
                native: "web_search_options".into(),
                web,
                ..Default::default()
            });
        }
        set.choice = decode_choice(from, &input["tool_choice"], &set.definitions)?;
        if !input["function_call"].is_null() {
            let v = &input["function_call"];
            let c = if let Some(t) = v.as_str() {
                choice_string(t)
            } else {
                let name = s(&v["name"]).trim();
                if name.is_empty() {
                    return Err(Error::message("legacy function_call requires name"));
                }
                Choice {
                    mode: "named".into(),
                    kind: "function".into(),
                    name: name.into(),
                    ..Default::default()
                }
            };
            if set.choice.is_some() {
                return Err(Error::message(
                    "tool_choice and legacy function_call cannot both be converted",
                ));
            }
            set.choice = Some(c);
        }
        strip_fields(
            &mut out,
            &[
                "tools",
                "tool_choice",
                "web_search_options",
                "functions",
                "function_call",
                "parallel_tool_calls",
            ],
        );
    } else if from == Format::OpenAiResponses {
        if ctx.options.preserve_message_structure {
            set.state = responses_tool_state(input)?;
            ctx.set_responses_tool_state(set.state.clone());
        }
        set.parallel = input["parallel_tool_calls"].as_bool();
        if !input["tools"].is_null() && !input["tools"].is_array() {
            return Err(Error::message("invalid Responses tools: expected array"));
        }
        let declarations = if ctx.options.preserve_message_structure {
            declared_responses_tools(input)?
        } else {
            arr(&input["tools"]).to_vec()
        };
        for (i, t) in declarations.iter().enumerate() {
            set.definitions.extend(
                responses_definitions(t).map_err(|e| Error::message(format!("tools[{i}]: {e}")))?,
            );
        }
        set.choice = decode_choice(from, &input["tool_choice"], &set.definitions)?;
        if input["input"].is_array() {
            let mut changed = false;
            for (i, item) in arr(&input["input"]).iter().enumerate() {
                if s(&item["type"]).trim() == "additional_tools" {
                    changed = true;
                    for (j, t) in arr(&item["tools"]).iter().enumerate() {
                        if ctx.options.preserve_message_structure {
                            continue;
                        }
                        set.definitions.extend(
                            responses_definitions(t).map_err(|e| {
                                Error::message(format!("input[{i}].tools[{j}]: {e}"))
                            })?,
                        );
                    }
                    set.diagnostics.push(diag(from,from,format!("input[{i}]"),"additional_tools_flattened","additional_tools items are sent as top-level tools; their position in the input is not preserved",true));
                }
            }
            if changed {
                let items: Vec<_> = arr(&input["input"])
                    .iter()
                    .filter(|item| s(&item["type"]).trim() != "additional_tools")
                    .cloned()
                    .collect();
                out.to_mut()["input"] = if items.is_empty() {
                    Value::Null
                } else {
                    Value::Array(items)
                };
            }
        }
        if ctx.options.preserve_message_structure {
            set.definitions = set
                .definitions
                .into_iter()
                .map(|definition| preserve_definition(definition, &set.state))
                .collect();
            set.choice = decode_choice(from, &input["tool_choice"], &set.definitions)?;
            if let Some(choice) = &mut set.choice {
                let raw = &input["tool_choice"];
                if let Some(flattened) = set
                    .state
                    .flatten_declared(s(&raw["namespace"]), s(&raw["name"]))
                {
                    choice.name = flattened.to_owned();
                }
                if raw["type"] == "tool_search" {
                    choice.mode = "named".into();
                    choice.kind = "function".into();
                    choice.name = "tool_search".into();
                    choice.native.clear();
                    choice.raw = Value::Null;
                } else if raw["type"] == "namespace" {
                    *choice = choice_string("auto");
                }
            }
        }
        let mut names = BTreeSet::new();
        let mut namespaces = BTreeSet::new();
        set.definitions.retain(|d| {
            if !d.namespace.is_empty() && d.namespace != "functions" && namespaces.insert(d.namespace.clone()) {
                set.diagnostics.push(diag(from, from, "tools", "tool_namespace_flattened",
                    format!("tools in namespace {:?} are sent as functions named {:?}", d.namespace, crate::namespaced_tool_name(&d.namespace, "<name>")), true));
            }
            if !d.namespace.is_empty() && !d.name.is_empty() && names.contains(&d.name) {
                set.diagnostics.push(diag(from, from, "tools", "namespaced_tool_dropped",
                    format!("tool {:?} from namespace {:?} shares its flattened name with another tool and was omitted", d.name, d.namespace), false));
                return false;
            }
            if !d.name.is_empty() { names.insert(d.name.clone()); }
            true
        });
        strip_fields(&mut out, &["tools", "tool_choice", "parallel_tool_calls"]);
    } else if from == Format::Claude {
        if !input["tools"].is_null() && !input["tools"].is_array() {
            return Err(Error::message("invalid Claude tools: expected array"));
        }
        for (i, t) in arr(&input["tools"]).iter().enumerate() {
            set.definitions.push(
                claude_definition(t).map_err(|e| Error::message(format!("tools[{i}]: {e}")))?,
            );
        }
        set.choice = decode_choice(from, &input["tool_choice"], &set.definitions)?;
        set.parallel = input["tool_choice"]["disable_parallel_tool_use"]
            .as_bool()
            .map(|b| !b);
        strip_fields(&mut out, &["tools", "tool_choice"]);
    } else {
        if !input["tools"].is_null() && !input["tools"].is_array() {
            return Err(Error::message("invalid Gemini tools: expected array"));
        }
        set.config = input["toolConfig"].clone();
        set.choice = decode_choice(from, &set.config, &set.definitions)?;
        for (group, t) in arr(&input["tools"]).iter().enumerate() {
            for f in arr(&t["functionDeclarations"]) {
                if !f.is_object() {
                    continue;
                }
                if !f["parameters"].is_null() && !f["parametersJsonSchema"].is_null() {
                    return Err(Error::message(format!(
                        "function {:?} declares both parameters and parametersJsonSchema",
                        s(&f["name"]).trim()
                    )));
                }
                let param = if f["parameters"].is_null() {
                    "parametersJsonSchema"
                } else {
                    "parameters"
                };
                set.definitions.push(Definition {
                    kind: "function".into(),
                    name: s(&f["name"]).trim().into(),
                    function: Some(function(f, param, true)),
                    openapi: param == "parameters",
                    group,
                    raw: json!({"functionDeclarations":[f]}),
                    ..Default::default()
                });
            }
            if let Some(map) = t.as_object() {
                for (k, v) in map {
                    if k == "functionDeclarations" {
                        continue;
                    }
                    let kind = match k.as_str() {
                        "googleSearch" | "googleSearchRetrieval" | "enterpriseWebSearch" => {
                            "web_search"
                        }
                        "codeExecution" => "code_execution",
                        "urlContext" => "url_context",
                        "fileSearch" | "retrieval" => "file_search",
                        "computerUse" => "computer_use",
                        _ => "native",
                    };
                    set.definitions.push(Definition {
                        kind: kind.into(),
                        native: k.clone(),
                        name: s(&t["name"]).trim().into(),
                        raw: json!({k:v}),
                        web: if kind == "web_search" {
                            json!({})
                        } else {
                            Value::Null
                        },
                        group,
                        ..Default::default()
                    });
                }
            }
        }
        strip_fields(&mut out, &["tools", "toolConfig"]);
    }
    if history::needs_extraction(from, &out)? {
        set.history = history::extract(from, out.to_mut())?;
    }
    Ok((out, set))
}

fn custom_tools(set: &ToolSet, to: Format) -> (BTreeMap<usize, Option<Value>>, Vec<Diagnostic>) {
    let mut tools = BTreeMap::new();
    let mut ds = vec![];
    if set.source != Format::OpenAiResponses {
        return (tools, ds);
    }
    let mut names: BTreeSet<String> = set
        .definitions
        .iter()
        .filter_map(|d| d.function.as_ref().map(|f| s(&f["name"]).into()))
        .collect();
    for (i, d) in set.definitions.iter().enumerate() {
        if d.kind != "native" || d.native != "custom" {
            continue;
        }
        let name = s(&d.raw["name"]).trim();
        if name.is_empty() {
            continue;
        }
        if !names.insert(name.into()) {
            tools.insert(i, None);
            ds.push(diag(set.source,to,format!("tools[{i}]"),"custom_tool_name_conflict",format!("custom tool {name:?} shares its name with another tool, so the target protocol cannot tell their calls apart"),false));
            continue;
        }
        let mut desc = vec![];
        if !s(&d.raw["description"]).trim().is_empty() {
            desc.push(s(&d.raw["description"]).trim().to_owned());
        }
        desc.push(
            "This tool takes freeform text. Put the complete raw text in the \"input\" argument."
                .into(),
        );
        let f = &d.raw["format"];
        if s(&f["type"]).trim() == "grammar" && !s(&f["definition"]).trim().is_empty() {
            let syntax = s(&f["syntax"]).trim();
            let hint = match syntax {
                "lark" => "Lark grammar".into(),
                "regex" => "regular expression".into(),
                _ => format!("{syntax} grammar"),
            };
            desc.push(format!(
                "The input must match this {hint}:\n{}",
                s(&f["definition"]).trim()
            ));
        }
        tools.insert(i,Some(json!({"name":name,"description":desc.join("\n\n"),"parameters":{"type":"object","properties":{"input":{"type":"string","description":"Raw input for the tool."}},"required":["input"],"additionalProperties":false}})));
        ds.push(diag(set.source,to,format!("tools[{i}]"),"custom_tool_as_function",format!("the target protocol has no freeform tools; custom tool {name:?} is sent as a function with one string argument and its input format becomes a description hint"),true));
    }
    (tools, ds)
}
fn no_tools(set: &ToolSet, to: Format, ds: &mut Vec<Diagnostic>) {
    if set.choice.is_some() || set.parallel.is_some() {
        ds.push(diag(set.source,to,"tool_choice","tool_choice_without_tools","the target request has no function tools, so tool_choice and parallel_tool_calls were omitted",true));
    }
}
fn cleaned_strings(v: &mut Value) {
    if let Some(m) = v.as_object_mut() {
        m.retain(|_, v| v != "");
    }
}
fn json_schema(v: &Value, depth: usize, dropped: &mut BTreeSet<String>) -> Value {
    let Some(m) = v.as_object() else {
        return v.clone();
    };
    if depth >= 64 {
        return v.clone();
    }
    let mut out = json!({});
    let nullable = v["nullable"] == true;
    for (k, x) in m {
        match k.as_str() {
            "type" => {
                if let Some(t) = x.as_str() {
                    let t = t.trim().to_lowercase();
                    if !t.is_empty() && t != "type_unspecified" {
                        out[k] = json!(t);
                    }
                } else {
                    out[k] = x.clone();
                }
            }
            "nullable" => {}
            "propertyOrdering" => {
                dropped.insert(k.clone());
            }
            "example" => {
                if m.contains_key("examples") {
                    dropped.insert(k.clone());
                } else {
                    out["examples"] = json!([x]);
                }
            }
            "properties" => {
                out[k] = x.clone();
                if let Some(p) = x.as_object() {
                    for (n, p) in p {
                        out[k][n] = json_schema(p, depth + 1, dropped);
                    }
                }
            }
            "items" => out[k] = json_schema(x, depth + 1, dropped),
            "anyOf" => {
                out[k] = if x.is_array() {
                    json!(
                        arr(x)
                            .iter()
                            .map(|x| json_schema(x, depth + 1, dropped))
                            .collect::<Vec<_>>()
                    )
                } else {
                    x.clone()
                };
            }
            "maxItems" | "minItems" | "maxLength" | "minLength" | "maxProperties"
            | "minProperties" => {
                out[k] = s(x)
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .map(|x| json!(x))
                    .unwrap_or(x.clone());
            }
            _ => out[k] = x.clone(),
        }
    }
    if nullable {
        if out["type"].is_string() {
            out["type"] = json!([out["type"], "null"]);
        } else if let Some(a) = out.get_mut("anyOf").and_then(Value::as_array_mut) {
            a.push(json!({"type":"null"}));
        }
        if let Some(a) = out.get_mut("enum").and_then(Value::as_array_mut) {
            if !a.contains(&Value::Null) {
                a.push(Value::Null);
            }
        }
    }
    out
}
fn encode_function(
    from: Format,
    to: Format,
    index: usize,
    function: &Value,
    openapi: bool,
    supports_json_schema: bool,
    ds: &mut Vec<Diagnostic>,
) -> Result<Value, Error> {
    let mut parameters = function["parameters"].clone();
    if openapi && to != Format::Gemini {
        let mut dropped = BTreeSet::new();
        parameters = json_schema(&parameters, 0, &mut dropped);
        if !dropped.is_empty() {
            ds.push(diag(from,to,format!("tools[{index}]"),"gemini_schema_keyword_dropped",format!("JSON Schema has no equivalent for Gemini schema keywords {}; they were dropped",dropped.into_iter().collect::<Vec<_>>().join(", ")),true));
        }
    }
    let mut parameters_field = "parameters";
    if to == Format::Gemini && !parameters.is_null() {
        let (field, converted, misfits) =
            super::gemini_schema::function_parameters(&parameters, supports_json_schema);
        parameters_field = field;
        parameters = converted;
        if !misfits.is_empty() {
            ds.push(diag(
                from,
                to,
                format!("tools[{index}].parameters"),
                "gemini_schema_keyword_dropped",
                format!(
                    "Gemini function parameters cannot carry {}",
                    misfits.join(", ")
                ),
                true,
            ));
        }
    }
    let mut f = json!({"name":s(&function["name"]),"description":s(&function["description"])});
    if let Some(strict) = function["strict"].as_bool() {
        if to == Format::Gemini {
            ds.push(diag(
                from,
                to,
                format!("tools[{index}].strict"),
                "unsupported_function_strict",
                "Gemini does not expose OpenAI function strictness",
                true,
            ));
        } else {
            f["strict"] = json!(strict);
        }
    }
    match to {
        Format::Claude => {
            if parameters.is_null() {
                parameters = json!({});
            }
            if !parameters.is_object() {
                return Err(Error::message(format!(
                    "tools[{index}].input_schema: expected JSON object"
                )));
            }
            if parameters["type"].is_null() {
                parameters["type"] = json!("object");
            }
            if parameters["properties"].is_null() {
                parameters["properties"] = json!({});
            }
            f["input_schema"] = parameters;
            cleaned_strings(&mut f);
            if f.get("name").is_none() {
                f["name"] = json!("");
            }
        }
        Format::Gemini => {
            f[parameters_field] = parameters;
            cleaned_strings(&mut f);
        }
        Format::OpenAiResponses => {
            f["type"] = json!("function");
            f["parameters"] = parameters;
            cleaned_strings(&mut f);
        }
        Format::OpenAi => {
            if !parameters.is_null() {
                f["parameters"] = parameters;
            }
            if f["description"] == "" {
                f.as_object_mut().unwrap().remove("description");
            }
            f = json!({"type":"function","function":f});
        }
    }
    Ok(f)
}
fn web_diagnostics(from: Format, to: Format, i: usize, w: &Value) -> Vec<Diagnostic> {
    let mut ds = vec![];
    let p = format!("tools[{i}]");
    let mut add = |field: &str, code: &str, msg: &str, warn: bool| {
        ds.push(diag(from, to, format!("{p}{field}"), code, msg, warn))
    };
    let allowed = !arr(&w["allowed_domains"]).is_empty();
    let blocked = !arr(&w["blocked_domains"]).is_empty();
    let callers = !arr(&w["allowed_callers"]).is_empty();
    let max = !w["max_uses"].is_null();
    let ext = w.get("external_web_access").is_some();
    let incl = !s(&w["response_inclusion"]).is_empty();
    let budget = w.get("return_token_budget").is_some();
    let context = !s(&w["search_context_size"]).is_empty();
    match to {
        Format::OpenAi => {
            if allowed || blocked {
                add(
                    ".domains",
                    "unsupported_domain_filter",
                    "Chat Completions web_search_options cannot preserve domain access constraints",
                    false,
                );
            }
            if max {
                add(
                    ".max_uses",
                    "unsupported_search_limit",
                    "Chat Completions cannot preserve Claude max_uses",
                    false,
                );
            }
            if callers || ext {
                add(
                    "",
                    "unsupported_search_controls",
                    "Chat Completions cannot preserve caller or external-access constraints",
                    false,
                );
            }
            if incl || budget {
                add(
                    "",
                    "unsupported_search_tuning",
                    "Chat Completions cannot preserve response-inclusion or return-token tuning",
                    true,
                );
            }
        }
        Format::OpenAiResponses => {
            if max {
                add(
                    ".max_uses",
                    "unsupported_search_limit",
                    "OpenAI Responses cannot preserve Claude max_uses",
                    false,
                );
            }
            if blocked {
                add(
                    ".blocked_domains",
                    "unsupported_blocked_domains",
                    "OpenAI Responses web search supports allow filters but not Claude blocked_domains",
                    false,
                );
            }
            if callers {
                add(
                    "",
                    "unsupported_search_controls",
                    "OpenAI Responses cannot preserve Claude caller constraints",
                    false,
                );
            }
            if incl {
                add(
                    ".response_inclusion",
                    "unsupported_search_tuning",
                    "OpenAI Responses cannot preserve Claude response-inclusion tuning",
                    true,
                );
            }
        }
        Format::Claude => {
            if context {
                add(
                    ".search_context_size",
                    "unsupported_search_context_size",
                    "Claude has no equivalent for OpenAI search_context_size; max_uses is deliberately not inferred",
                    true,
                );
            }
            if ext {
                add(
                    ".external_web_access",
                    "unsupported_search_controls",
                    "Claude cannot preserve OpenAI external-web access constraints",
                    false,
                );
            }
            if budget {
                add(
                    ".return_token_budget",
                    "unsupported_search_tuning",
                    "Claude cannot preserve OpenAI return-token tuning",
                    true,
                );
            }
        }
        Format::Gemini => {
            if allowed || blocked || ext || max || callers {
                add(
                    "",
                    "unsupported_search_constraints",
                    "Gemini Google Search cannot preserve source web-search access or execution constraints",
                    false,
                );
            }
            if !w["location"].is_null() || context || incl || budget {
                add(
                    "",
                    "unsupported_search_tuning",
                    "Gemini Google Search cannot preserve source web-search location or result tuning",
                    true,
                );
            }
        }
    }
    ds
}
fn native_search_diagnostics(
    from: Format,
    to: Format,
    i: usize,
    d: &Definition,
) -> Vec<Diagnostic> {
    if from != Format::Gemini {
        return vec![];
    }
    let (code, msg, field) = match d.native.as_str() {
        "googleSearch" => {
            let v = &d.raw["googleSearch"];
            if v.is_null() || v.as_object().is_some_and(|m| m.is_empty()) {
                return vec![];
            }
            (
                "unsupported_native_search_config",
                format!("{to} cannot preserve Gemini googleSearch configuration"),
                ".googleSearch",
            )
        }
        "googleSearchRetrieval" => (
            "legacy_search_semantics_unrepresentable",
            format!("{to} cannot preserve Gemini legacy dynamic-retrieval semantics"),
            ".googleSearchRetrieval",
        ),
        "enterpriseWebSearch" => (
            "enterprise_search_semantics_unrepresentable",
            format!(
                "{to} cannot replace Gemini enterprise search with public web search without changing its data source"
            ),
            ".enterpriseWebSearch",
        ),
        _ => (
            "unverified_search_mapping",
            format!(
                "{to} has no verified mapping for Gemini search tool {:?}",
                d.native
            ),
            "",
        ),
    };
    vec![diag(
        from,
        to,
        format!("tools[{i}]{field}"),
        code,
        msg,
        false,
    )]
}
fn default_search(
    from: Format,
    to: Format,
    i: usize,
    d: &Definition,
    out: &mut Value,
    ctx: &Context,
) -> Result<Option<Value>, Error> {
    let w = &d.web;
    Ok(match to {
        Format::OpenAi => {
            let mut v = json!({});
            if !s(&w["search_context_size"]).is_empty() {
                v["search_context_size"] = w["search_context_size"].clone();
            }
            if !w["location"].is_null() {
                v["user_location"] = json!({"type":"approximate","approximate":w["location"]});
            }
            out["web_search_options"] = v;
            None
        }
        Format::OpenAiResponses => {
            let mut v = json!({"type":if from==Format::OpenAiResponses&&!d.native.is_empty(){d.native.as_str()}else{"web_search"}});
            for k in [
                "search_context_size",
                "external_web_access",
                "return_token_budget",
            ] {
                if let Some(x) = w.get(k) {
                    v[k] = x.clone();
                }
            }
            if !w["location"].is_null() {
                v["user_location"] = w["location"].clone();
                v["user_location"]["type"] = json!("approximate");
            }
            if !arr(&w["allowed_domains"]).is_empty() {
                v["filters"] = json!({"allowed_domains":w["allowed_domains"]});
            }
            Some(v)
        }
        Format::Claude => {
            let typ = if from == Format::Claude && claude_web(&d.native) {
                d.native.as_str()
            } else if !ctx.options.claude.web_search_tool_version.is_empty() {
                let t = &ctx.options.claude.web_search_tool_version;
                if !claude_web(t) {
                    return Err(Error::message(format!(
                        "unsupported Claude web-search tool version {t:?}"
                    )));
                }
                t
            } else {
                "web_search_20250305"
            };
            if !arr(&w["allowed_domains"]).is_empty() && !arr(&w["blocked_domains"]).is_empty() {
                return Err(Error::message(format!(
                    "tools[{i}]: allowed_domains and blocked_domains are mutually exclusive"
                )));
            }
            if !s(&w["response_inclusion"]).is_empty() && typ != "web_search_20260318" {
                return Err(Error::message(format!(
                    "tools[{i}].response_inclusion requires Claude web_search_20260318"
                )));
            }
            let mut v = json!({"type":typ,"name":"web_search"});
            if !w["location"].is_null() {
                v["user_location"] = w["location"].clone();
                v["user_location"]["type"] = json!("approximate");
            }
            for k in [
                "allowed_domains",
                "blocked_domains",
                "max_uses",
                "allowed_callers",
                "response_inclusion",
            ] {
                if let Some(x) = w.get(k) {
                    v[k] = x.clone();
                }
            }
            Some(v)
        }
        Format::Gemini => Some(json!({"googleSearch":{}})),
    })
}
fn encode_choice(
    source: Format,
    to: Format,
    choice: Option<&Choice>,
    parallel: Option<bool>,
    ds: &mut Vec<Diagnostic>,
) -> Option<Value> {
    let unsupported = |ds: &mut Vec<Diagnostic>, msg: &str| {
        ds.push(diag(
            source,
            to,
            "tool_choice",
            "unsupported_tool_choice",
            msg,
            false,
        ));
        None
    };
    if to == Format::Claude && source == Format::Claude {
        if let Some(c) = choice {
            if !c.raw.is_null() {
                return Some(c.raw.clone());
            }
        }
    }
    if choice.is_none() {
        if to == Format::Claude {
            if let Some(parallel) = parallel {
                return Some(json!({"type":"auto","disable_parallel_tool_use":!parallel}));
            }
        }
        return None;
    }
    let c = choice.unwrap();
    match to {
        Format::OpenAi => match c.mode.as_str() {
            "auto" | "none" | "required" => Some(json!(c.mode)),
            "named" if c.kind == "function" => {
                Some(json!({"type":"function","function":{"name":c.name}}))
            }
            "named" if c.kind == "web_search" => {
                ds.push(diag(source,to,"tool_choice","implicit_search_choice","Chat Completions expresses hosted search through web_search_options instead of tool_choice",true));
                None
            }
            _ => unsupported(
                ds,
                "Chat Completions cannot represent the source hosted tool choice",
            ),
        },
        Format::OpenAiResponses => match c.mode.as_str() {
            "auto" | "none" | "required" => Some(json!(c.mode)),
            "named" => {
                if !c.native.is_empty() && !c.raw.is_null() {
                    Some(c.raw.clone())
                } else if c.kind == "function" {
                    Some(json!({"type":"function","name":c.name}))
                } else if c.kind == "web_search" {
                    Some(json!({"type":"web_search"}))
                } else {
                    unsupported(
                        ds,
                        "OpenAI Responses has no verified hosted tool-choice mapping",
                    )
                }
            }
            "opaque" if source == Format::OpenAiResponses => Some(c.raw.clone()),
            _ => unsupported(
                ds,
                "OpenAI Responses cannot reconstruct the source complex tool choice",
            ),
        },
        Format::Claude => {
            let mut v = match c.mode.as_str() {
                "auto" | "none" => json!({"type":c.mode}),
                "required" => json!({"type":"any"}),
                "named" if c.kind == "function" || c.kind == "web_search" => {
                    json!({"type":"tool","name":c.name})
                }
                "named" => {
                    return unsupported(ds, "Claude has no verified hosted tool-choice mapping");
                }
                _ => {
                    return unsupported(
                        ds,
                        "Claude cannot represent the source complex tool-choice policy",
                    );
                }
            };
            if v["type"] != "none" {
                if let Some(p) = parallel {
                    v["disable_parallel_tool_use"] = json!(!p);
                } else if let Some(disabled) = c.disabled {
                    v["disable_parallel_tool_use"] = json!(disabled);
                }
            }
            Some(v)
        }
        Format::Gemini => {
            let mut v = json!({});
            if !c.allowed.is_empty() {
                v["mode"] = json!("ANY");
                v["allowedFunctionNames"] = json!(c.allowed);
            } else {
                match c.mode.as_str() {
                    "auto" => v["mode"] = json!("AUTO"),
                    "none" => v["mode"] = json!("NONE"),
                    "required" => v["mode"] = json!("ANY"),
                    "named" if c.kind == "function" => {
                        v["mode"] = json!("ANY");
                        v["allowedFunctionNames"] = json!([c.name]);
                    }
                    "named" => {
                        return unsupported(
                            ds,
                            "Gemini generateContent does not expose an equivalent hosted-tool choice",
                        );
                    }
                    _ => {
                        return unsupported(
                            ds,
                            "Gemini cannot represent the source complex tool-choice policy",
                        );
                    }
                }
            }
            Some(json!({"functionCallingConfig":v}))
        }
    }
}
pub(super) fn attach(
    to: Format,
    out: &mut Value,
    set: ToolSet,
    ctx: &Context,
) -> Result<Vec<Diagnostic>, Error> {
    let from = set.source;
    let mut ds = set.diagnostics.clone();
    if set.definitions.is_empty()
        && set.choice.is_none()
        && set.parallel.is_none()
        && set.config.is_null()
        && set.history.is_empty()
        && ds.is_empty()
    {
        if from == Format::OpenAiResponses {
            ctx.set_responses_tool_state(ResponsesToolState::default());
        }
        return Ok(ds);
    }
    let (custom, custom_ds) = custom_tools(&set, to);
    let mut tool_state = if ctx.options.preserve_message_structure {
        set.state.clone()
    } else {
        ResponsesToolState::default()
    };
    if from == Format::OpenAiResponses {
        for f in custom.values().flatten() {
            tool_state.custom_tool_names.insert(s(&f["name"]).into());
        }
        for d in &set.definitions {
            if !d.namespace.is_empty() && d.namespace != "functions" && !d.name.is_empty() {
                tool_state
                    .namespaces
                    .insert(d.name.clone(), d.namespace.clone());
            }
        }
    }
    if to == Format::Gemini && from == Format::Gemini {
        let mut groups = BTreeMap::<usize, Value>::new();
        for (i, d) in set.definitions.iter().enumerate() {
            if !d.raw.is_object() {
                return Err(Error::message(format!(
                    "tools[{i}]: missing native Gemini tool payload"
                )));
            }
            let group = groups.entry(d.group).or_insert(json!({}));
            for (k, v) in d.raw.as_object().unwrap() {
                if k == "functionDeclarations" {
                    if group[k].is_null() {
                        group[k] = json!([]);
                    }
                    group[k]
                        .as_array_mut()
                        .unwrap()
                        .extend(arr(v).iter().cloned());
                } else {
                    group[k] = v.clone();
                }
            }
        }
        if !groups.is_empty() {
            out["tools"] = json!(groups.into_values().collect::<Vec<_>>());
        }
        if !set.config.is_null() {
            out["toolConfig"] = set.config.clone();
        }
        ds.extend(history::attach(from, to, out, set.history)?);
        return Ok(ds);
    }
    if to != Format::OpenAiResponses {
        ds.extend(custom_ds);
    }
    let supports_json_schema = to == Format::Gemini
        && ctx
            .options
            .gemini
            .supports_json_schema
            .as_ref()
            .is_some_and(|supports| supports(ctx.upstream_model()));
    let mut tools = vec![];
    let mut functions = vec![];
    let mut chat_search_encoded = false;
    let forced = set
        .choice
        .as_ref()
        .is_some_and(|c| c.mode == "named" && c.kind == "web_search");
    for (i, d) in set.definitions.iter().enumerate() {
        let path = format!("tools[{i}]");
        if d.kind == "function" {
            if let Some(f) = &d.function {
                let f = encode_function(from, to, i, f, d.openapi, supports_json_schema, &mut ds)?;
                if to == Format::Gemini {
                    functions.push(f);
                } else {
                    tools.push(f);
                }
            }
            continue;
        }
        if d.kind == "web_search" {
            ds.extend(native_search_diagnostics(from, to, i, d));
            if to == Format::OpenAi && (!out["web_search_options"].is_null() || chat_search_encoded)
            {
                ds.push(diag(
                    from,
                    to,
                    path,
                    "duplicate_hosted_web_search",
                    "Chat Completions carries one hosted web search; this definition was dropped",
                    false,
                ));
                continue;
            }
            if let Some(encoder) = &ctx.options.web_search {
                let call = WebSearchCall {
                    source: from,
                    target: to,
                    native_type: d.native.clone(),
                    index: i,
                    forced,
                    spec: d.web.clone(),
                };
                if let Some(mut result) =
                    encoder(&call, out).map_err(|e| Error::message(format!("tools[{i}]: {e}")))?
                {
                    if result.omitted {
                        result.diagnostics.push(diag(from,to,&path,"hosted_web_search_omitted","the upstream has no hosted web search for this protocol; the web search tool was omitted",false));
                    }
                    ds.extend(result.diagnostics);
                    if !result.omitted {
                        if let Some(t) = result.native_tool {
                            if to == Format::OpenAi {
                                let index = tools.len().to_string();
                                out["__relaykit_native_chat_tools"][&index] = t.clone();
                            }
                            tools.push(t);
                        } else if let Some(t) = result.tool {
                            tools.push(t);
                        }
                    }
                    if to == Format::OpenAi {
                        chat_search_encoded = true;
                    }
                    continue;
                }
            }
            if let Some(t) = default_search(from, to, i, d, out, ctx)? {
                tools.push(t);
            }
            ds.extend(web_diagnostics(from, to, i, &d.web));
            continue;
        }
        if ((to == Format::Claude && from == Format::Claude)
            || (to == Format::OpenAiResponses && from == Format::OpenAiResponses))
            && !d.raw.is_null()
        {
            tools.push(d.raw.clone());
            continue;
        }
        if to == Format::Gemini && (d.kind == "code_execution" || d.kind == "url_context") {
            if d.native == "codeExecution" {
                tools.push(json!({"codeExecution":{}}));
            } else if d.native == "urlContext" {
                tools.push(json!({"urlContext":{}}));
            } else {
                ds.push(diag(
                    from,
                    to,
                    path,
                    "unverified_tool_mapping",
                    if d.kind == "code_execution" {
                        "code execution semantics differ across providers"
                    } else {
                        "URL context has no verified mapping from the source protocol"
                    },
                    false,
                ));
            }
            continue;
        }
        if let Some(f) = custom.get(&i) {
            if let Some(f) = f {
                let f = encode_function(from, to, i, f, false, false, &mut ds)?;
                if to == Format::Gemini {
                    functions.push(f);
                } else {
                    tools.push(f);
                }
            }
            continue;
        }
        let (code, msg) = match to {
            Format::OpenAi => (
                "unsupported_hosted_tool",
                format!(
                    "OpenAI Chat Completions cannot represent hosted tool {:?}",
                    d.native
                ),
            ),
            Format::OpenAiResponses => (
                "unsupported_hosted_tool",
                format!(
                    "OpenAI Responses has no verified mapping for hosted tool {:?}",
                    d.native
                ),
            ),
            Format::Claude => (
                "unsupported_hosted_tool",
                format!(
                    "Claude Messages has no verified mapping for hosted tool {:?}",
                    d.native
                ),
            ),
            Format::Gemini if from == Format::OpenAiResponses && d.kind == "native" => (
                "unsupported_opaque_tool",
                format!(
                    "Gemini cannot represent OpenAI opaque tool {:?}; the definition was omitted",
                    d.native
                ),
            ),
            _ => (
                "unsupported_hosted_tool",
                format!(
                    "Gemini generateContent has no verified mapping for hosted tool {:?}",
                    d.native
                ),
            ),
        };
        ds.push(diag(from, to, path, code, msg, false));
    }
    let function_count = functions.len();
    if !functions.is_empty() {
        tools.push(json!({"functionDeclarations":functions}));
    }
    let has_tools = if to == Format::Gemini {
        function_count > 0
    } else {
        !tools.is_empty()
    };
    if !tools.is_empty() {
        out["tools"] = Value::Array(tools);
    }
    ds.extend(history::attach(from, to, out, set.history.clone())?);
    let mut choice = set.choice.clone();
    if to != Format::OpenAiResponses {
        if let Some(c) = &choice {
            if c.mode == "opaque" && c.native == "custom" {
                let mut name = crate::namespaced_tool_name(
                    s(&c.raw["namespace"]).trim(),
                    s(&c.raw["name"]).trim(),
                );
                if ctx.options.preserve_message_structure {
                    if let Some(flattened) = tool_state.flatten_declared(
                        s(&c.raw["namespace"]).trim(),
                        s(&c.raw["name"]).trim(),
                    ) {
                        name = flattened.to_owned();
                    }
                }
                if tool_state.custom_tool_names.contains(&name) {
                    choice = Some(Choice {
                        mode: "named".into(),
                        kind: "function".into(),
                        name,
                        ..Default::default()
                    });
                }
            }
        }
    }
    if chat_search_encoded && forced {
        choice = None;
    }
    if !has_tools {
        let mut missing = ToolSet {
            source: from,
            definitions: vec![],
            choice,
            parallel: set.parallel,
            config: Value::Null,
            history: vec![],
            diagnostics: vec![],
            state: ResponsesToolState::default(),
        };
        no_tools(&missing, to, &mut ds);
        missing.choice = None;
    } else {
        if to != Format::Gemini {
            if let Some(c) = &mut choice {
                if c.allowed.len() == 1 {
                    c.mode = "named".into();
                    c.kind = "function".into();
                    c.name = c.allowed[0].clone();
                } else if c.allowed.len() > 1 {
                    ds.push(diag(from,to,"tool_choice.allowed_function_names","allowed_function_subset_unrepresentable",format!("{to} cannot restrict a required tool call to Gemini's {}-name function subset",c.allowed.len()),false));
                }
                c.allowed.clear();
            }
        }
        if to == Format::Claude && from != Format::Claude && out["thinking"]["type"] == "enabled" {
            if let Some(c) = &mut choice {
                if c.mode == "required" || c.mode == "named" {
                    c.mode = "auto".into();
                    ds.push(diag(from,to,"tool_choice","forced_tool_choice_downgraded","Claude rejects forced tool use while manual extended thinking is enabled; tool_choice was sent as auto, so the model may answer without calling a tool",ctx.options.preserve_message_structure));
                }
            }
        }
        if let Some(value) = encode_choice(from, to, choice.as_ref(), set.parallel, &mut ds) {
            out[if to == Format::Gemini {
                "toolConfig"
            } else {
                "tool_choice"
            }] = value;
        }
        if matches!(to, Format::OpenAi | Format::OpenAiResponses) {
            if let Some(p) = set.parallel {
                out["parallel_tool_calls"] = json!(p);
            }
        }
        if to == Format::Gemini && set.parallel == Some(false) {
            ds.push(diag(from,to,"parallel_tool_calls","unsupported_parallel_tool_control","Gemini generateContent does not expose a request field equivalent to parallel_tool_calls",false));
        }
    }
    for d in &mut ds {
        d.from = from;
        d.to = to;
    }
    crate::core::reject_loss(ctx.options.tool_loss_policy, &ds)?;
    if from == Format::OpenAiResponses {
        ctx.set_responses_tool_state(tool_state);
    }
    Ok(ds)
}
