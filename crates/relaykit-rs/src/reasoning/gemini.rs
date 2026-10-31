use super::*;
#[derive(Debug, Default)]
pub struct GeminiRender {
    pub config: Option<Value>,
    pub effective_effort: String,
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Unknown,
    Unavailable,
    Budget,
    Level,
}
struct Cap {
    kind: Kind,
    disable: bool,
    include: bool,
    min: i64,
    max: i64,
}
fn cap(model: &str) -> Cap {
    let m = model.to_ascii_lowercase();
    let mut c = Cap {
        kind: Kind::Unknown,
        disable: false,
        include: false,
        min: 0,
        max: 0,
    };
    if m.starts_with("gemini-2.5-flash-native-audio")
        || m.starts_with("gemini-live-2.5-flash-preview-native-audio")
    {
        c.kind = Kind::Budget;
        c.disable = true;
        c.max = 24576
    } else if m.starts_with("gemini-2.5-flash-image")
        || m.contains("-tts")
        || m.contains("-native-audio")
        || m.contains("-live")
    {
        c.kind = Kind::Unavailable
    } else if m.starts_with("gemini-3-pro-image") || m.starts_with("nano-banana-pro") {
        c.kind = Kind::Unavailable;
        c.include = true
    } else if [
        "gemini-flash-latest",
        "gemini-flash-lite-latest",
        "gemini-pro-latest",
    ]
    .contains(&m.as_str())
    {
        c.kind = Kind::Level
    } else if m.starts_with("gemini-2.5-pro") {
        c.kind = Kind::Budget;
        c.min = 128;
        c.max = 32768
    } else if m.starts_with("gemini-2.5-flash-lite") {
        c.kind = Kind::Budget;
        c.disable = true;
        c.min = 512;
        c.max = 24576
    } else if m.starts_with("gemini-2.5-") {
        c.kind = Kind::Budget;
        c.disable = true;
        c.max = 24576
    } else if m.starts_with("gemini-3") {
        c.kind = Kind::Level
    }
    c
}
fn default_effort(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m == "gemini-flash-latest"
        || m.starts_with("gemini-3.5-flash") && !m.starts_with("gemini-3.5-flash-lite")
        || m.starts_with("gemini-3.6-flash")
    {
        "medium"
    } else if m == "gemini-flash-lite-latest"
        || m.starts_with("gemini-3.5-flash-lite")
        || m.starts_with("gemini-3.1-flash-lite")
    {
        "minimal"
    } else if m == "gemini-pro-latest"
        || ["gemini-3.1-pro", "gemini-3-pro", "gemini-3-flash"]
            .iter()
            .any(|p| m.starts_with(p))
    {
        "high"
    } else {
        ""
    }
}
fn level(model: &str, e: &str) -> Result<String, Error> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("gemini-3.1-flash-image") || m.starts_with("gemini-3.1-flash-lite-image") {
        return Ok(if e == "minimal" || e == "low" {
            "minimal"
        } else {
            "high"
        }
        .into());
    }
    if m.starts_with("gemini-3-pro") && !m.starts_with("gemini-3.1-pro") {
        return Ok(if e == "minimal" || e == "low" {
            "low"
        } else {
            "high"
        }
        .into());
    }
    if (m.starts_with("gemini-3.1-pro") || m == "gemini-pro-latest") && e == "minimal" {
        return Ok("low".into());
    }
    match e {
        "minimal" | "low" | "medium" | "high" => Ok(e.into()),
        "xhigh" | "max" => Ok("high".into()),
        "none" => Err(Error::message(format!(
            "thinking cannot be disabled for model {model:?}"
        ))),
        _ => Err(Error::message(format!(
            "unsupported reasoning effort {e:?} for model {model:?}"
        ))),
    }
}
fn budget_for_effort(e: &str, maximum: i64) -> i64 {
    match e {
        "minimal" | "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        "xhigh" | "max" => maximum,
        _ => 0,
    }
}

