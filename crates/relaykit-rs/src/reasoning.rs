//! Portable reasoning intent, explicit model suffix parsing, and provider rendering.
use crate::{Context, Diagnostic, Error, Format, Severity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Intent {
    pub mode: String,
    pub effort: String,
    pub budget_tokens: Option<i64>,
    pub include_thoughts: Option<bool>,
    pub source: String,
    pub budget_source: String,
}
impl Intent {
    pub fn has_strength(&self) -> bool {
        !self.mode.is_empty() || !self.effort.is_empty() || self.budget_tokens.is_some()
    }
    pub fn is_empty(&self) -> bool {
        !self.has_strength() && self.include_thoughts.is_none()
    }
}
fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
fn diagnostic(code: &str, path: &str, message: String) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        path: path.into(),
        message,
        severity: Severity::Warning,
        from: Format::OpenAi,
        to: Format::OpenAi,
    }
}
pub fn parse_effort(value: &str) -> Result<String, Error> {
    let effort = value.trim().to_ascii_lowercase();
    if [
        "", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ]
    .contains(&effort.as_str())
    {
        Ok(effort)
    } else {
        Err(Error::message(format!(
            "unsupported reasoning effort: {value:?}"
        )))
    }
}
pub fn effort_from_budget(budget: i64) -> &'static str {
    match budget {
        0 => "none",
        i64::MIN..=-1 => "high",
        1..=1024 => "low",
        1025..=8192 => "medium",
        _ => "high",
    }
}
pub fn effective_effort(intent: &Intent) -> String {
    let Ok((i, _)) = normalize_intent(intent.clone()) else {
        return String::new();
    };
    if i.mode == "disabled" {
        "none".into()
    } else if !i.effort.is_empty() {
        i.effort
    } else if let Some(b) = i.budget_tokens {
        effort_from_budget(b).into()
    } else if i.mode == "enabled" || i.mode == "adaptive" {
        "high".into()
    } else {
        String::new()
    }
}
pub fn normalize_intent(mut i: Intent) -> Result<(Intent, Vec<Diagnostic>), Error> {
    i.effort = parse_effort(&i.effort)?;
    if !["", "enabled", "disabled", "adaptive"].contains(&i.mode.as_str()) {
        return Err(Error::message(format!(
            "unsupported reasoning mode {:?}",
            i.mode
        )));
    }
    let mut ds = vec![];
    if let Some(mut b) = i.budget_tokens {
        if b < -1 {
            ds.push(diagnostic(
                "reasoning_budget_adjusted",
                "reasoning",
                format!("thinking budget {b} is below -1; using the dynamic budget -1"),
            ));
            b = -1;
            i.budget_tokens = Some(b)
        }
        if b == 0 {
            if i.mode == "enabled"
                || i.mode == "adaptive"
                || !i.effort.is_empty() && i.effort != "none"
            {
                ds.push(diagnostic("explicit_fields_conflict","reasoning","a zero thinking budget disables thinking; the enabling mode or effort was ignored".into()))
            }
            i.mode = "disabled".into();
            i.effort = "none".into();
        } else if i.mode == "disabled" || i.effort == "none" {
            ds.push(diagnostic("explicit_fields_conflict","reasoning",format!("a non-zero thinking budget {b} enables thinking; the disabling mode or effort was ignored")));
            i.mode = "enabled".into();
            i.effort.clear();
        } else if i.mode.is_empty() {
            i.mode = "enabled".into();
        }
    }
    if i.effort == "none" {
        if i.mode == "enabled" || i.mode == "adaptive" {
            ds.push(diagnostic(
                "explicit_fields_conflict",
                "reasoning",
                "effort none disables thinking; the enabling mode was ignored".into(),
            ))
        }
        i.mode = "disabled".into();
    }
    Ok((i, ds))
}
pub fn merge_explicit(
    primary: Intent,
    secondary: Intent,
    model: &str,
) -> Result<(Intent, Vec<Diagnostic>), Error> {
    let (mut p, mut ds) = normalize_intent(primary)?;
    let (q, d) = normalize_intent(secondary)?;
    ds.extend(d);
    if p.is_empty() {
        return Ok((q, ds));
    }
    if q.is_empty() {
        return Ok((p, ds));
    }
    if p.has_strength()
        && q.has_strength()
        && (p.mode == "disabled" || p.effort == "none")
            != (q.mode == "disabled" || q.effort == "none")
    {
        ds.push(diagnostic("explicit_fields_conflict","reasoning",format!("model {model:?}: reasoning fields disagree about whether thinking is enabled; keeping the more specific field")));
        if p.include_thoughts.is_none() {
            p.include_thoughts = q.include_thoughts
        }
        return Ok((p, ds));
    }
    if !p.effort.is_empty() && !q.effort.is_empty() && p.effort != q.effort {
        ds.push(diagnostic(
            "explicit_fields_conflict",
            "reasoning",
            format!(
                "model {model:?}: reasoning efforts {:?} and {:?} differ; keeping {:?}",
                p.effort, q.effort, p.effort
            ),
        ))
    }
    if let (Some(a), Some(b)) = (p.budget_tokens, q.budget_tokens) {
        if a != b {
            ds.push(diagnostic(
                "explicit_fields_conflict",
                "reasoning",
                format!("model {model:?}: thinking budgets {a} and {b} differ; keeping {a}"),
            ))
        }
    }
    let mut out = q;
    if !p.mode.is_empty() {
        out.mode = p.mode
    }
    if !p.effort.is_empty() {
        out.effort = p.effort
    }
    if p.budget_tokens.is_some() {
        out.budget_tokens = p.budget_tokens;
        out.budget_source = p.budget_source
    }
    if p.include_thoughts.is_some() {
        out.include_thoughts = p.include_thoughts
    }
    let (out, d) = normalize_intent(out)?;
    ds.extend(d);
    Ok((out, ds))
}
pub fn merge_explicit_and_suffix(
    explicit: Intent,
    suffix: Intent,
    model: &str,
) -> Result<(Intent, Vec<Diagnostic>), Error> {
    let (e, mut ds) = normalize_intent(explicit)?;
    let (mut x, d) = normalize_intent(suffix)?;
    ds.extend(d);
    if !e.has_strength() {
        if e.include_thoughts.is_some() {
            x.include_thoughts = e.include_thoughts
        }
        return Ok((x, ds));
    }
    if !x.has_strength() {
        let mut out = e;
        if out.include_thoughts.is_none() {
            out.include_thoughts = x.include_thoughts
        }
        return Ok((out, ds));
    }
    if e.include_thoughts.is_some() {
        x.include_thoughts = e.include_thoughts
    }
    let ed = e.mode == "disabled" || e.effort == "none";
    let xd = x.mode == "disabled" || x.effort == "none";
    if ed != xd {
        ds.push(diagnostic("suffix_overrode_request","reasoning",format!("model {model:?}: the model name and request fields disagree about whether thinking is enabled; the model name wins")));
        return Ok((x, ds));
    }
    if xd {
        return Ok((x, ds));
    }
    if x.mode == "enabled" && e.mode == "adaptive" {
        x.mode = "adaptive".into()
    }
    if x.budget_tokens.is_none() && x.effort.is_empty() {
        x.budget_tokens = e.budget_tokens;
        x.budget_source = e.budget_source;
        x.effort = e.effort;
        return Ok((x, ds));
    }
    if !e.effort.is_empty() && e.effort != x.effort {
        ds.push(diagnostic("suffix_overrode_request","reasoning",format!("model {model:?}: request effort {:?} was replaced by the reasoning strength in the model name",e.effort)))
    }
    if let Some(b) = e.budget_tokens {
        if x.budget_tokens != Some(b) {
            ds.push(diagnostic("suffix_overrode_request","reasoning",format!("model {model:?}: request thinking budget {b} was replaced by the reasoning strength in the model name")))
        }
    }
    Ok((x, ds))
}
pub fn from_request(format: Format, v: &Value) -> Result<(Intent, Vec<Diagnostic>), Error> {
    let mut i = Intent::default();
    let mut ds = vec![];
    match format {
        Format::OpenAi => {
            i.source = "explicit".into();
            i.effort = parse_effort(s(&v["reasoning_effort"]))?;
            if !i.effort.is_empty() {
                i.mode = if i.effort == "none" {
                    "disabled"
                } else {
                    "enabled"
                }
                .into()
            }
            if !v["reasoning"].is_null() {
                if !v["reasoning"].is_object() {
                    return Err(Error::message("invalid reasoning config: expected object"));
                }
                let r = &v["reasoning"];
                for (field, valid) in [
                    (
                        "enabled",
                        r["enabled"].is_null() || r["enabled"].is_boolean(),
                    ),
                    ("effort", r["effort"].is_null() || r["effort"].is_string()),
                    (
                        "max_tokens",
                        r["max_tokens"].is_null() || r["max_tokens"].as_i64().is_some(),
                    ),
                    (
                        "exclude",
                        r["exclude"].is_null() || r["exclude"].is_boolean(),
                    ),
                ] {
                    if !valid {
                        return Err(Error::message(format!(
                            "invalid reasoning config: invalid {field} type"
                        )));
                    }
                }
                let mut nested = Intent {
                    budget_tokens: r["max_tokens"].as_i64(),
                    source: "explicit".into(),
                    budget_source: "explicit".into(),
                    include_thoughts: r["exclude"].as_bool().map(|v| !v),
                    ..Intent::default()
                };
                if let Some(on) = r["enabled"].as_bool() {
                    nested.mode = if on { "enabled" } else { "disabled" }.into();
                    if !on {
                        nested.effort = "none".into()
                    }
                }
                if !s(&r["effort"]).is_empty() {
                    nested.effort = parse_effort(s(&r["effort"]))?;
                    if nested.effort == "none" {
                        nested.mode = "disabled".into()
                    } else if nested.mode.is_empty() {
                        nested.mode = "enabled".into()
                    }
                }
                let (out, d) = merge_explicit(i, nested, s(&v["model"]))?;
                i = out;
                ds.extend(d);
            }
        }
        Format::OpenAiResponses => {
            i.source = "explicit".into();
            i.effort = parse_effort(s(&v["reasoning"]["effort"]))?;
            if !i.effort.is_empty() {
                i.mode = if i.effort == "none" {
                    "disabled"
                } else {
                    "enabled"
                }
                .into()
            }
            if !s(&v["reasoning"]["summary"]).is_empty() {
                i.include_thoughts = Some(true)
            }
        }
        Format::Claude => {
            i.source = "native".into();
            if !v["output_config"].is_null()
                && (!v["output_config"].is_object()
                    || (!v["output_config"]["effort"].is_null()
                        && !v["output_config"]["effort"].is_string()))
            {
                return Err(Error::message(
                    "invalid Claude output_config: expected object with string effort",
                ));
            }
            if !v["thinking"].is_null() {
                let t = &v["thinking"];
                i.mode=match s(&t["type"]){""|"enabled"=>"enabled","adaptive"=>"adaptive","disabled"=>{i.effort="none".into();"disabled"},unknown=>{ds.push(diagnostic("claude_thinking_type_coerced","thinking",format!("unsupported Claude thinking type {unknown:?} was treated as enabled")));"enabled"}}.into();
                i.budget_tokens = t["budget_tokens"].as_i64();
                i.budget_source = "native".into();
                i.include_thoughts = match s(&t["display"]) {
                    "summarized" => Some(true),
                    "omitted" => Some(false),
                    _ => None,
                };
            }
            if !s(&v["output_config"]["effort"]).is_empty() {
                i.effort = parse_effort(s(&v["output_config"]["effort"]))?
            }
            if i.mode == "disabled" && !i.effort.is_empty() && i.effort != "none" {
                return Ok((i, ds));
            }
        }
        Format::Gemini => {
            let t = &v["generationConfig"]["thinkingConfig"];
            i.source = "native".into();
            i.budget_source = "native".into();
            i.budget_tokens = t["thinkingBudget"].as_i64();
            i.include_thoughts = t["includeThoughts"].as_bool();
            i.effort = parse_effort(s(&t["thinkingLevel"]))?;
            if i.budget_tokens.is_some() && !i.effort.is_empty() {
                return Err(Error::message(
                    "reasoning settings conflict: Gemini thinkingBudget and thinkingLevel cannot both be set",
                ));
            }
            if !i.effort.is_empty() {
                i.mode = if i.effort == "none" {
                    "disabled"
                } else {
                    "enabled"
                }
                .into()
            }
        }
    }
    let (i, d) = normalize_intent(i)?;
    ds.extend(d);
    Ok((i, ds))
}
mod claude;
mod gemini;
mod suffix;
pub use claude::{ClaudeRender, claude_effort, render_claude};
pub use gemini::{GeminiRender, render_gemini};
pub use suffix::*;

