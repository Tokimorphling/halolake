//! Stateful, transport-independent response stream conversion.
//!
//! Feed decoded protocol events in arrival order, then call [`ResponseStreamState::finalize`]
//! once the upstream closes. No runtime, networking, SSE parser, or executor is required.
use crate::response::{
    claude_usage, gemini_usage, usage_from_chat, usage_from_claude, usage_from_gemini,
    usage_from_responses,
};
use crate::{Context, Error, Format};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
mod accumulator;
mod chat_responses;
mod claude;
pub(crate) mod claude_responses;
mod diagnostics;
pub mod event;
mod gemini;
mod hosted;
mod responses_chat;
pub use accumulator::ResponsesBufferedAccumulator;
use chat_responses::ChatResponses;
use claude::{ChatClaude, ClaudeChat, ResponsesClaude};
use claude_responses::ClaudeResponses;
use gemini::{ChatGemini, GeminiChat};
pub use hosted::{
    ClaudeHostedStreamBridge, GeminiHostedStreamBridge, HostedToolResult, HostedToolStart,
};
use responses_chat::ResponsesChat;

/// Stable metadata and optional wire features for a single conversion stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamOptions {
    pub id: String,
    pub model: String,
    pub created_at: i64,
    pub include_usage: bool,
    /// Emit monotonically increasing Responses `sequence_number` fields.
    /// Off by default to preserve the upstream package's legacy wire contract.
    pub emit_sequence_number: bool,
    /// Retain extension-aware native history and event ordering for the host.
    pub preserve_message_structure: bool,
    /// Compatibility facades may treat an unpaired buffered web-search call as
    /// completed; the low-level stream state keeps the strict default.
    pub allow_unpaired_hosted_search: bool,
}

/// An immutable protocol event normalized once at the input boundary.
///
/// This prepared JSON path lets callers reuse decoded events without repeating
/// DTO validation on every conversion. It retains the diagnostic behavior of
/// [`ResponseStreamState::convert_chunk`], including omitted Gemini usage.
#[derive(Debug, Clone)]
pub struct PreparedStreamChunk {
    format: Format,
    value: Value,
    diagnostic_overrides: Vec<(Format, Vec<crate::Diagnostic>)>,
}
impl PreparedStreamChunk {
    pub fn from_slice(format: Format, input: &[u8]) -> Result<Self, Error> {
        Self::from_value(format, serde_json::from_slice(input)?)
    }
    pub fn from_value(format: Format, input: Value) -> Result<Self, Error> {
        if input.is_null() {
            return Err(Error::message("response is nil"));
        }
        let formats = [
            Format::OpenAi,
            Format::OpenAiResponses,
            Format::Claude,
            Format::Gemini,
        ];
        let original_diagnostics =
            formats.map(|target| diagnostics::inspect(format, target, &input));
        let reported_gemini_usage = format == Format::Gemini && reported_gemini_usage(&input);
        let mut value = crate::wire::normalize_stream_owned(format, input)?;
        if format == Format::Gemini && !reported_gemini_usage {
            value.as_object_mut().unwrap().remove("usageMetadata");
        }
        let diagnostic_overrides = formats
            .into_iter()
            .zip(original_diagnostics)
            .filter(|(target, original)| *original != diagnostics::inspect(format, *target, &value))
            .collect();
        Ok(Self {
            format,
            value,
            diagnostic_overrides,
        })
    }
    pub fn format(&self) -> Format {
        self.format
    }
    /// The prepared event. An absent Gemini `usageMetadata` remains absent.
    pub fn as_value(&self) -> &Value {
        &self.value
    }
}

