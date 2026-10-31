use super::*;
#[derive(Debug, Default)]
pub struct ClaudeRender {
    pub thinking: Option<Value>,
    pub output_effort: String,
    pub effective_effort: String,
    pub clear_sampling: bool,
    pub constrain_thinking_sampling: bool,
    pub max_tokens: Option<u64>,
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Default)]
struct Cap {
    adaptive: bool,
    manual: bool,
    default_thinking: bool,
    disable: bool,
    between: bool,
    effort: bool,
    xhigh: bool,
    max: bool,
    strict: bool,
    off_effort_at_most_high: bool,
    default_effort: &'static str,
}
fn cap(model: &str) -> Cap {
    let m = model.to_ascii_lowercase();
    let mut c = Cap {
        manual: true,
        disable: true,
        default_effort: "high",
        ..Cap::default()
    };
    if m.starts_with("claude-fable-5") || m.starts_with("claude-mythos-5") {
        c.adaptive = true;
        c.manual = false;
        c.default_thinking = true;
        c.disable = false;
        c.effort = true;
        c.xhigh = true;
        c.max = true;
        c.strict = true
    } else if m.starts_with("claude-mythos-preview") {
        c.adaptive = true;
        c.default_thinking = true;
        c.disable = false;
        c.effort = true;
        c.max = true;
        c.strict = true
    } else if m.starts_with("claude-opus-5-5") || m.starts_with("claude-sonnet-5-5") {
        c.adaptive = true;
        c.manual = false;
        c.default_thinking = true;
        c.disable = false;
        c.between = m.starts_with("claude-sonnet-5-5");
        c.off_effort_at_most_high = c.between;
        if m.starts_with("claude-opus-5-5") {
            c.default_effort = "medium"
        }
        c.effort = true;
        c.xhigh = true;
        c.max = true;
        c.strict = true
    } else if [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
    ]
    .iter()
    .any(|p| m.starts_with(p))
    {
        c.adaptive = true;
        c.manual = false;
        c.default_thinking = m.starts_with("claude-opus-5")
            || m.starts_with("claude-sonnet-5")
            || m.starts_with("claude-haiku-5");
        c.off_effort_at_most_high =
            m.starts_with("claude-opus-5") || m.starts_with("claude-haiku-5");
        if m.starts_with("claude-haiku-5") {
            c.default_effort = "medium";
        }
        c.effort = true;
        c.xhigh = true;
        c.max = true;
        c.strict = true
    } else if m.starts_with("claude-opus-4-6") || m.starts_with("claude-sonnet-4-6") {
        c.adaptive = true;
        c.effort = true;
        c.max = true
    } else if m.starts_with("claude-opus-4-5") {
        c.effort = true
    }
    c
}
pub(super) fn known(model: &str) -> bool {
    [
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-preview",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-haiku-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-opus-4-5",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-opus-4-1",
        "claude-opus-4-",
        "claude-sonnet-4-",
        "claude-3-7-sonnet",
    ]
    .iter()
    .any(|p| model.starts_with(p))
}
pub(super) fn manual(model: &str, i: &Intent) -> bool {
    cap(model).manual && i.budget_tokens.is_some() && i.mode != "adaptive"
}
pub(super) fn resolve_default(model: &str, mut i: Intent) -> Intent {
    let c = cap(model);
    if !i.has_strength() {
        if c.default_thinking {
            i.mode = "adaptive".into();
            i.effort = c.default_effort.into()
        }
    } else if i.effort.is_empty()
        && i.budget_tokens.is_none()
        && (i.mode == "adaptive" || i.mode == "enabled")
    {
        i.effort = c.default_effort.into()
    }
    i
}
fn effort(e: &str, c: &Cap) -> String {
    match e {
        "minimal" => "low",
        "xhigh" if !c.xhigh => {
            if c.max {
                "max"
            } else {
                "high"
            }
        }
        "max" if !c.max => "high",
        _ => e,
    }
    .into()
}
/// The effort a model accepts, or an empty string when it takes no effort.
pub fn claude_effort(model: &str, requested: &str) -> String {
    let c = cap(model);
    if !c.effort || requested.is_empty() || requested == "none" {
        String::new()
    } else if requested == "ultra" {
        requested.into()
    } else {
        effort(requested, &c)
    }
}
fn effort_diagnostics(model: &str, requested: &str, sent: &str) -> Vec<Diagnostic> {
    if requested.is_empty() || requested == "none" || requested == sent {
        vec![]
    } else if sent.is_empty() {
        vec![diagnostic(
            "claude_effort_unsupported",
            "thinking",
            format!(
                "model {model:?} does not support effort {requested:?}; the effort was removed"
            ),
        )]
    } else {
        vec![diagnostic(
            "claude_effort_adjusted",
            "thinking",
            format!(
                "model {model:?} does not support effort {requested:?} with the selected thinking mode; using {sent:?}"
            ),
        )]
    }
}
fn thinking(kind: &str, include: Option<bool>) -> Value {
    let mut t = json!({"type":kind});
    if let Some(b) = include {
        t["display"] = json!(if b { "summarized" } else { "omitted" })
    }
    t
}
pub fn render_claude(
    model: &str,
    intent: Intent,
    max_tokens: Option<u64>,
    adapter_percentage: f64,
) -> Result<ClaudeRender, Error> {
    let disabled_with_effort =
        intent.mode == "disabled" && !intent.effort.is_empty() && intent.effort != "none";
    let (mut i, mut ds) = if disabled_with_effort {
        let mut i = intent;
        i.effort = parse_effort(&i.effort)?;
        (i, vec![])
    } else {
        normalize_intent(intent)?
    };
    let c = cap(model);
    let mut r = ClaudeRender {
        clear_sampling: c.strict,
        ..ClaudeRender::default()
    };
    if !i.has_strength() {
        if c.adaptive && c.default_thinking && i.include_thoughts.is_some() {
            r.thinking = Some(thinking("adaptive", i.include_thoughts))
        }
        if c.default_thinking {
            r.effective_effort = c.default_effort.into()
        }
        r.diagnostics = ds;
        return Ok(r);
    }
    if i.mode == "disabled" || i.effort == "none" {
        if disabled_with_effort && (c.disable || c.between) {
            r.output_effort = claude_effort(model, &i.effort);
            if c.off_effort_at_most_high && matches!(r.output_effort.as_str(), "xhigh" | "max") {
                r.output_effort = "high".into();
            }
            ds.extend(effort_diagnostics(model, &i.effort, &r.output_effort));
        }
        if c.between {
            ds.push(diagnostic("claude_thinking_disable_as_between_tools","thinking",format!("model {model:?} rejects disabled thinking; using between_tools, its lowest thinking setting")));
            r.thinking = Some(json!({"type":"between_tools"}));
            r.effective_effort = "none".into();
        } else if !c.disable {
            ds.push(diagnostic("claude_thinking_disable_unsupported","thinking",format!("model {model:?} cannot disable thinking; using the lowest representable thinking mode")));
            if c.adaptive {
                r.thinking = Some(thinking("adaptive", i.include_thoughts));
                r.effective_effort = c.default_effort.into();
                if c.effort {
                    r.output_effort = "low".into();
                    r.effective_effort = "low".into();
                }
            }
        } else {
            r.thinking = Some(json!({"type":"disabled"}));
            r.effective_effort = "none".into()
        }
        r.diagnostics = ds;
        return Ok(r);
    }
    let prefer_manual = c.manual && i.budget_tokens.is_some() && i.mode != "adaptive";
    if !c.manual && i.budget_tokens.is_some() && i.mode == "enabled" {
        ds.push(diagnostic("claude_budget_to_adaptive","thinking",format!("model {model:?} uses adaptive thinking; budget_tokens was converted to an effort level")))
    }
    if c.adaptive && !prefer_manual {
        let mut e = i.effort.clone();
        if e.is_empty() {
            if let Some(b) = i.budget_tokens {
                e = effort_from_budget(b).into()
            }
        }
        if e.is_empty() && i.mode == "enabled" {
            e = c.default_effort.into()
        }
        let normalized = effort(&e, &c);
        if !e.is_empty() && normalized != e {
            ds.push(diagnostic(
                "claude_effort_adjusted",
                "thinking",
                format!("model {model:?} does not support effort {e:?}; using {normalized:?}"),
            ))
        }
        e = normalized;
        r.effective_effort = if e.is_empty() && i.mode == "adaptive" {
            c.default_effort.into()
        } else {
            e.clone()
        };
        r.output_effort = e;
        if !i.mode.is_empty() {
            r.thinking = Some(thinking("adaptive", i.include_thoughts));
            r.constrain_thinking_sampling = !c.strict
        }
        r.diagnostics = ds;
        return Ok(r);
    }
    if i.mode == "adaptive" {
        ds.push(diagnostic(
            "claude_adaptive_to_manual",
            "thinking",
            format!("model {model:?} does not support adaptive thinking; using manual thinking"),
        ));
        i.mode = "enabled".into();
        if i.effort.is_empty() {
            i.effort = c.default_effort.into()
        }
    }
    if i.mode.is_empty() {
        r.output_effort = if known(model) {
            let sent = claude_effort(model, &i.effort);
            ds.extend(effort_diagnostics(model, &i.effort, &sent));
            sent
        } else {
            i.effort
        };
        r.effective_effort = if r.output_effort.is_empty() {
            "none".into()
        } else {
            r.output_effort.clone()
        };
        r.diagnostics = ds;
        return Ok(r);
    }
    let mut max = max_tokens.unwrap_or(0);
    if max <= 1024 {
        let mut raised = 1280;
        if let Some(b) = i
            .budget_tokens
            .filter(|b| *b >= 0 && *b < (i32::MAX as i64 / 2))
        {
            raised = raised.max(b as u64 + 1)
        }
        let current = max_tokens
            .map(|v| format!("max_tokens {v}"))
            .unwrap_or_else(|| "a missing max_tokens".into());
        ds.push(diagnostic("claude_max_tokens_raised","thinking",format!("model {model:?} requires max_tokens greater than 1024 for manual thinking; raised {current} to {raised}")));
        max = raised;
        r.max_tokens = Some(raised)
    }
    if max > i64::MAX as u64 {
        return Err(Error::message(
            "max_tokens is too large for a thinking budget",
        ));
    }
    let budget = if let Some(b) = i.budget_tokens.filter(|b| *b >= 0) {
        let budget = b.max(1024).min(max as i64 - 1);
        if budget != b {
            ds.push(diagnostic("claude_budget_adjusted","thinking",format!("model {model:?} requires 1024 <= budget_tokens < max_tokens; adjusted {b} to {budget}")))
        }
        budget
    } else {
        if i.budget_tokens.is_some() {
            ds.push(diagnostic("claude_dynamic_budget_converted","thinking",format!("model {model:?} does not support a dynamic budget; derived a manual budget from reasoning effort")))
        }
        let percent = match i.effort.as_str() {
            "minimal" => 5,
            "low" => 20,
            "medium" => 50,
            "high" => 80,
            "xhigh" | "max" | "ultra" => 95,
            _ => {
                let p = (adapter_percentage * 100.).round() as i64;
                if p <= 0 { 80 } else { p.min(99) }
            }
        };
        ((max as i128 * percent as i128 / 100) as i64)
            .max(1024)
            .min(max as i64 - 1)
    };
    r.effective_effort = if i.budget_tokens.is_some() && !c.effort || i.effort.is_empty() {
        effort_from_budget(budget).into()
    } else {
        i.effort.clone()
    };
    r.output_effort = claude_effort(model, &i.effort);
    if !r.output_effort.is_empty() {
        r.effective_effort = r.output_effort.clone()
    }
    let mut t = thinking("enabled", i.include_thoughts);
    t["budget_tokens"] = json!(budget);
    r.thinking = Some(t);
    r.clear_sampling = false;
    r.constrain_thinking_sampling = true;
    r.diagnostics = ds;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_off_keeps_model_supported_effort() {
        for (model, thinking_type, sent) in [
            ("claude-haiku-5-5", "disabled", "high"),
            ("claude-opus-5", "disabled", "high"),
            ("claude-sonnet-5-5", "between_tools", "high"),
            ("claude-sonnet-5", "disabled", "max"),
            ("claude-opus-5-5", "adaptive", "low"),
            ("claude-haiku-4-5", "disabled", ""),
        ] {
            let rendered = render_claude(
                model,
                Intent {
                    mode: "disabled".into(),
                    effort: "max".into(),
                    ..Intent::default()
                },
                Some(10000),
                0.,
            )
            .unwrap();
            assert_eq!(rendered.thinking.unwrap()["type"], thinking_type, "{model}");
            assert_eq!(rendered.output_effort, sent, "{model}");
        }
    }

    #[test]
    fn effort_without_thinking_is_normalized_for_known_models() {
        let rendered = render_claude(
            "claude-opus-4-5",
            Intent {
                effort: "minimal".into(),
                ..Intent::default()
            },
            Some(10000),
            0.,
        )
        .unwrap();
        assert!(rendered.thinking.is_none());
        assert_eq!(rendered.output_effort, "low");
        assert_eq!(rendered.diagnostics[0].code, "claude_effort_adjusted");

        let rendered = render_claude(
            "claude-haiku-4-5",
            Intent {
                effort: "high".into(),
                ..Intent::default()
            },
            Some(10000),
            0.,
        )
        .unwrap();
        assert!(rendered.thinking.is_none());
        assert!(rendered.output_effort.is_empty());
        assert_eq!(rendered.effective_effort, "none");
        assert_eq!(rendered.diagnostics[0].code, "claude_effort_unsupported");
    }

    #[test]
    fn manual_mythos_preview_only_constrains_sampling() {
        let rendered = render_claude(
            "claude-mythos-preview",
            Intent {
                mode: "enabled".into(),
                budget_tokens: Some(8192),
                ..Intent::default()
            },
            Some(10000),
            0.,
        )
        .unwrap();
        assert_eq!(rendered.thinking.unwrap()["budget_tokens"], 8192);
        assert!(!rendered.clear_sampling);
        assert!(rendered.constrain_thinking_sampling);
    }

    #[test]
    fn public_effort_helper_uses_model_capabilities() {
        assert_eq!(claude_effort("CLAUDE-OPUS-5-5", "max"), "max");
        assert_eq!(claude_effort("claude-mythos-preview", "xhigh"), "max");
        assert_eq!(claude_effort("claude-opus-4-5", "minimal"), "low");
        assert_eq!(claude_effort("claude-opus-4-5", "max"), "high");
        assert_eq!(claude_effort("claude-haiku-4-5", "high"), "");
        assert_eq!(claude_effort("unknown-model", "high"), "");
        assert_eq!(claude_effort("claude-opus-5-5", "none"), "");
        // Like Go's helper, this accepts an already parsed effort; validation
        // belongs to parse_effort and the public renderers.
        assert_eq!(claude_effort("claude-opus-5-5", "ultra"), "ultra");
    }
}