pub(crate) fn convert_request(
    from: Format,
    to: Format,
    input: &Value,
    out: &mut Value,
    ctx: &Context,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), Error> {
    if from == to {
        return Ok(());
    }
    let mut normalized_input = Cow::Borrowed(input);
    let source_model = if !ctx.origin_model_name.is_empty() {
        ctx.origin_model_name.as_str()
    } else if from == Format::Gemini {
        ctx.upstream_model()
    } else {
        s(&input["model"])
    };
    let preserve_source = ctx
        .options
        .preserve_thinking_suffix
        .as_ref()
        .is_some_and(|f| f(source_model));
    let mut source_base = source_model.to_owned();
    if from == Format::Gemini && !preserve_source {
        source_base = parse_model_modifiers(source_model).base;
        if let Ok((base, _, true)) =
            parse_gemini_model_suffix(&source_base, ctx.options.gemini.thinking_adapter_enabled)
        {
            source_base = base;
        }
    }
    if from == Format::Gemini && !source_base.is_empty() {
        let (_, ds) = gemini::normalize_config(
            &source_base,
            &mut normalized_input.to_mut()["generationConfig"],
        )?;
        diagnostics.extend(ds);
    }
    let (mut intent, d) = from_request(from, &normalized_input)?;
    diagnostics.extend(d);
    if ctx.options.preserve_message_structure
        && from == Format::OpenAiResponses
        && to == Format::Claude
    {
        match intent.effort.as_str() {
            "xhigh" | "ultra" | "max" => {
                intent.effort = "max".into();
                intent.budget_tokens = Some(32_768);
                intent.mode = "enabled".into();
            }
            "high" => {
                intent.budget_tokens = Some(10_240);
                intent.mode = "enabled".into();
            }
            "medium" => {
                intent.budget_tokens = Some(4_096);
                intent.mode = "enabled".into();
            }
            "minimal" | "none" => {
                intent.effort = "low".into();
                intent.mode = "disabled".into();
                intent.budget_tokens = None;
            }
            _ => {}
        }
    }
    if ctx.options.preserve_message_structure
        && from == Format::OpenAi
        && to == Format::Claude
        && intent.budget_tokens.is_none()
    {
        intent.budget_tokens = match intent.effort.as_str() {
            "low" => Some(1_024),
            "medium" => Some(4_096),
            "high" => Some(10_240),
            "xhigh" | "max" | "ultra" => Some(32_768),
            _ => None,
        };
        if intent.budget_tokens.is_some() {
            intent.budget_source = "effort".into();
        }
    }
    if matches!(from, Format::Claude | Format::Gemini) {
        if from == Format::Claude || !preserve_source {
            if let Some(suffix) = &ctx.reasoning_conversion {
                let (i, d) = merge_explicit_and_suffix(intent, suffix.clone(), source_model)?;
                intent = i;
                diagnostics.extend(d);
            }
        }
        intent = if from == Format::Claude {
            claude::resolve_default(source_model, intent)
        } else {
            gemini::resolve_default(&source_base, intent)
        };
    }
    if let Some(pivot) = input.get("__relaykit_reasoning") {
        let p: Intent = serde_json::from_value(pivot.clone())?;
        if intent.effort == effective_effort(&p) {
            intent.effort.clear();
            intent.mode.clear()
        }
        let (i, d) = merge_explicit(intent, p, s(&input["model"]))?;
        intent = i;
        diagnostics.extend(d);
    }
    let model = if to == Format::Gemini {
        if !ctx.upstream_model().is_empty() {
            ctx.upstream_model()
        } else {
            s(&input["model"])
        }
    } else {
        s(&out["model"])
    }
    .to_owned();
    if to == Format::Claude {
        if let Some(suffix) = &ctx.reasoning_conversion {
            let preserve = ctx
                .options
                .preserve_thinking_suffix
                .as_ref()
                .is_some_and(|f| f(&model) || f(&ctx.origin_model_name));
            if !preserve {
                let (i, d) = merge_explicit_and_suffix(intent, suffix.clone(), &model)?;
                intent = i;
                diagnostics.extend(d)
            }
        }
    }
    match to {
        Format::OpenAi => {
            if from == Format::Claude && ctx.options.open_router_dialect {
                if !s(&input["output_config"]["effort"]).is_empty() {
                    out["verbosity"] = input["output_config"]["effort"].clone();
                }
                if !intent.is_empty() {
                    let disabled = intent.mode == "disabled" || intent.effort == "none";
                    let mut config = json!({"enabled":!disabled});
                    if !disabled {
                        if let Some(budget) =
                            intent.budget_tokens.filter(|_| intent.mode != "adaptive")
                        {
                            if budget != 0 {
                                config["max_tokens"] = json!(budget);
                            }
                        } else {
                            let effort = effective_effort(&intent);
                            if !effort.is_empty() {
                                config["effort"] = json!(effort);
                            }
                        }
                    }
                    if intent.include_thoughts == Some(false) {
                        config["exclude"] = json!(true);
                    }
                    out["reasoning"] = config;
                }
            } else {
                let effort = effective_effort(&intent);
                if !effort.is_empty() {
                    out["reasoning_effort"] = json!(effort)
                }
                if !intent.is_empty() {
                    out["__relaykit_reasoning"] = serde_json::to_value(&intent)?;
                }
            }
        }
        Format::OpenAiResponses => {
            let effort = effective_effort(&intent);
            if !effort.is_empty() {
                let mut r = json!({"effort":effort});
                if effort != "none" && intent.include_thoughts != Some(false) {
                    r["summary"] = json!("detailed")
                }
                out["reasoning"] = r;
            }
            if !intent.is_empty() {
                out["__relaykit_reasoning"] = serde_json::to_value(&intent)?;
            }
        }
        Format::Claude => {
            let capability = if !claude::known(&model) && claude::known(&ctx.origin_model_name) {
                ctx.origin_model_name.as_str()
            } else {
                model.as_str()
            };
            if !claude::known(capability) && intent.mode == "adaptive" {
                intent.mode = "enabled".into();
                if intent.effort.is_empty() {
                    intent.effort = "high".into()
                }
            }
            let request_effort = parse_effort(s(&out["output_config"]["effort"]))?;
            if claude::known(capability)
                && intent.mode == "disabled"
                && (intent.effort.is_empty() || intent.effort == "none")
                && !request_effort.is_empty()
                && request_effort != "none"
            {
                intent.effort = request_effort.clone();
            }
            if out["max_tokens"].is_null() && intent.has_strength() {
                let mut minimum = ctx
                    .options
                    .claude
                    .default_max_tokens
                    .as_ref()
                    .map(|f| f(capability))
                    .filter(|v| *v > 0)
                    .unwrap_or(1280);
                if claude::manual(capability, &intent) {
                    if let Some(b) = intent.budget_tokens.filter(|b| *b >= 0) {
                        if b >= i32::MAX as i64 / 2 {
                            return Err(Error::message(
                                "thinking budget exceeds the supported conversion limit",
                            ));
                        }
                        minimum = minimum.max(
                            b + if ctx.options.preserve_message_structure
                                && from == Format::OpenAi
                            {
                                1024
                            } else {
                                1
                            },
                        )
                    }
                }
                out["max_tokens"] = json!(minimum)
            }
            if ctx.options.preserve_message_structure
                && from == Format::OpenAi
                && intent.mode != "disabled"
                && let Some(budget) = intent.budget_tokens.filter(|budget| *budget >= 0)
            {
                let minimum = budget.saturating_add(1024) as u64;
                if out["max_tokens"].as_u64().unwrap_or(0) < minimum {
                    out["max_tokens"] = json!(minimum);
                }
            }
            let rendered = render_claude(
                capability,
                intent,
                out["max_tokens"].as_u64(),
                ctx.options.claude.thinking_adapter_budget_tokens_percentage,
            )?;
            diagnostics.extend(rendered.diagnostics);
            if let Some(t) = rendered.thinking {
                out["thinking"] = t
            }
            if let Some(max) = rendered.max_tokens {
                out["max_tokens"] = json!(max)
            }
            if ctx.options.preserve_message_structure
                && from == Format::OpenAi
                && let Some(budget) = out["thinking"]["budget_tokens"].as_i64()
            {
                let minimum = budget.saturating_add(1024);
                if out["max_tokens"].as_i64().unwrap_or(0) < minimum {
                    out["max_tokens"] = json!(minimum);
                }
            }
            let mut output_effort = rendered.output_effort;
            let rewrite_request_effort =
                output_effort.is_empty() && claude::known(capability) && !request_effort.is_empty();
            if rewrite_request_effort {
                output_effort = claude_effort(capability, &request_effort);
            }
            if !output_effort.is_empty() || rewrite_request_effort {
                if out["output_config"].is_null() {
                    out["output_config"] = json!({})
                }
                if !out["output_config"].is_object() {
                    return Err(Error::message("Claude output_config must be a JSON object"));
                }
                if output_effort.is_empty() {
                    out["output_config"]
                        .as_object_mut()
                        .unwrap()
                        .remove("effort");
                } else {
                    out["output_config"]["effort"] = json!(output_effort);
                }
                if out["output_config"].as_object().unwrap().is_empty() {
                    out.as_object_mut().unwrap().remove("output_config");
                }
            }
            if ctx.options.preserve_message_structure
                && from == Format::OpenAiResponses
                && matches!(s(&input["reasoning"]["effort"]), "minimal" | "none")
            {
                out.as_object_mut().unwrap().remove("thinking");
            }
            if !ctx.options.preserve_message_structure
                && from == Format::OpenAiResponses
                && matches!(s(&input["reasoning"]["effort"]), "minimal" | "none")
            {
                out["thinking"] = json!({"type":"disabled"});
                out.as_object_mut().unwrap().remove("output_config");
            }
            let clear = rendered.clear_sampling;
            let constrain = rendered.constrain_thinking_sampling;
            let removed = (!out["temperature"].is_null() || !out["top_k"].is_null())
                && (clear || constrain)
                || !out["top_p"].is_null()
                    && (clear
                        || constrain
                            && out["top_p"]
                                .as_f64()
                                .is_some_and(|v| !(0.95..=1.).contains(&v)));
            if clear || constrain {
                let top = out["top_p"].as_f64();
                let m = out.as_object_mut().unwrap();
                m.remove("temperature");
                m.remove("top_k");
                if clear || top.is_some_and(|v| !(0.95..=1.).contains(&v)) {
                    m.remove("top_p");
                }
            }
            if removed {
                diagnostics.push(diagnostic(if clear{"claude_sampling_removed"}else{"claude_sampling_constrained"},"temperature/top_p/top_k",if clear{format!("model {capability:?} does not accept sampling controls with the selected thinking mode")}else{format!("model {capability:?} accepts only top_p between 0.95 and 1 with manual thinking")}))
            }
            if !rendered.effective_effort.is_empty() {
                ctx.set_reasoning_effort(rendered.effective_effort)
            }
        }
        Format::Gemini => {
            let (native_effort, d) =
                gemini::normalize_config(&model, &mut out["generationConfig"])?;
            diagnostics.extend(d);
            let (mut native, d) = from_request(Format::Gemini, out)?;
            diagnostics.extend(d);
            let mut suffix = ctx.reasoning_conversion.clone().unwrap_or_default();
            if ctx
                .options
                .preserve_thinking_suffix
                .as_ref()
                .is_some_and(|f| f(&model) || f(&ctx.origin_model_name))
            {
                suffix = Intent::default();
            }
            if native.has_strength() && intent.has_strength() {
                if (!intent.effort.is_empty() || intent.budget_tokens.is_some())
                    && !gemini::equivalent(&model, &native, &intent)?
                {
                    diagnostics.push(diagnostic("native_overrode_standard","generationConfig.thinkingConfig",format!("model {model:?}: Gemini thinking_config effort {:?} overrides the standard reasoning effort {:?}",effective_effort(&native),effective_effort(&intent))));
                }
                if native.include_thoughts.is_none() {
                    native.include_thoughts = intent.include_thoughts;
                }
                intent = Intent::default();
            }
            let native_strength = native.has_strength();
            let (explicit, d) = merge_explicit(native, intent, &model)?;
            diagnostics.extend(d);
            if explicit.has_strength()
                && suffix.has_strength()
                && gemini::equivalent(&model, &explicit, &suffix)?
            {
                suffix = Intent::default();
            }
            let suffix_strength = suffix.has_strength();
            let (requested, d) = merge_explicit_and_suffix(explicit.clone(), suffix, &model)?;
            diagnostics.extend(d);
            let requested = gemini::resolve_enabled_default(
                &model,
                requested,
                out["generationConfig"]["maxOutputTokens"].as_u64(),
            );
            if native_strength && !suffix_strength {
                if let Some(include) = explicit.include_thoughts {
                    out["generationConfig"]["thinkingConfig"]["includeThoughts"] = json!(include);
                }
                if !native_effort.is_empty() {
                    ctx.set_reasoning_effort(native_effort);
                }
                return Ok(());
            }
            if requested.is_empty() {
                return Ok(());
            }
            let rendered = render_gemini(
                &model,
                requested,
                out["generationConfig"]["maxOutputTokens"].as_u64(),
                ctx.options.gemini.thinking_adapter_budget_tokens_percentage,
            )?;
            diagnostics.extend(rendered.diagnostics);
            if let Some(t) = rendered.config {
                if out["generationConfig"].is_null() {
                    out["generationConfig"] = json!({})
                }
                out["generationConfig"]["thinkingConfig"] = t;
            } else if let Some(generation) = out["generationConfig"].as_object_mut() {
                generation.remove("thinkingConfig");
            }
            if !rendered.effective_effort.is_empty() {
                ctx.set_reasoning_effort(rendered.effective_effort)
            }
        }
    }
    Ok(())
}