#[derive(Debug)]
pub struct ResponseStreamState {
    pub from: Format,
    pub to: Format,
    steps: Vec<Step>,
    usage: Option<Value>,
    usage_needs_normalization: bool,
    diagnostics: Vec<crate::Diagnostic>,
    pending_diagnostics: Vec<crate::Diagnostic>,
    failed: bool,
    preserve_message_structure: bool,
}
#[derive(Debug)]
enum Step {
    ChatResponses(ChatResponses),
    ResponsesChat(ResponsesChat),
    ChatClaude(ChatClaude),
    ClaudeChat(ClaudeChat),
    ClaudeResponses(ClaudeResponses),
    ResponsesClaude(ResponsesClaude),
    ChatGemini(ChatGemini),
    GeminiChat(GeminiChat),
}
impl ResponseStreamState {
    /// Select a registered converter by canonical ID or legacy response alias.
    pub fn new_by_id(id: &str, options: StreamOptions) -> Result<Self, Error> {
        let spec = crate::lookup_converter(id).ok_or_else(|| {
            Error::message(format!("response converter {id:?} is not registered"))
        })?;
        Self::new(spec.from, spec.to, options)
    }
    pub fn converter(&self) -> &'static str {
        crate::converter(self.from, self.to)
            .map(|s| s.id)
            .unwrap_or("")
    }
    pub fn quality(&self) -> Option<crate::Quality> {
        crate::converter(self.from, self.to).map(|s| s.quality)
    }
    pub fn conversion_steps(&self) -> Vec<crate::ConversionStep> {
        if matches!(self.steps.first(), Some(Step::ClaudeResponses(_))) {
            return vec![crate::ConversionStep {
                converter: self.converter().to_string(),
                from: self.from,
                to: self.to,
            }];
        }
        crate::conversion_path(self.from, self.to, true)
            .windows(2)
            .map(|p| crate::ConversionStep {
                converter: crate::converter(p[0], p[1]).unwrap().id.into(),
                from: p[0],
                to: p[1],
            })
            .collect()
    }
    pub fn new(from: Format, to: Format, options: StreamOptions) -> Result<Self, Error> {
        let mut steps = Vec::new();
        if from != to {
            if options.preserve_message_structure
                && from == Format::Claude
                && to == Format::OpenAiResponses
            {
                steps.push(Step::ClaudeResponses(ClaudeResponses::new(&options)));
            } else if from == Format::OpenAiResponses && to == Format::Claude {
                steps.push(Step::ResponsesClaude(ResponsesClaude::new(&options)));
            } else {
                if from != Format::OpenAi {
                    steps.push(match from {
                        Format::OpenAiResponses => {
                            Step::ResponsesChat(ResponsesChat::new(&options))
                        }
                        Format::Claude => Step::ClaudeChat(ClaudeChat::new(&options)),
                        Format::Gemini => Step::GeminiChat(GeminiChat::new(&options)),
                        Format::OpenAi => unreachable!(),
                    });
                }
                if to != Format::OpenAi {
                    steps.push(match to {
                        Format::OpenAiResponses => {
                            Step::ChatResponses(ChatResponses::new(&options))
                        }
                        Format::Claude => Step::ChatClaude(ChatClaude::new(&options)),
                        Format::Gemini => Step::ChatGemini(ChatGemini::default()),
                        Format::OpenAi => unreachable!(),
                    });
                }
            }
        }
        Ok(Self {
            from,
            to,
            steps,
            usage: None,
            usage_needs_normalization: false,
            diagnostics: Vec::new(),
            pending_diagnostics: Vec::new(),
            failed: false,
            preserve_message_structure: options.preserve_message_structure,
        })
    }
    pub fn convert_chunk(&mut self, ctx: &Context, input: &Value) -> Result<Vec<Value>, Error> {
        if self.failed {
            return Ok(Vec::new());
        }
        if input.is_null() {
            return Err(Error::message("response is nil"));
        }
        if self.preserve_message_structure
            && let Some((code, message, param)) = protocol_error(input)
        {
            return Ok(self.fail_stream(&code, &message, &param));
        }
        self.remember_diagnostics(diagnostics::inspect(self.from, self.to, input));
        let reported_gemini_usage = self.from == Format::Gemini && reported_gemini_usage(input);
        let mut input = if self.preserve_message_structure {
            input.clone()
        } else {
            crate::wire::normalize_stream(self.from, input)?
        };
        if self.steps.is_empty() {
            self.remember_usage(if self.from == Format::Gemini && !reported_gemini_usage {
                None
            } else {
                canonical(self.from, &input, 0)
            });
            return Ok(vec![input]);
        }
        if self.from == Format::Gemini && !reported_gemini_usage {
            input.as_object_mut().unwrap().remove("usageMetadata");
        }
        let (values, usage) = self.execute(ctx, vec![Cow::Owned(input)], 0)?;
        // The upstream registry also overlays the returned snapshot at this
        // boundary, which computes provider-native totals after the first frame.
        self.remember_usage(usage);
        Ok(values)
    }
    /// Convert an immutable event that has already crossed the DTO boundary.
    pub fn convert_prepared_chunk(
        &mut self,
        ctx: &Context,
        input: &PreparedStreamChunk,
    ) -> Result<Vec<Value>, Error> {
        if input.format != self.from {
            return Err(Error::message(format!(
                "response stream converter {:?} expects {} response, got {}",
                self.converter(),
                self.from,
                input.format,
            )));
        }
        let inspected = input
            .diagnostic_overrides
            .iter()
            .find(|(target, _)| *target == self.to)
            .map(|(_, diagnostics)| diagnostics.clone())
            .unwrap_or_else(|| diagnostics::inspect(self.from, self.to, &input.value));
        self.remember_diagnostics(inspected);
        if self.steps.is_empty() {
            self.remember_usage(canonical(self.from, &input.value, 0));
            return Ok(vec![crate::wire::normalize_stream(
                self.from,
                &input.value,
            )?]);
        }
        let (values, usage) = self.execute(ctx, vec![Cow::Borrowed(&input.value)], 0)?;
        self.remember_usage(usage);
        Ok(values)
    }
    fn remember_diagnostics(&mut self, diagnostics: Vec<crate::Diagnostic>) {
        for d in diagnostics {
            if !self.diagnostics.iter().any(|seen| {
                seen.code == d.code
                    && seen.path == d.path
                    && seen.severity == d.severity
                    && seen.from == d.from
                    && seen.to == d.to
            }) {
                self.pending_diagnostics.push(d.clone());
                self.diagnostics.push(d);
            }
        }
    }
    fn execute(
        &mut self,
        ctx: &Context,
        mut values: Vec<Cow<'_, Value>>,
        start: usize,
    ) -> Result<(Vec<Value>, Option<Value>), Error> {
        let mut latest_usage = None;
        for index in start..self.steps.len() {
            let mut next = Vec::new();
            for value in values {
                let converted = self.steps[index].convert(ctx, &value)?;
                for v in converted {
                    next.push(Cow::Owned(if self.preserve_message_structure {
                        v
                    } else {
                        crate::wire::normalize_stream_owned(self.steps[index].target(), v)?
                    }));
                }
                let usage = match self.steps[index] {
                    Step::ChatClaude(_) | Step::ChatGemini(_) => {
                        canonical(Format::OpenAi, &value, 0)
                    }
                    Step::GeminiChat(_) => {
                        canonical(Format::Gemini, &value, ctx.estimate_prompt_tokens)
                    }
                    _ => self.steps[index].usage().cloned(),
                };
                if let Some(usage) = usage {
                    merge_nonzero_impl(&mut self.usage, &usage, self.usage_needs_normalization);
                    self.usage_needs_normalization = false;
                    latest_usage = Some(usage);
                }
            }
            values = next;
        }
        Ok((
            values.into_iter().map(Cow::into_owned).collect(),
            latest_usage,
        ))
    }
    pub fn finalize(&mut self, ctx: &Context) -> Result<Vec<Value>, Error> {
        if self.failed {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for index in 0..self.steps.len() {
            if matches!(self.steps[index], Step::ChatClaude(_))
                && self.steps[index].usage().is_none()
            {
                if let Some(u) = &self.usage {
                    self.steps[index].set_usage(u.clone());
                }
            }
            let values = self.steps[index].finalize(ctx)?;
            if let Some(usage) = self.steps[index].usage() {
                merge_nonzero_impl(&mut self.usage, usage, self.usage_needs_normalization);
                self.usage_needs_normalization = false;
            }
            let values = values
                .into_iter()
                .map(|v| {
                    if self.preserve_message_structure {
                        Ok(Cow::Owned(v))
                    } else {
                        crate::wire::normalize_stream_owned(self.steps[index].target(), v)
                            .map(Cow::Owned)
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            let (values, usage) = self.execute(ctx, values, index + 1)?;
            self.remember_usage(usage);
            out.extend(values);
        }
        Ok(out)
    }
    /// Every distinct loss diagnostic observed, including source events that emitted no output.
    pub fn diagnostics(&self) -> &[crate::Diagnostic] {
        &self.diagnostics
    }
    /// Drain newly observed diagnostics without losing the complete stream history.
    pub fn take_diagnostics(&mut self) -> Vec<crate::Diagnostic> {
        std::mem::take(&mut self.pending_diagnostics)
    }
    pub fn usage(&self) -> Option<&Value> {
        self.usage
            .as_ref()
            .or_else(|| self.steps.iter().find_map(Step::usage))
    }
    pub fn set_usage(&mut self, usage: Value) {
        for step in &mut self.steps {
            step.set_usage(usage.clone());
        }
        self.usage = Some(usage);
        self.usage_needs_normalization = true;
    }
    pub fn usage_text(&self) -> &str {
        self.steps
            .iter()
            .map(Step::usage_text)
            .find(|s| !s.is_empty())
            .unwrap_or("")
    }
    /// Emit native terminal failure events for a Responses target. Other targets return `None`.
    pub fn fail_responses_stream(
        &mut self,
        code: &str,
        message: &str,
        param: &str,
    ) -> Option<Vec<Value>> {
        self.steps.iter_mut().find_map(|s| {
            if let Step::ChatResponses(s) = s {
                Some(
                    s.fail(code, message, param)
                        .into_iter()
                        .map(|event| {
                            crate::wire::normalize_stream(Format::OpenAiResponses, &event)
                                .unwrap_or(event)
                        })
                        .collect(),
                )
            } else if let Step::ClaudeResponses(s) = s {
                Some(s.fail(code, message, param))
            } else {
                None
            }
        })
    }
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Translate a host-detected transport/protocol failure without a success terminator.
    pub fn fail_stream(&mut self, code: &str, message: &str, param: &str) -> Vec<Value> {
        if self.failed {
            return Vec::new();
        }
        self.failed = true;
        if let Some(events) = self.fail_responses_stream(code, message, param) {
            return events;
        }
        match self.to {
            Format::Claude => vec![json!({
                "type":"error",
                "error":{"type":"api_error","code":code,"message":message}
            })],
            Format::OpenAi | Format::Gemini => vec![json!({
                "error":{"type":"api_error","code":code,"message":message,"param":param}
            })],
            Format::OpenAiResponses => vec![
                json!({"type":"error","code":code,"message":message,"param":param}),
                json!({
                    "type":"response.failed",
                    "response":{
                        "object":"response","status":"failed","output":[],
                        "error":{"code":code,"message":message}
                    }
                }),
            ],
        }
    }
    fn remember_usage(&mut self, usage: Option<Value>) {
        if let Some(u) = usage {
            merge_nonzero_impl(&mut self.usage, &u, self.usage_needs_normalization);
            self.usage_needs_normalization = false;
        }
    }
}

pub(crate) fn protocol_error(input: &Value) -> Option<(String, String, String)> {
    let nested = input.get("response").unwrap_or(input);
    let failed = matches!(st(&input["type"]), "error" | "response.failed")
        || st(&nested["status"]) == "failed";
    let error = input
        .get("error")
        .filter(|v| !v.is_null())
        .or_else(|| nested.get("error").filter(|v| !v.is_null()));
    if !failed && error.is_none() {
        return None;
    }
    let error = error.unwrap_or(input);
    let code = error.get("code").and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .unwrap_or("upstream_error");
    let message = error.get("message").and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("upstream protocol reported a failure");
    Some((code.into(), message.into(), st(&error["param"]).into()))
}
impl Step {
    fn target(&self) -> Format {
        match self {
            Self::ChatResponses(_) | Self::ClaudeResponses(_) => Format::OpenAiResponses,
            Self::ChatClaude(_) | Self::ResponsesClaude(_) => Format::Claude,
            Self::ChatGemini(_) => Format::Gemini,
            _ => Format::OpenAi,
        }
    }
    fn convert(&mut self, ctx: &Context, v: &Value) -> Result<Vec<Value>, Error> {
        match self {
            Self::ChatResponses(s) => s.convert(ctx, v),
            Self::ResponsesChat(s) => s.convert(v),
            Self::ChatClaude(s) => s.convert(ctx, v),
            Self::ClaudeChat(s) => s.convert(v),
            Self::ClaudeResponses(s) => s.convert(ctx, v),
            Self::ResponsesClaude(s) => s.convert(ctx, v),
            Self::ChatGemini(s) => s.convert(ctx, v),
            Self::GeminiChat(s) => s.convert(ctx, v),
        }
    }
    fn finalize(&mut self, ctx: &Context) -> Result<Vec<Value>, Error> {
        match self {
            Self::ChatResponses(s) => s.finalize(),
            Self::ResponsesChat(s) => s.finalize(None),
            Self::ChatClaude(s) => s.finalize(),
            Self::ClaudeChat(_) => Ok(Vec::new()),
            Self::ClaudeResponses(s) => s.finalize(ctx),
            Self::ResponsesClaude(s) => s.finish(ctx, None),
            Self::ChatGemini(s) => s.finalize(ctx),
            Self::GeminiChat(s) => s.finalize(ctx),
        }
    }
    fn usage(&self) -> Option<&Value> {
        match self {
            Self::ChatResponses(s) => s.usage.as_ref(),
            Self::ResponsesChat(s) => s.usage.as_ref(),
            Self::ChatClaude(s) => s.usage.as_ref(),
            Self::ClaudeChat(s) => s.usage.as_ref(),
            Self::ClaudeResponses(s) => s.usage.as_ref(),
            Self::ResponsesClaude(s) => s.usage.as_ref(),
            Self::ChatGemini(s) => s.usage.as_ref(),
            Self::GeminiChat(s) => s.usage.as_ref(),
        }
    }
    fn set_usage(&mut self, v: Value) {
        // Internal carriers hold canonical snapshots. The public state keeps
        // the caller's Value until its next merge, matching the setter contract.
        let v = if v.get("prompt_tokens").is_some() || v.get("completion_tokens").is_some() {
            crate::response::normalize_usage(&v)
        } else {
            v
        };
        match self {
            Self::ChatResponses(s) => s.usage = Some(usage_from_chat(&v)),
            Self::ResponsesChat(s) => s.usage = Some(v),
            Self::ChatClaude(s) => s.usage = Some(v),
            Self::ClaudeChat(s) => s.usage = Some(v),
            Self::ClaudeResponses(s) => s.usage = Some(v),
            Self::ResponsesClaude(s) => s.usage = Some(v),
            Self::ChatGemini(s) => s.usage = Some(usage_from_chat(&v)),
            Self::GeminiChat(s) => s.usage = Some(v),
        }
    }
    fn usage_text(&self) -> &str {
        match self {
            Self::ChatResponses(s) => &s.usage_text,
            Self::ClaudeResponses(s) => &s.usage_text,
            Self::ResponsesChat(s) => &s.usage_text,
            Self::ResponsesClaude(s) => &s.usage_text,
            _ => "",
        }
    }
}
fn canonical(f: Format, v: &Value, estimate: i64) -> Option<Value> {
    match f {
        Format::OpenAi => v.get("usage").filter(|u| !u.is_null()).map(usage_from_chat),
        Format::OpenAiResponses => v
            .get("response")
            .filter(|r| !r.is_null())
            .map(|r| usage_from_responses(&r["usage"])),
        Format::Claude => v
            .get("usage")
            .filter(|u| !u.is_null())
            .or_else(|| v.pointer("/message/usage").filter(|u| !u.is_null()))
            .map(usage_from_claude),
        Format::Gemini => usage_from_gemini(&v["usageMetadata"], estimate),
    }
}
fn reported_gemini_usage(v: &Value) -> bool {
    // Match the wire normalizer's field lookup: an exact key takes precedence,
    // otherwise encoding/json-compatible ASCII case folding accepts aliases.
    v.as_object()
        .and_then(|fields| {
            fields.get("usageMetadata").or_else(|| {
                fields
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("usageMetadata"))
                    .map(|(_, value)| value)
            })
        })
        .is_some_and(|usage| !usage.is_null())
}
fn st(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn n(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}
fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn uid(prefix: &str) -> String {
    crate::new_id(prefix)
}
fn merge_nonzero(old: &mut Option<Value>, value: &Value) {
    merge_nonzero_impl(old, value, false)
}
fn merge_nonzero_impl(old: &mut Option<Value>, value: &Value, force_normalize: bool) {
    fn assign(dst: &mut Value, src: Value, changed: &mut bool) {
        if *dst != src {
            *dst = src;
            *changed = true;
        }
    }
    fn copy_field(
        dst: &mut serde_json::Map<String, Value>,
        key: &str,
        src: &Value,
        changed: &mut bool,
    ) {
        if dst.get(key) != Some(src) {
            dst.insert(key.into(), src.clone());
            *changed = true;
        }
    }
    fn merge_cached_token_details(a: &mut Value, b: &Value, changed: &mut bool) {
        if !a.is_object() {
            assign(a, json!({}), changed);
        }
        // These optional counters report explicit zeros; absent modalities retain
        // their previous count, unlike the legacy nonzero scalar counters.
        for key in ["text_tokens", "image_tokens", "audio_tokens"] {
            if let Some(value) = b.get(key).filter(|value| !value.is_null()) {
                copy_field(a.as_object_mut().unwrap(), key, value, changed);
            }
        }
    }
    fn merge_gemini_token_details(a: &Value, b: &Value) -> Value {
        let modality = |detail: &Value| st(&detail["modality"]).trim().to_uppercase();
        let reported = arr(b)
            .iter()
            .filter(|detail| n(&detail["tokenCount"]) > 0)
            .map(modality)
            .collect::<BTreeSet<_>>();
        Value::Array(
            arr(a)
                .iter()
                .filter(|detail| !reported.contains(&modality(detail)))
                .chain(arr(b).iter().filter(|detail| n(&detail["tokenCount"]) > 0))
                .cloned()
                .collect::<Vec<_>>(),
        )
    }
    fn merge_gemini_metadata(a: &mut Value, b: &Value, changed: &mut bool) {
        if !a.is_object() {
            assign(a, b.clone(), changed);
            return;
        }
        let details = [
            "promptTokensDetails",
            "toolUsePromptTokensDetails",
            "candidatesTokensDetails",
        ]
        .map(|key| (key, merge_gemini_token_details(&a[key], &b[key])));
        merge(a, b, changed);
        for (key, value) in details {
            if a.get(key) != Some(&value) {
                a[key] = value;
                *changed = true;
            }
        }
        if n(&b["candidatesTokenCount"]) > 0 {
            // A new candidate count makes its accompanying thoughts count
            // authoritative, including a reported or DTO-default zero.
            copy_field(
                a.as_object_mut().unwrap(),
                "thoughtsTokenCount",
                &json!(n(&b["thoughtsTokenCount"])),
                changed,
            );
        }
        let total = [
            "promptTokenCount",
            "toolUsePromptTokenCount",
            "candidatesTokenCount",
            "thoughtsTokenCount",
        ]
        .iter()
        .map(|key| n(&a[*key]))
        .sum::<i64>();
        let total = n(&a["totalTokenCount"]).max(total);
        copy_field(
            a.as_object_mut().unwrap(),
            "totalTokenCount",
            &json!(total),
            changed,
        );
    }
    fn merge(a: &mut Value, b: &Value, changed: &mut bool) {
        let Some(b) = b.as_object() else { return };
        if !a.is_object() {
            assign(a, json!({}), changed);
        }
        let a = a.as_object_mut().unwrap();
        for (k, v) in b {
            if v.is_null() || v.as_f64().is_some_and(|n| n <= 0.) || v.as_str() == Some("") {
                continue;
            }
            if k == "billing_usage" {
                let prev = a.get(k).unwrap_or(&Value::Null);
                let same = ["source", "semantic"].iter().all(|f| {
                    st(&prev[*f]).is_empty()
                        || st(&v[*f]).is_empty()
                        || st(&prev[*f]).eq_ignore_ascii_case(st(&v[*f]))
                }) && ["openai_usage", "claude_usage", "gemini_usage_metadata"]
                    .iter()
                    .any(|f| prev[*f].is_object() && v[*f].is_object());
                if !same {
                    let estimated = prev["estimated"].as_bool() == Some(true)
                        || v["estimated"].as_bool() == Some(true);
                    if estimated {
                        let mut snapshot = v.clone();
                        snapshot["estimated"] = json!(true);
                        copy_field(a, k, &snapshot, changed);
                    } else {
                        copy_field(a, k, v, changed);
                    }
                    continue;
                }
            }
            if k == "estimated" && a.get(k).and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if v.is_object() {
                if k == "cache_creation" {
                    copy_field(a, k, v, changed);
                    continue;
                }
                if !a.contains_key(k) {
                    a.insert(k.clone(), Value::Null);
                    *changed = true;
                }
                let target = a.get_mut(k).unwrap();
                match k.as_str() {
                    "cached_tokens_details" => merge_cached_token_details(target, v, changed),
                    "gemini_usage_metadata" => merge_gemini_metadata(target, v, changed),
                    _ => merge(target, v, changed),
                }
            } else {
                copy_field(a, k, v, changed);
            }
        }
        if a.contains_key("prompt_tokens") || a.contains_key("completion_tokens") {
            let get = |k: &str| a.get(k).and_then(Value::as_i64).unwrap_or(0);
            let total = get("total_tokens")
                .max(get("prompt_tokens") + get("completion_tokens"))
                .max(get("input_tokens") + get("output_tokens"));
            copy_field(a, "total_tokens", &json!(total), changed);
        }
    }
    let mut changed = old.is_none() || force_normalize;
    if let Some(old) = old {
        merge(old, value, &mut changed);
    } else {
        let mut initial = json!({});
        merge(&mut initial, value, &mut changed); // Preserve zero-valued DTO fields on the first snapshot.
        if let (Some(dst), Some(src)) = (initial.as_object_mut(), value.as_object()) {
            for (k, v) in src {
                dst.entry(k).or_insert_with(|| v.clone());
            }
        }
        *old = Some(initial);
    }
    if changed {
        if let Some(value) = old {
            if value.get("prompt_tokens").is_some() || value.get("completion_tokens").is_some() {
                *value = crate::response::normalize_usage(value);
            }
        }
    }
}
fn content(v: &Value) -> Cow<'_, str> {
    if let Some(s) = v.as_str() {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        arr(v)
            .iter()
            .filter(|x| st(&x["type"]) == "text")
            .map(|x| st(&x["text"]))
            .collect(),
    )
}
fn reasoning(v: &Value) -> &str {
    let s = st(&v["reasoning_content"]);
    if s.is_empty() { st(&v["reasoning"]) } else { s }
}
fn chat_chunk(id: &str, model: &str, created: i64, delta: Value, finish: Option<&str>) -> Value {
    object([
        ("id", id.into()),
        ("object", "chat.completion.chunk".into()),
        ("created", created.into()),
        ("model", model.into()),
        (
            "choices",
            Value::Array(vec![object([
                ("index", 0.into()),
                ("delta", delta),
                (
                    "finish_reason",
                    finish.map(Value::from).unwrap_or(Value::Null),
                ),
            ])]),
        ),
    ])
}
// Unlike json!(owned_value), assembling owned children does not serialize and
// copy their strings, arrays, and objects into a second JSON tree.
fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
fn claude_stop(s: &str) -> &str {
    match s.to_ascii_lowercase().as_str() {
        "stop" => "end_turn",
        "stop_sequence" => "stop_sequence",
        "length" | "max_tokens" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "refusal",
        _ => s,
    }
}
fn chat_stop(s: &str) -> &str {
    match s.to_ascii_lowercase().as_str() {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" | "pause_turn" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => s,
    }
}
fn responses_finish(v: Option<&Value>) -> Option<&str> {
    let v = v?;
    if st(&v["status"]).trim() != "incomplete" {
        return None;
    }
    Some(
        if st(&v["incomplete_details"]["reason"]).trim() == "content_filter" {
            "content_filter"
        } else {
            "length"
        },
    )
}
fn separated(s: &str) -> String {
    if s.starts_with("\n\n") {
        s.into()
    } else if s.starts_with('\n') {
        format!("\n{s}")
    } else {
        format!("\n\n{s}")
    }
}
fn item_id(e: &Value) -> &str {
    let id = st(&e["item"]["id"]).trim();
    if id.is_empty() {
        st(&e["item_id"]).trim()
    } else {
        id
    }
}
fn arguments(item: &Value) -> String {
    let a = &item["arguments"];
    if a.is_null() {
        String::new()
    } else {
        a.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::json::stringify(a))
    }
}
fn reasoning_text(item: &Value) -> String {
    crate::response::reasoning_output(item)
}
fn to_response_annotation(a: &Value) -> Result<Value, Error> {
    Ok(crate::response::citations::to_responses(&json!([a]))?
        .into_iter()
        .next()
        .unwrap_or(Value::Null))
}
fn to_chat_annotation(a: &Value) -> Result<Value, Error> {
    crate::response::citations::response_annotation_to_chat(a)
}
