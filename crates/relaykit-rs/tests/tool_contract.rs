use relaykit::{Context, Error, Format, LossPolicy, Request};
use serde_json::{Value, json};
fn diagnostic_set(v: &Value, unstable_group_indexes: bool) -> Vec<String> {
    let mut ds: Vec<String> = v
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| {
                    let mut v = v.clone();
                    // Go iterates native Gemini tool fragments through a map;
                    // those flattened diagnostic indexes vary between runs.
                    // Their code, field suffix, message and severity are stable.
                    if unstable_group_indexes {
                        if let Some(path) = v["path"].as_str() {
                            if let Some(end) = path.find(']') {
                                if path.starts_with("tools[") {
                                    v["path"] = json!(format!("tools[*]{}", &path[end + 1..]));
                                }
                            }
                        }
                    }
                    v.to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    ds.sort();
    ds
}
#[test]
fn tools_and_hosted_history_match_go() {
    let corpus: Value = serde_json::from_str(include_str!("fixtures/tool_contract.json")).unwrap();
    let mut failures = vec![];
    for entry in corpus.as_array().unwrap() {
        let c = &entry["case"];
        if std::env::var("TOOL_CASE_FILTER")
            .is_ok_and(|f| !c["name"].as_str().unwrap().contains(&f))
        {
            continue;
        }
        let from: Format = c["from"].as_str().unwrap().parse().unwrap();
        let to = c["to"].as_str().unwrap().parse().unwrap();
        let mut ctx = Context {
            channel_meta_attached: true,
            upstream_model_name: "upstream-model".into(),
            ..Default::default()
        };
        ctx.options.tool_loss_policy = match c["policy"].as_str() {
            Some("safe") => LossPolicy::Safe,
            Some("strict") => LossPolicy::Strict,
            _ => LossPolicy::Allow,
        };
        let result = Request::from_value(from, c["input"].clone())
            .and_then(|input| relaykit::convert_request(&ctx, to, &input));
        let expected = &entry["expected"];
        let actual = match result {
            Ok(r) => json!({"value":r.value.into_value(),"diagnostics":r.diagnostics}),
            Err(Error::ConversionLoss(ds)) => json!({"error":true,"diagnostics":ds}),
            Err(e) => json!({"error":e.to_string()}),
        };
        if expected.get("error").is_some() {
            if actual.get("error").is_some() {
                if expected["diagnostics"].is_array() {
                    let rejected = json!(
                        expected["diagnostics"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|d| c["policy"] == "strict" || d["severity"] == "error")
                            .cloned()
                            .collect::<Vec<_>>()
                    );
                    let unstable = c["name"].as_str().unwrap().starts_with("native_group/");
                    if diagnostic_set(&actual["diagnostics"], unstable)
                        == diagnostic_set(&rejected, unstable)
                    {
                        continue;
                    }
                } else {
                    continue;
                }
            }
        } else if actual.get("error").is_none()
            && actual["value"] == expected["value"]
            && diagnostic_set(
                &actual["diagnostics"],
                c["name"].as_str().unwrap().starts_with("native_group/"),
            ) == diagnostic_set(
                &expected["diagnostics"],
                c["name"].as_str().unwrap().starts_with("native_group/"),
            )
        {
            continue;
        }
        failures.push(json!({"case":c,"actual":actual,"expected":expected}));
    }
    if !failures.is_empty() {
        let path = std::env::temp_dir().join("relaykit-tools-diff.json");
        std::fs::write(&path, serde_json::to_string_pretty(&failures).unwrap()).unwrap();
        panic!(
            "{} tool parity failures: {}",
            failures.len(),
            path.display()
        );
    }
}

#[test]
fn host_search_encoder_preserves_forced_choice_without_function_tools() {
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = seen.clone();
    let mut ctx = Context::default();
    ctx.options.tool_loss_policy = LossPolicy::Safe;
    ctx.options.web_search = Some(Arc::new(move |call, out| {
        calls
            .lock()
            .unwrap()
            .push((call.source, call.target, call.forced, call.spec.clone()));
        out["web_search_options"] = json!({"search_context_size":"low"});
        Ok(Some(relaykit::WebSearchEncoding::default()))
    }));
    let input=Request::from_value(Format::OpenAiResponses,json!({"model":"gpt-test","input":"hi","tools":[{"type":"web_search","external_web_access":false}],"tool_choice":{"type":"web_search"}})).unwrap();
    let result = relaykit::convert_request(&ctx, Format::OpenAi, &input).unwrap();
    assert!(seen.lock().unwrap()[0].2);
    assert_eq!(seen.lock().unwrap()[0].3["external_web_access"], false);
    assert_eq!(
        result.value.as_value()["web_search_options"]["search_context_size"],
        "low"
    );
    assert!(result.value.as_value().get("tool_choice").is_none());
    assert!(result.diagnostics.is_empty());
}

#[test]
fn gemini_schema_hook_is_called_once_only_for_gemini_tool_attachment() {
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = seen.clone();
    let mut ctx = Context {
        channel_meta_attached: true,
        upstream_model_name: "gemini-3-pro".into(),
        ..Default::default()
    };
    ctx.options.gemini.supports_json_schema = Some(Arc::new(move |model| {
        calls.lock().unwrap().push(model.to_owned());
        true
    }));
    let input = Request::from_value(Format::OpenAi, json!({
        "model":"test", "max_tokens":1024,
        "messages":[{"role":"user","content":"hi"}],
        "tools":[
            {"type":"function","function":{"name":"first","parameters":{"type":"object","const":{}}}},
            {"type":"function","function":{"name":"second","parameters":{"type":"object","const":{}}}}
        ]
    })).unwrap();
    for target in [Format::Claude, Format::OpenAiResponses] {
        relaykit::convert_request(&ctx, target, &input).unwrap();
    }
    assert!(seen.lock().unwrap().is_empty());
    let converted = relaykit::convert_request(&ctx, Format::Gemini, &input).unwrap();
    assert_eq!(*seen.lock().unwrap(), ["gemini-3-pro"]);
    assert_eq!(
        converted.value.as_value()["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        json!({"type":"object","const":{}})
    );
}

#[test]
fn request_context_restores_custom_tool_namespace_on_response() {
    let ctx = Context::default();
    let input=Request::from_value(Format::OpenAiResponses,json!({"model":"gpt-test","input":"hi","tools":[{"type":"namespace","name":"shell","tools":[{"type":"custom","name":"exec"}]}]})).unwrap();
    let request = relaykit::convert_request(&ctx, Format::OpenAi, &input).unwrap();
    assert_eq!(
        request.value.as_value()["tools"][0]["function"]["name"],
        "shell__exec"
    );
    let response=relaykit::Response::from_value(Format::OpenAi,json!({"id":"r","choices":[{"message":{"tool_calls":[{"id":"c","type":"function","function":{"name":"shell__exec","arguments":"{\"input\":\"echo hello\"}"}}]},"finish_reason":"tool_calls"}]})).unwrap();
    let response = relaykit::convert_response(&ctx, Format::OpenAiResponses, &response).unwrap();
    assert_eq!(
        response.value.as_value()["output"][0]["type"],
        "custom_tool_call"
    );
    assert_eq!(response.value.as_value()["output"][0]["namespace"], "shell");
    assert_eq!(response.value.as_value()["output"][0]["name"], "exec");
    assert_eq!(
        response.value.as_value()["output"][0]["input"],
        "echo hello"
    );
}

#[test]
fn host_native_search_tool_bypasses_chat_function_schema() {
    let mut ctx = relaykit::Context::default();
    ctx.options.web_search = Some(std::sync::Arc::new(|_, _| {
        Ok(Some(relaykit::WebSearchEncoding {
            native_tool: Some(
                json!({"type":"vendor_search","engine":"native","constraints":{"allowed":["example.com"]}}),
            ),
            ..Default::default()
        }))
    }));
    let input = relaykit::Request::from_value(
        Format::OpenAiResponses,
        json!({"model":"test","input":"hi","tools":[{"type":"web_search"}]}),
    )
    .unwrap();
    let result = relaykit::convert_request(&ctx, Format::OpenAi, &input).unwrap();
    assert_eq!(
        result.value.as_value()["tools"],
        json!([{"type":"vendor_search","engine":"native","constraints":{"allowed":["example.com"]}}])
    );
    assert!(
        result
            .value
            .as_value()
            .get("__relaykit_native_chat_tools")
            .is_none()
    );
}
