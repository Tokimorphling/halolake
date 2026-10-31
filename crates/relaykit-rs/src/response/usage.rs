use super::{arr, n, owned_object, s};
use serde_json::{Value, json};

pub(crate) fn normalize_usage(v: &Value) -> Value {
    let mut o = owned_object([
        ("prompt_tokens", json!(n(&v["prompt_tokens"]))),
        ("completion_tokens", json!(n(&v["completion_tokens"]))),
        ("total_tokens", json!(n(&v["total_tokens"]))),
        (
            "prompt_tokens_details",
            input_details(&v["prompt_tokens_details"]),
        ),
        (
            "completion_tokens_details",
            output_details(&v["completion_tokens_details"]),
        ),
        ("input_tokens", json!(n(&v["input_tokens"]))),
        ("output_tokens", json!(n(&v["output_tokens"]))),
        ("input_tokens_details", Value::Null),
        (
            "claude_cache_creation_5_m_tokens",
            json!(n(&v["claude_cache_creation_5_m_tokens"])),
        ),
        (
            "claude_cache_creation_1_h_tokens",
            json!(n(&v["claude_cache_creation_1_h_tokens"])),
        ),
    ]);
    for key in ["prompt_cache_hit_tokens", "usage_semantic", "usage_source"] {
        if !v[key].is_null() && v[key] != 0 && v[key] != "" {
            o[key] = v[key].clone();
        }
    }
    // Cost is Go's `any`, so a present numeric zero is not an omitted value.
    if !v["cost"].is_null() {
        o["cost"] = v["cost"].clone();
    }
    if !v["billing_usage"].is_null() {
        o["billing_usage"] = clone_billing(&v["billing_usage"]);
    }
    if !v["input_tokens_details"].is_null() {
        o["input_tokens_details"] = input_details(&v["input_tokens_details"]);
    }
    if !v["output_tokens_details"].is_null() {
        o["output_tokens_details"] = output_details(&v["output_tokens_details"]);
    }
    o
}
fn input_details(v: &Value) -> Value {
    let mut o = json!({"cached_tokens":n(&v["cached_tokens"]),"text_tokens":n(&v["text_tokens"]),"audio_tokens":n(&v["audio_tokens"]),"image_tokens":n(&v["image_tokens"])});
    for k in ["cached_creation_tokens", "cache_write_tokens", "cache_creation_input_tokens"] {
        if n(&v[k]) != 0 {
            o[k] = v[k].clone();
        }
    }
    if v["cached_tokens_details"].is_object() {
        o["cached_tokens_details"] = json!({});
        for key in ["text_tokens", "image_tokens", "audio_tokens"] {
            if !v["cached_tokens_details"][key].is_null() {
                o["cached_tokens_details"][key] = v["cached_tokens_details"][key].clone();
            }
        }
    }
    if v["cache_write_tokens_details"].is_object() {
        o["cache_write_tokens_details"] = crate::wire::normalize_named(
            "CacheWriteTokenDetails",
            &v["cache_write_tokens_details"],
        )
        .expect("cache-write details have already passed wire validation");
    }
    o
}
fn output_details(v: &Value) -> Value {
    json!({"text_tokens":n(&v["text_tokens"]),"audio_tokens":n(&v["audio_tokens"]),"image_tokens":n(&v["image_tokens"]),"reasoning_tokens":n(&v["reasoning_tokens"])})
}
fn claude_native(v: &Value) -> Value {
    let mut o = json!({"input_tokens":n(&v["input_tokens"]),"output_tokens":n(&v["output_tokens"]),"cache_creation_input_tokens":n(&v["cache_creation_input_tokens"]),"cache_read_input_tokens":n(&v["cache_read_input_tokens"]),"claude_cache_creation_5_m_tokens":n(&v["claude_cache_creation_5_m_tokens"]),"claude_cache_creation_1_h_tokens":n(&v["claude_cache_creation_1_h_tokens"])});
    for k in ["cache_creation", "server_tool_use"] {
        if !v[k].is_null() {
            o[k] = crate::wire::normalize_named(
                if k == "cache_creation" {
                    "ClaudeCacheCreationUsage"
                } else {
                    "ClaudeServerToolUse"
                },
                &v[k],
            )
            .unwrap_or_else(|_| v[k].clone());
        }
    }
    o
}
fn gemini_native(v: &Value) -> Value {
    let mut o = json!({});
    for k in [
        "promptTokenCount",
        "toolUsePromptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
        "thoughtsTokenCount",
        "cachedContentTokenCount",
    ] {
        o[k] = json!(n(&v[k]));
    }
    for k in [
        "promptTokensDetails",
        "toolUsePromptTokensDetails",
        "candidatesTokensDetails",
    ] {
        o[k] = Value::Array(
            arr(&v[k])
                .iter()
                .map(|d| json!({"modality":s(&d["modality"]),"tokenCount":n(&d["tokenCount"])}))
                .collect(),
        );
    }
    o
}
fn has_tokens(v: &Value, source: &str) -> bool {
    let keys: &[&str] = match source {
        "claude_messages" => &[
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "claude_cache_creation_5_m_tokens",
            "claude_cache_creation_1_h_tokens",
        ],
        "gemini_chat" => &[
            "promptTokenCount",
            "toolUsePromptTokenCount",
            "candidatesTokenCount",
            "totalTokenCount",
            "thoughtsTokenCount",
            "cachedContentTokenCount",
        ],
        _ => &[
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "input_tokens",
            "output_tokens",
            "prompt_cache_hit_tokens",
            "claude_cache_creation_5_m_tokens",
            "claude_cache_creation_1_h_tokens",
        ],
    };
    if keys.iter().any(|key| n(&v[key]) != 0) {
        return true;
    }
    match source {
        "claude_messages" => ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"]
            .iter()
            .any(|key| n(&v["cache_creation"][key]) != 0),
        "gemini_chat" => [
            "promptTokensDetails",
            "toolUsePromptTokensDetails",
            "candidatesTokensDetails",
        ]
        .iter()
        .any(|key| arr(&v[key]).iter().any(|d| n(&d["tokenCount"]) != 0)),
        _ => {
            for key in ["prompt_tokens_details", "input_tokens_details"] {
                let d = &v[key];
                if [
                    "cached_tokens",
                    "cached_creation_tokens",
                    "cache_write_tokens",
                    "text_tokens",
                    "image_tokens",
                    "audio_tokens",
                ]
                .iter()
                .any(|k| n(&d[k]) != 0)
                    || ["text_tokens", "image_tokens", "audio_tokens"]
                        .iter()
                        .any(|k| n(&d["cached_tokens_details"][k]) != 0)
                    || ["ttl_30m_tokens", "ttl_12h_tokens"]
                        .iter()
                        .any(|k| n(&d["cache_write_tokens_details"][k]) != 0)
                {
                    return true;
                }
            }
            [
                "reasoning_tokens",
                "text_tokens",
                "image_tokens",
                "audio_tokens",
            ]
            .iter()
            .any(|key| n(&v["completion_tokens_details"][key]) != 0)
        }
    }
}
fn clone_billing(v: &Value) -> Value {
    let mut o = json!({});
    for key in ["source", "semantic"] {
        if !s(&v[key]).is_empty() {
            o[key] = v[key].clone();
        }
    }
    if v["estimated"] == true {
        o["estimated"] = json!(true);
    }
    for (key, format) in [
        ("openai_usage", 0),
        ("claude_usage", 1),
        ("gemini_usage_metadata", 2),
    ] {
        if !v[key].is_null() {
            let mut native = match format {
                0 => normalize_usage(&without_billing(&v[key])),
                1 => claude_native(&v[key]),
                _ => gemini_native(&v[key]),
            };
            native.as_object_mut().unwrap().remove("billing_usage");
            o[key] = native;
        }
    }
    o
}
fn without_billing(v: &Value) -> Value {
    let mut o = v.clone();
    if let Some(m) = o.as_object_mut() {
        m.remove("billing_usage");
    }
    o
}
fn billing(v: &Value, source: &str) -> Option<Value> {
    if !v["billing_usage"].is_null() {
        return Some(clone_billing(&v["billing_usage"]));
    }
    if !has_tokens(v, source) {
        return None;
    }
    Some(match source {
        "claude_messages" => owned_object([
            ("source", json!(source)),
            ("semantic", json!("anthropic")),
            ("claude_usage", claude_native(v)),
        ]),
        "gemini_chat" => owned_object([
            ("source", json!(source)),
            ("semantic", json!("gemini")),
            ("gemini_usage_metadata", gemini_native(v)),
        ]),
        _ => owned_object([
            ("source", json!(source)),
            ("semantic", json!("openai")),
            ("openai_usage", normalize_usage(&without_billing(v))),
        ]),
    })
}
fn copy_common(out: &mut Value, src: &Value, source: &str) {
    for key in ["usage_semantic", "usage_source"] {
        if !src[key].is_null() && src[key] != "" {
            out[key] = src[key].clone();
        }
    }
    if !src["cost"].is_null() {
        out["cost"] = src["cost"].clone();
    }
    if let Some(b) = billing(src, source) {
        out["billing_usage"] = b;
    }
    for k in [
        "claude_cache_creation_5_m_tokens",
        "claude_cache_creation_1_h_tokens",
    ] {
        out[k] = json!(n(&src[k]));
    }
}
pub(crate) fn usage_from_chat(src: &Value) -> Value {
    let mut o = normalize_usage(&Value::Null);
    copy_common(&mut o, src, "oai_chat");
    let p = n(&src["prompt_tokens"]);
    let c = n(&src["completion_tokens"]);
    let t = n(&src["total_tokens"]);
    o["prompt_tokens"] = json!(p);
    o["input_tokens"] = json!(p);
    o["completion_tokens"] = json!(c);
    o["output_tokens"] = json!(c);
    o["total_tokens"] = json!(if t != 0 { t } else { p + c });
    let d = &src["prompt_tokens_details"];
    if [
        "cached_tokens",
        "image_tokens",
        "audio_tokens",
        "cached_creation_tokens",
        "cache_write_tokens",
        "cache_creation_input_tokens",
        "text_tokens",
    ]
    .iter()
    .any(|k| n(&d[k]) != 0)
    {
        o["input_tokens_details"] = input_details(d);
    }
    o["completion_tokens_details"] = output_details(&src["completion_tokens_details"]);
    o
}
pub(crate) fn usage_from_responses(src: &Value) -> Value {
    let mut o = normalize_usage(&Value::Null);
    copy_common(&mut o, src, "oai_responses");
    let p = n(&src["input_tokens"]);
    let c = n(&src["output_tokens"]);
    let t = n(&src["total_tokens"]);
    o["prompt_tokens"] = json!(p);
    o["input_tokens"] = json!(p);
    o["completion_tokens"] = json!(c);
    o["output_tokens"] = json!(c);
    o["total_tokens"] = json!(if t != 0 { t } else { p + c });
    o["prompt_tokens_details"] = input_details(&src["input_tokens_details"]);
    o["completion_tokens_details"] = output_details(&src["completion_tokens_details"]);
    o
}
pub(crate) fn usage_from_claude(src: &Value) -> Value {
    let mut o = normalize_usage(&Value::Null);
    if src.is_null() {
        return o;
    }
    let cr = n(&src["cache_read_input_tokens"]);
    let cc = n(&src["cache_creation_input_tokens"]);
    let c5 = n(&src["cache_creation"]["ephemeral_5m_input_tokens"]);
    let c1 = n(&src["cache_creation"]["ephemeral_1h_input_tokens"]);
    let cw = cc.max(c5 + c1);
    let p = n(&src["input_tokens"]) + cr + cw;
    let c = n(&src["output_tokens"]);
    o["prompt_tokens"] = json!(p);
    o["input_tokens"] = json!(p);
    o["completion_tokens"] = json!(c);
    o["total_tokens"] = json!(p + c);
    o["usage_semantic"] = json!("openai");
    o["usage_source"] = json!("anthropic");
    o["prompt_tokens_details"]["cached_tokens"] = json!(cr);
    if cc != 0 {
        o["prompt_tokens_details"]["cached_creation_tokens"] = json!(cc);
    }
    if cw != 0 {
        o["prompt_tokens_details"]["cache_write_tokens"] = json!(cw);
    }
    o["claude_cache_creation_5_m_tokens"] = json!(c5 + (cc - c5 - c1).max(0));
    o["claude_cache_creation_1_h_tokens"] = json!(c1);
    if let Some(b) = billing(src, "claude_messages") {
        o["billing_usage"] = b;
    }
    o
}
pub(crate) fn usage_from_gemini(src: &Value, fallback: i64) -> Option<Value> {
    if src.is_null() {
        if fallback <= 0 {
            return None;
        }
        let mut o = normalize_usage(&Value::Null);
        o["prompt_tokens"] = json!(fallback);
        o["prompt_tokens_details"]["text_tokens"] = json!(fallback);
        return Some(o);
    }
    let mut o = normalize_usage(&Value::Null);
    let mut p = n(&src["promptTokenCount"]) + n(&src["toolUsePromptTokenCount"]);
    if p <= 0 && fallback > 0 {
        p = fallback;
    }
    let mut c = n(&src["candidatesTokenCount"]) + n(&src["thoughtsTokenCount"]);
    let t = n(&src["totalTokenCount"]);
    if t > 0 && c <= 0 {
        c = t - p;
    }
    o["prompt_tokens"] = json!(p);
    o["completion_tokens"] = json!(c);
    o["total_tokens"] = json!(t);
    o["completion_tokens_details"]["reasoning_tokens"] = json!(n(&src["thoughtsTokenCount"]));
    o["prompt_tokens_details"]["cached_tokens"] = json!(n(&src["cachedContentTokenCount"]));
    for (key, target) in [
        ("promptTokensDetails", "prompt_tokens_details"),
        ("toolUsePromptTokensDetails", "prompt_tokens_details"),
        ("candidatesTokensDetails", "completion_tokens_details"),
    ] {
        for d in arr(&src[key]) {
            let field = match s(&d["modality"]) {
                "AUDIO" => "audio_tokens",
                "IMAGE" => "image_tokens",
                "TEXT" => "text_tokens",
                _ => continue,
            };
            o[target][field] = json!(n(&o[target][field]) + n(&d["tokenCount"]));
        }
    }
    if p > 0
        && n(&o["prompt_tokens_details"]["text_tokens"]) == 0
        && n(&o["prompt_tokens_details"]["audio_tokens"]) == 0
    {
        o["prompt_tokens_details"]["text_tokens"] = json!(p);
    }
    if let Some(b) = billing(src, "gemini_chat") {
        o["billing_usage"] = b;
    }
    Some(o)
}
pub(crate) fn claude_usage(src: &Value) -> Value {
    let b = billing(src, "oai_chat");
    if let Some(b) = &b {
        if !b["claude_usage"].is_null()
            && (s(&b["source"]) == "claude_messages" || s(&b["semantic"]) == "anthropic")
        {
            let mut o = b["claude_usage"].clone();
            o["billing_usage"] = b.clone();
            return o;
        }
    }
    let d = &src["prompt_tokens_details"];
    let cr = n(&d["cached_tokens"]);
    let cc = n(&d["cached_creation_tokens"]);
    let cw = cc
        .max(n(&d["cache_write_tokens"]))
        .max(n(&d["cache_creation_input_tokens"]))
        .max(0);
    let p = if s(&src["usage_semantic"]) == "anthropic" {
        n(&src["prompt_tokens"])
    } else {
        (n(&src["prompt_tokens"]) - cr - cw).max(0)
    };
    let mut o = claude_native(&Value::Null);
    o["input_tokens"] = json!(p);
    o["output_tokens"] = json!(n(&src["completion_tokens"]));
    o["cache_read_input_tokens"] = json!(cr);
    o["cache_creation_input_tokens"] = json!(cw);
    let c5 = n(&src["claude_cache_creation_5_m_tokens"]);
    let c1 = n(&src["claude_cache_creation_1_h_tokens"]);
    let c5 = c5 + (cc - c5 - c1).max(0);
    if c5 > 0 || c1 > 0 {
        o["cache_creation"] = json!({});
        if c5 != 0 {
            o["cache_creation"]["ephemeral_5m_input_tokens"] = json!(c5);
        }
        if c1 != 0 {
            o["cache_creation"]["ephemeral_1h_input_tokens"] = json!(c1);
        }
    }
    if let Some(b) = b {
        o["billing_usage"] = b;
    }
    o
}
pub(crate) fn gemini_usage(src: &Value) -> Value {
    let b = billing(src, "oai_chat");
    if let Some(b) = &b {
        if !b["gemini_usage_metadata"].is_null()
            && (s(&b["source"]) == "gemini_chat" || s(&b["semantic"]) == "gemini")
        {
            let mut o = b["gemini_usage_metadata"].clone();
            o["billing_usage"] = b.clone();
            return o;
        }
    }
    let mut o = gemini_native(&Value::Null);
    let p = n(&src["prompt_tokens"]);
    let c = n(&src["completion_tokens"]);
    let t = n(&src["total_tokens"]);
    o["promptTokenCount"] = json!(p);
    o["candidatesTokenCount"] = json!(c);
    o["totalTokenCount"] = json!(if t == 0 { p + c } else { t });
    // Native zero-value slices marshal as null. Snapshot slices marshal as [].
    for k in [
        "promptTokensDetails",
        "toolUsePromptTokensDetails",
        "candidatesTokensDetails",
    ] {
        o[k] = Value::Null;
    }
    if let Some(b) = b {
        o["billing_usage"] = b;
    }
    o
}
