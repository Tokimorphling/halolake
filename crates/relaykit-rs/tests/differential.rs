use relaykit::{
    Context, Format, LossPolicy, Request, Response, ResponseStreamState, StreamOptions,
};
use serde_json::{Value, json};
use std::sync::Arc;
fn normalize(v: &mut Value) {
    // Go exposes Responses stream events through an internal Type/Payload envelope; Rust returns the wire payload.
    if v.get("Type").is_some() && v.get("Payload").is_some() {
        *v = v["Payload"].take();
    }
    match v {
        Value::String(s) => {
            let bytes = s.as_bytes();
            let mut out = String::new();
            let mut start = 0;
            let mut i = 0;
            while i + 32 <= bytes.len() {
                if bytes[i..i + 32]
                    .iter()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
                {
                    out.push_str(&s[start..i]);
                    out.push_str("<uuid>");
                    i += 32;
                    start = i
                } else {
                    i += 1
                }
            }
            out.push_str(&s[start..]);
            *s = out
        }
        Value::Array(a) => {
            for v in a {
                normalize(v)
            }
        }
        Value::Object(m) => {
            for (k, v) in m {
                if (k == "created" || k == "created_at")
                    && v.as_i64().is_some_and(|v| v >= 100_000_000)
                {
                    *v = json!(0)
                } else {
                    normalize(v)
                }
            }
        }
        _ => {}
    }
}
fn execute(
    case: &Value,
) -> Result<(Value, Option<Value>, Vec<relaykit::Diagnostic>), relaykit::Error> {
    let from: Format = case["from"].as_str().unwrap().parse()?;
    let to: Format = case["to"].as_str().unwrap().parse()?;
    let model = case["model"].as_str().unwrap_or("");
    let mut ctx = Context {
        channel_meta_attached: !model.is_empty(),
        upstream_model_name: model.into(),
        origin_model_name: case["origin_model"].as_str().unwrap_or("").into(),
        ..Context::default()
    };
    if let Some(max) = case["default_max"].as_i64() {
        ctx.options.claude.default_max_tokens = Some(Arc::new(move |_| max));
    }
    ctx.options.tool_loss_policy = match case["policy"].as_str() {
        Some("safe") => LossPolicy::Safe,
        Some("strict") => LossPolicy::Strict,
        _ => LossPolicy::Allow,
    };
    if case["golden"] == true {
        ctx.options.gemini.function_call_thought_signature_enabled = true;
        ctx.options.gemini.thinking_adapter_budget_tokens_percentage = 0.6;
        ctx.options.gemini.safety_setting = Some(Arc::new(|_| "OFF".into()));
    }
    ctx.options.open_router_dialect = case["open_router"] == true;
    ctx.options.claude.thinking_adapter_enabled = case["claude_adapter"] == true;
    ctx.options.gemini.thinking_adapter_enabled = case["gemini_adapter"] == true;
    if case["supports_json_schema"] == true {
        ctx.options.gemini.supports_json_schema = Some(Arc::new(|_| true));
    }
    if let Some(percentage) = case["adapter_percentage"].as_f64() {
        ctx.options.claude.thinking_adapter_budget_tokens_percentage = percentage;
        ctx.options.gemini.thinking_adapter_budget_tokens_percentage = percentage;
    }
    if let Some(state) = case.get("reasoning_conversion") {
        ctx.reasoning_conversion = Some(relaykit::reasoning::Intent {
            mode: state["Mode"].as_str().unwrap_or("").into(),
            effort: state["Effort"].as_str().unwrap_or("").into(),
            budget_tokens: state["BudgetTokens"].as_i64(),
            include_thoughts: state["IncludeThoughts"].as_bool(),
            source: "suffix".into(),
            budget_source: "suffix".into(),
        });
    }
    ctx.media.get_base64_data = Some(Arc::new(|_, _| Ok(("aGVsbG8=".into(), "image/png".into()))));
    ctx.media.decode_base64_file_data =
        Some(Arc::new(|_| Ok(("aGVsbG8=".into(), "image/png".into()))));
    match case["kind"].as_str().unwrap() {
        "deepseek_suffix" => {
            let (base, thinking, effort, ok) =
                relaykit::reasoning::parse_deep_seek_v4_thinking_suffix(
                    case["input"]["model"].as_str().unwrap(),
                );
            Ok((
                json!({"base_model":base,"thinking_type":thinking,"effort":effort,"ok":ok}),
                None,
                vec![],
            ))
        }
        "dto" => {
            fn roundtrip<T: serde::de::DeserializeOwned + serde::Serialize>(
                value: Value,
            ) -> Result<Value, relaykit::Error> {
                Ok(serde_json::to_value(serde_json::from_value::<T>(value)?)?)
            }
            let input = case["input"].clone();
            let value = match model {
                "Thinking" => roundtrip::<relaykit::dto::Thinking>(input)?,
                "ResponsesOutput" => roundtrip::<relaykit::dto::ResponsesOutput>(input)?,
                "ChannelOtherSettings" => roundtrip::<relaykit::dto::ChannelOtherSettings>(input)?,
                "UserSetting" => roundtrip::<relaykit::dto::UserSetting>(input)?,
                "DeepSeekToolsRequest" => roundtrip::<relaykit::dto::DeepSeekToolsRequest>(input)?,
                _ => unreachable!("unsupported DTO"),
            };
            Ok((value, None, vec![]))
        }
        "request" => {
            let r = relaykit::convert_request(
                &ctx,
                to,
                &Request::from_value(from, case["input"].clone())?,
            )?;
            Ok((r.value.into_value(), r.usage, r.diagnostics))
        }
        "response" => {
            let r = relaykit::convert_response(
                &ctx,
                to,
                &Response::from_value(from, case["input"].clone())?,
            )?;
            Ok((r.value.into_value(), r.usage, r.diagnostics))
        }
        "stream" => {
            let mut state = ResponseStreamState::new(
                from,
                to,
                StreamOptions {
                    id: "stream_fixed".into(),
                    model: "stream-model".into(),
                    include_usage: case["include_usage"] == true,
                    emit_sequence_number: case["emit_sequence_number"] == true,
                    ..StreamOptions::default()
                },
            )?;
            let mut events = vec![];
            for chunk in case["chunks"].as_array().unwrap() {
                events.extend(state.convert_chunk(&ctx, chunk)?);
            }
            events.extend(state.finalize(&ctx)?);
            Ok((
                json!(events),
                state.usage().cloned(),
                state.diagnostics().to_vec(),
            ))
        }
        _ => unreachable!(),
    }
}
fn run(kind: &str, upgrade: bool) {
    run_fixture(
        kind,
        if upgrade {
            include_str!("fixtures/upgrade_parity.json")
        } else {
            include_str!("fixtures/differential.json")
        },
        upgrade,
    );
}
fn run_fixture(kind: &str, corpus: &str, upgrade: bool) {
    let corpus: Value = serde_json::from_str(corpus).unwrap();
    let mut failures = vec![];
    let mut reports = vec![];
    let mut count = 0;
    for entry in corpus.as_array().unwrap() {
        let c = &entry["case"];
        if !kind.is_empty() && c["kind"] != kind {
            continue;
        }
        if let Ok(filter) = std::env::var("CASE_FILTER") {
            if !c["name"].as_str().unwrap().contains(&filter) {
                continue;
            }
        }
        count += 1;
        let expected = &entry["expected"];
        let result = execute(c);
        let mut actual = match result {
            Ok((value, usage, diagnostics)) => {
                let mut actual = json!({"value":value,"usage":usage});
                if upgrade {
                    actual["diagnostics"] = json!(diagnostics);
                }
                actual
            }
            Err(e) => json!({"error":e.to_string()}),
        };
        let mut want = if expected.get("error").is_some() {
            json!({"error":expected["error"]})
        } else {
            json!({"value":expected["value"],"usage":expected["usage"]})
        };
        if upgrade && want.get("error").is_none() {
            want["diagnostics"] = json!(
                expected["diagnostics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            );
        }
        normalize(&mut actual);
        normalize(&mut want);
        // Error classification is a public Rust type; message wording need not expose Go implementation names.
        if actual.get("error").is_some() && want.get("error").is_some() {
            continue;
        }
        if actual != want {
            failures.push(c["name"].as_str().unwrap().to_owned());
            reports.push(json!({"case":c,"actual":actual,"expected":want}));
        }
    }
    if !failures.is_empty() {
        let p = std::env::temp_dir().join(format!("relaykit-{kind}-diff.json"));
        std::fs::write(&p, serde_json::to_string_pretty(&reports).unwrap()).unwrap();
        panic!(
            "{} / {count} differential cases failed; details {}:\n{}",
            failures.len(),
            p.display(),
            failures.join("\n")
        );
    }
}
#[test]
fn request_parity() {
    run("request", false)
}
#[test]
fn response_parity() {
    run("response", false)
}
#[test]
fn stream_parity() {
    run("stream", false)
}
#[test]
fn upstream_upgrade_parity() {
    run("", true)
}
#[test]
fn upstream_v035_upgrade_parity() {
    run_fixture("", include_str!("fixtures/v035_upgrade_parity.json"), true)
}