pub(super) fn normalize_config(
    model: &str,
    generation: &mut Value,
) -> Result<(String, Vec<Diagnostic>), Error> {
    if generation["thinkingConfig"].is_null() {
        return Ok((String::new(), vec![]));
    }
    let capabilities = cap(model);
    let config = &mut generation["thinkingConfig"];
    let mut ds = vec![];
    let path = "generationConfig.thinkingConfig";
    if config["thinkingBudget"].is_number() && !s(&config["thinkingLevel"]).is_empty() {
        match capabilities.kind {
            Kind::Budget => {
                ds.push(diagnostic("gemini_budget_level_conflict",path,format!("model {model:?} uses thinkingBudget; thinkingLevel {:?} was dropped because both were set",s(&config["thinkingLevel"]))));
                config.as_object_mut().unwrap().remove("thinkingLevel");
            }
            Kind::Level => {
                ds.push(diagnostic("gemini_budget_level_conflict",path,format!("model {model:?} uses thinkingLevel; thinkingBudget {} was dropped because both were set",config["thinkingBudget"])));
                config.as_object_mut().unwrap().remove("thinkingBudget");
            }
            _ => {}
        }
    }
    if capabilities.kind == Kind::Unavailable {
        if !config["thinkingBudget"].is_null()
            || !s(&config["thinkingLevel"]).is_empty()
            || !config["includeThoughts"].is_null() && !capabilities.include
        {
            ds.push(diagnostic("gemini_thinking_unsupported",path,format!("model {model:?} does not support configurable thinking; the thinking configuration was dropped")));
        }
        if !capabilities.include || config["includeThoughts"].is_null() {
            generation.as_object_mut().unwrap().remove("thinkingConfig");
            return Ok((String::new(), ds));
        }
        config.as_object_mut().unwrap().remove("thinkingBudget");
        config.as_object_mut().unwrap().remove("thinkingLevel");
        return Ok(("high".into(), ds));
    }
    let (intent, extra) = from_request(
        Format::Gemini,
        &json!({"generationConfig":{"thinkingConfig":config}}),
    )?;
    ds.extend(extra);
    if capabilities.kind == Kind::Unknown {
        return Ok((effective_effort(&intent), ds));
    }
    if capabilities.kind == Kind::Budget {
        if !s(&config["thinkingLevel"]).is_empty() {
            let budget = budget_for_effort(&intent.effort, capabilities.max);
            ds.push(diagnostic("gemini_level_to_budget",path,format!("model {model:?} uses thinkingBudget; thinkingLevel {:?} was converted to budget {budget}",s(&config["thinkingLevel"]))));
            config.as_object_mut().unwrap().remove("thinkingLevel");
            config["thinkingBudget"] = json!(budget);
        }
        let Some(mut budget) = config["thinkingBudget"].as_i64() else {
            return Ok((effective_effort(&intent), ds));
        };
        if budget < -1 {
            budget = -1;
        } else if budget == 0 && !capabilities.disable {
            ds.push(diagnostic(
                "gemini_thinking_disable_unsupported",
                path,
                format!(
                    "model {model:?} cannot disable thinking; using its minimum thinking budget {}",
                    capabilities.min
                ),
            ));
            budget = capabilities.min;
        } else if budget != 0 && budget != -1 {
            let clamped = budget.clamp(capabilities.min, capabilities.max);
            if budget != clamped {
                ds.push(diagnostic("gemini_budget_clamped",path,format!("thinking budget {budget} is outside the supported range [{},{}] for model {model:?}; using {clamped}",capabilities.min,capabilities.max)));
                budget = clamped;
            }
        }
        config["thinkingBudget"] = json!(budget);
        return Ok((effort_from_budget(budget).into(), ds));
    }
    if let Some(budget) = config["thinkingBudget"].as_i64() {
        config.as_object_mut().unwrap().remove("thinkingBudget");
        let level = level(
            model,
            if budget == 0 {
                "minimal"
            } else {
                effort_from_budget(budget)
            },
        )?;
        if budget == 0 {
            ds.push(diagnostic("gemini_thinking_disable_unsupported",path,format!("model {model:?} cannot disable thinking; using its lowest thinking level {level:?}")));
        } else {
            ds.push(diagnostic("gemini_budget_to_level",path,format!("model {model:?} uses thinkingLevel; thinkingBudget {budget} was converted to thinkingLevel {level:?}")));
        }
        config["thinkingLevel"] = json!(level);
        return Ok((level, ds));
    }
    if s(&config["thinkingLevel"]).is_empty() {
        return Ok((String::new(), ds));
    }
    let resolved = level(
        model,
        if intent.effort == "none" {
            "minimal"
        } else {
            &intent.effort
        },
    )?;
    if intent.effort == "none" {
        ds.push(diagnostic("gemini_thinking_disable_unsupported",path,format!("model {model:?} cannot disable thinking; using its lowest thinking level {resolved:?}")));
    } else if resolved != intent.effort {
        ds.push(diagnostic(
            "gemini_level_adjusted",
            path,
            format!(
                "model {model:?} does not support thinkingLevel {:?}; using {resolved:?}",
                s(&config["thinkingLevel"])
            ),
        ));
    }
    config["thinkingLevel"] = json!(resolved);
    Ok((resolved, ds))
}
pub(super) fn resolve_enabled_default(model: &str, mut intent: Intent, max: Option<u64>) -> Intent {
    if intent.mode != "enabled" || !intent.effort.is_empty() || intent.budget_tokens.is_some() {
        return intent;
    }
    match cap(model).kind {
        Kind::Budget => {
            if intent.source == "suffix" && max.is_some_and(|n| n > 0) {
                return intent;
            }
            intent.budget_tokens = Some(-1);
            intent.budget_source = "suffix".into();
        }
        Kind::Level => intent.effort = default_effort(model).into(),
        _ => {}
    }
    intent
}
pub(super) fn equivalent(model: &str, left: &Intent, right: &Intent) -> Result<bool, Error> {
    let left = render_gemini(model, left.clone(), None, 0.)?.config;
    let right = render_gemini(model, right.clone(), None, 0.)?.config;
    Ok(match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left["thinkingLevel"] == right["thinkingLevel"]
                && left["thinkingBudget"] == right["thinkingBudget"]
        }
        _ => false,
    })
}
pub(super) fn resolve_default(model: &str, mut i: Intent) -> Intent {
    if i.has_strength() {
        return i;
    }
    let c = cap(model);
    if c.kind == Kind::Budget {
        if model
            .to_ascii_lowercase()
            .starts_with("gemini-2.5-flash-lite")
        {
            i.mode = "disabled".into();
            i.effort = "none".into();
            i.budget_tokens = Some(0)
        } else {
            i.mode = "enabled".into();
            i.budget_tokens = Some(-1)
        }
        i.budget_source = "native".into()
    } else if c.kind == Kind::Level {
        let e = default_effort(model);
        if !e.is_empty() {
            i.mode = "enabled".into();
            i.effort = e.into()
        }
    }
    i
}
pub fn render_gemini(
    model: &str,
    intent: Intent,
    max_tokens: Option<u64>,
    adapter_percentage: f64,
) -> Result<GeminiRender, Error> {
    let (i, mut ds) = normalize_intent(intent)?;
    if i.is_empty() {
        return Ok(GeminiRender {
            diagnostics: ds,
            ..GeminiRender::default()
        });
    }
    let c = cap(model);
    let path = "generationConfig.thinkingConfig";
    let mut config = json!({});
    if let Some(b) = i.include_thoughts {
        config["includeThoughts"] = json!(b)
    }
    if c.kind == Kind::Unavailable {
        if i.has_strength() || i.include_thoughts.is_some() && !c.include {
            ds.push(diagnostic("gemini_thinking_unsupported",path,format!("model {model:?} does not support configurable thinking; the requested thinking controls were dropped")))
        }
        return Ok(GeminiRender {
            config: if c.include && i.include_thoughts.is_some() {
                Some(config)
            } else {
                None
            },
            effective_effort: if c.include {
                "high".into()
            } else {
                String::new()
            },
            diagnostics: ds,
        });
    }
    if c.kind == Kind::Unknown {
        if i.has_strength() {
            ds.push(diagnostic("gemini_unknown_capability",path,format!("model {model:?} has no known Gemini thinking configuration; the requested thinking strength was not applied")))
        }
        return Ok(GeminiRender {
            config: i.include_thoughts.map(|_| config),
            diagnostics: ds,
            ..GeminiRender::default()
        });
    }
    if c.kind == Kind::Budget {
        if i.mode == "disabled" || i.effort == "none" {
            let b = if c.disable {
                0
            } else {
                ds.push(diagnostic("gemini_thinking_disable_unsupported",path,format!("model {model:?} cannot disable thinking; using its minimum thinking budget {}",c.min)));
                c.min
            };
            config["thinkingBudget"] = json!(b);
            return Ok(GeminiRender {
                config: Some(config),
                effective_effort: effort_from_budget(b).into(),
                diagnostics: ds,
            });
        }
        let budget = if let Some(b) = i.budget_tokens {
            let clamped = if b == -1 { b } else { b.clamp(c.min, c.max) };
            if b != clamped {
                ds.push(diagnostic("gemini_budget_clamped",path,format!("thinking budget {b} is outside the supported range [{},{}] for model {model:?}; using {clamped}",c.min,c.max)))
            }
            Some(clamped)
        } else if !i.effort.is_empty() {
            Some(budget_for_effort(&i.effort, c.max))
        } else if !i.mode.is_empty() {
            if let Some(max) = max_tokens.filter(|v| *v > 0) {
                if max > i64::MAX as u64 {
                    return Err(Error::message(
                        "max_output_tokens is too large for a thinking budget",
                    ));
                }
                let p = if adapter_percentage <= 0. {
                    0.6
                } else {
                    adapter_percentage.min(1.)
                };
                Some(((max as f64 * p).round() as i64).clamp(c.min, c.max))
            } else {
                None
            }
        } else {
            None
        };
        if let Some(b) = budget {
            config["thinkingBudget"] = json!(b)
        }
        let effort = budget
            .map(|b| effort_from_budget(b).to_owned())
            .unwrap_or_else(|| {
                if i.mode == "enabled" || i.mode == "adaptive" {
                    default_effort(model).into()
                } else {
                    i.effort
                }
            });
        return Ok(GeminiRender {
            config: Some(config),
            effective_effort: effort,
            diagnostics: ds,
        });
    }
    if i.mode == "disabled" || i.effort == "none" {
        let l = level(model, "minimal")?;
        ds.push(diagnostic(
            "gemini_thinking_disable_unsupported",
            path,
            format!(
                "model {model:?} cannot disable thinking; using its lowest thinking level {l:?}"
            ),
        ));
        config["thinkingLevel"] = json!(l);
        return Ok(GeminiRender {
            config: Some(config),
            effective_effort: l,
            diagnostics: ds,
        });
    }
    let mut e = i.effort;
    if e.is_empty() {
        if let Some(b) = i.budget_tokens {
            e = effort_from_budget(b).into();
            ds.push(diagnostic("gemini_budget_to_level",path,format!("model {model:?} uses thinkingLevel; thinking budget {b} was converted to effort {e:?}")))
        }
    }
    if !e.is_empty() {
        let l = level(model, &e)?;
        if l != e {
            ds.push(diagnostic(
                "gemini_level_adjusted",
                path,
                format!("model {model:?} does not support thinking level {e:?}; using {l:?}"),
            ))
        }
        config["thinkingLevel"] = json!(l);
        e = l
    } else if i.mode == "enabled" || i.mode == "adaptive" {
        e = default_effort(model).into()
    }
    Ok(GeminiRender {
        config: Some(config),
        effective_effort: e,
        diagnostics: ds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highest_efforts_use_each_gemini_25_models_maximum() {
        for (model, high, maximum) in [
            ("gemini-2.5-pro", 24576, 32768),
            ("gemini-2.5-flash", 24576, 24576),
            ("gemini-2.5-flash-lite", 24576, 24576),
        ] {
            for (effort, expected) in [("high", high), ("xhigh", maximum), ("max", maximum)] {
                let rendered = render_gemini(
                    model,
                    Intent {
                        mode: "enabled".into(),
                        effort: effort.into(),
                        ..Intent::default()
                    },
                    None,
                    0.,
                )
                .unwrap();
                assert_eq!(rendered.config.unwrap()["thinkingBudget"], expected);

                let mut generation = json!({"thinkingConfig":{"thinkingLevel":effort}});
                normalize_config(model, &mut generation).unwrap();
                assert_eq!(generation["thinkingConfig"]["thinkingBudget"], expected);
            }
        }
    }
}
