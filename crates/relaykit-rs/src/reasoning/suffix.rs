use super::*;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelModifier {
    pub key: String,
    pub value: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelModifierSpec {
    pub raw: String,
    pub base: String,
    pub modifiers: Vec<ModelModifier>,
}
pub fn parse_model_modifiers(model: &str) -> ModelModifierSpec {
    let mut spec = ModelModifierSpec {
        raw: model.into(),
        base: model.into(),
        modifiers: vec![],
    };
    let parts: Vec<_> = model.split('@').collect();
    let mut first = parts.len();
    for i in (1..parts.len()).rev() {
        let Some((key, value)) = parts[i].split_once(':') else {
            break;
        };
        if key.is_empty()
            || !key.bytes().enumerate().all(|(i, c)| {
                c.is_ascii_alphabetic() || i > 0 && (c.is_ascii_digit() || c == b'_' || c == b'-')
            })
        {
            break;
        }
        first = i;
        spec.modifiers.insert(
            0,
            ModelModifier {
                key: key.to_ascii_lowercase(),
                value: value.into(),
            },
        );
    }
    if first == parts.len() {
        return spec;
    }
    let base = parts[..first].join("@");
    if base.is_empty() {
        spec.modifiers.clear();
    } else {
        spec.base = base
    }
    spec
}
pub fn parse_thinking_modifier(raw: &str) -> Option<Intent> {
    let v = raw.trim().to_ascii_lowercase();
    let mut i = Intent {
        source: "suffix".into(),
        ..Intent::default()
    };
    match v.as_str() {
        "on" => i.mode = "enabled".into(),
        "adaptive" => i.mode = "adaptive".into(),
        "off" => {
            i.mode = "disabled".into();
            i.effort = "none".into()
        }
        _ => {
            let b: i64 = v.parse().ok()?;
            if b < -1 {
                return None;
            }
            if b == 0 {
                i.mode = "disabled".into();
                i.effort = "none".into()
            } else {
                i.mode = "enabled".into();
                i.budget_tokens = Some(b);
                i.budget_source = "suffix".into()
            }
        }
    }
    Some(i)
}
pub fn trim_effort_suffix_with_suffixes(model: &str, suffixes: &[&str]) -> (String, String, bool) {
    for &suffix in suffixes {
        if let Some(base) = model.strip_suffix(suffix) {
            return (base.into(), suffix.trim_start_matches('-').into(), true);
        }
    }
    (model.into(), String::new(), false)
}
fn valid_family(model: &str, prefix: &str) -> bool {
    model.strip_prefix(prefix).is_some_and(|tail| {
        tail.as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && tail
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
    })
}
pub fn parse_openai_reasoning_effort_from_model_suffix(
    model: &str,
    preserve: Option<&dyn Fn(&str) -> bool>,
) -> (String, String) {
    if preserve.is_some_and(|p| p(model)) {
        return (String::new(), model.into());
    }
    let (base, effort, found) = trim_effort_suffix_with_suffixes(
        model,
        &[
            "-max", "-xhigh", "-high", "-medium", "-low", "-minimal", "-none",
        ],
    );
    let bare = base.rsplit('/').next().unwrap_or("");
    let valid = valid_family(bare, "gpt-")
        || bare.starts_with('o')
            && bare
                .as_bytes()
                .get(1)
                .is_some_and(|c| b"123456789".contains(c))
            && bare[1..]
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c));
    if found && valid {
        (effort, base)
    } else {
        (String::new(), model.into())
    }
}
fn provider(model: &str, prefix: &str, allow: bool) -> Result<(String, Intent, bool), Error> {
    let original = || (model.into(), Intent::default(), false);
    let (ns, bare) = model
        .rsplit_once('/')
        .map(|(p, b)| (format!("{p}/"), b))
        .unwrap_or_else(|| (String::new(), model));
    if !allow || !bare.starts_with(prefix) {
        return Ok(original());
    }
    let mut i = Intent {
        source: "suffix".into(),
        ..Intent::default()
    };
    let base;
    if let Some((b, budget)) = bare.rsplit_once("-thinking-") {
        base = b;
        i.budget_tokens = Some(budget.parse::<i64>().map_err(|_| {
            Error::message(format!("invalid thinking budget suffix on model {model:?}"))
        })?);
        i.budget_source = "suffix".into();
        i.include_thoughts = Some(true)
    } else if let Some(b) = bare.strip_suffix("-nothinking") {
        base = b;
        i.mode = "disabled".into();
        i.effort = "none".into()
    } else if let Some(b) = bare.strip_suffix("-thinking") {
        base = b;
        i.mode = "enabled".into();
        i.include_thoughts = Some(true)
    } else {
        let (b, e, found) = trim_effort_suffix_with_suffixes(
            bare,
            &[
                "-max", "-xhigh", "-high", "-medium", "-low", "-minimal", "-none",
            ],
        );
        if !found || !valid_family(&b, prefix) {
            return Ok(original());
        }
        i.mode = if e == "none" { "disabled" } else { "enabled" }.into();
        if e != "none" {
            i.include_thoughts = Some(true)
        }
        i.effort = e;
        return Ok((format!("{ns}{b}"), i, true));
    }
    if !valid_family(base, prefix) {
        return Ok(original());
    }
    Ok((format!("{ns}{base}"), i, true))
}
pub fn parse_claude_model_suffix(
    model: &str,
    allow: bool,
) -> Result<(String, Intent, bool), Error> {
    provider(model, "claude-", allow)
}
pub fn parse_gemini_model_suffix(
    model: &str,
    allow: bool,
) -> Result<(String, Intent, bool), Error> {
    provider(model, "gemini-", allow)
}
pub fn parse_known_provider_model_suffix(
    model: &str,
    allow: bool,
) -> Result<(String, Intent, bool), Error> {
    let bare = model.rsplit('/').next().unwrap_or("");
    if bare.starts_with("claude-") {
        parse_claude_model_suffix(model, allow)
    } else if bare.starts_with("gemini-") {
        parse_gemini_model_suffix(model, allow)
    } else {
        Ok((model.into(), Intent::default(), false))
    }
}
pub fn parse_deep_seek_v4_thinking_suffix(model: &str) -> (String, String, String, bool) {
    let (base, e, found) = trim_effort_suffix_with_suffixes(model, &["-none", "-low", "-max"]);
    if !found || base != "deepseek-flash" && !base.starts_with("deepseek-v4-") {
        return (model.into(), String::new(), String::new(), false);
    }
    if e == "none" {
        (base, "disabled".into(), String::new(), true)
    } else {
        (base, "enabled".into(), e, true)
    }
}
