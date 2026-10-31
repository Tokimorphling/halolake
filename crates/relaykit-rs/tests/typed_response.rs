//! Typed response views and the existing Value API share the pinned contract.
use relaykit::{Context, Format, PreparedResponse, Response};
use serde_json::{Value, json};

fn normalize_time(value: &mut Value) {
    match value {
        Value::String(text) => {
            let bytes = text.as_bytes();
            let mut output = String::new();
            let mut start = 0;
            let mut index = 0;
            while index + 32 <= bytes.len() {
                if bytes[index..index + 32]
                    .iter()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
                {
                    output.push_str(&text[start..index]);
                    output.push_str("<uuid>");
                    index += 32;
                    start = index;
                } else {
                    index += 1;
                }
            }
            output.push_str(&text[start..]);
            *text = output;
        }
        Value::Array(values) => values.iter_mut().for_each(normalize_time),
        Value::Object(values) => {
            for (key, value) in values {
                if matches!(key.as_str(), "created" | "created_at")
                    && value.as_i64().is_some_and(|value| value >= 100_000_000)
                {
                    *value = json!(0);
                } else {
                    normalize_time(value);
                }
            }
        }
        _ => {}
    }
}

fn expected(entry: &Value) -> Value {
    let value = &entry["expected"];
    if value.get("error").is_some() {
        return json!({"error":value["error"]});
    }
    json!({"value":value["value"],"usage":value["usage"],
        "diagnostics":value["diagnostics"].as_array().cloned().unwrap_or_default()})
}

fn verify(entry: &Value) {
    let case = &entry["case"];
    let name = case["name"].as_str().unwrap_or("unnamed");
    let from: Format = case["from"].as_str().unwrap().parse().unwrap();
    let to: Format = case["to"].as_str().unwrap().parse().unwrap();
    let bytes = serde_json::to_vec(&case["input"]).unwrap();
    let ctx = Context {
        channel_meta_attached: !case["model"].as_str().unwrap_or("").is_empty(),
        upstream_model_name: case["model"].as_str().unwrap_or("").into(),
        ..Default::default()
    };
    let legacy = Response::from_value(from, case["input"].clone())
        .and_then(|input| relaykit::convert_response(&ctx, to, &input));
    let prepared = PreparedResponse::from_slice(from, &bytes);
    let typed = prepared
        .as_ref()
        .map_err(|error| error.to_string())
        .and_then(|input| {
            relaykit::convert_prepared_response(&ctx, to, input).map_err(|error| error.to_string())
        });
    let mut wanted = expected(entry);
    if wanted.get("error").is_some() {
        assert!(legacy.is_err(), "{name}: Value API accepted invalid input");
        assert!(
            typed.is_err(),
            "{name}: prepared API accepted invalid input"
        );
        return;
    }
    let legacy = legacy.unwrap_or_else(|error| panic!("{name}: Value API failed: {error}"));
    let typed = typed.unwrap_or_else(|error| panic!("{name}: prepared API failed: {error}"));
    assert_eq!(
        typed.converter, legacy.converter,
        "{name}: converter metadata differs"
    );
    assert_eq!(
        typed.quality, legacy.quality,
        "{name}: quality metadata differs"
    );
    assert_eq!(typed.steps, legacy.steps, "{name}: route metadata differs");
    let mut actual = json!({"value":serde_json::to_value(&typed.value).unwrap(),
        "usage":serde_json::to_value(&typed.usage).unwrap(),"diagnostics":typed.diagnostics});
    let mut previous = json!({"value":legacy.value.into_value(),"usage":legacy.usage,"diagnostics":legacy.diagnostics});
    normalize_time(&mut wanted);
    normalize_time(&mut actual);
    normalize_time(&mut previous);
    assert_eq!(
        actual, wanted,
        "{name}: typed output/usage/diagnostics differ from pinned Go"
    );
    assert_eq!(
        previous, wanted,
        "{name}: Value output/usage/diagnostics differ from pinned Go"
    );

    let mut written = Vec::new();
    let metadata =
        relaykit::convert_response_to_writer(&ctx, to, prepared.as_ref().unwrap(), &mut written)
            .unwrap_or_else(|error| panic!("{name}: writer API failed: {error}"));
    let mut writer_value: Value = serde_json::from_slice(&written).unwrap();
    normalize_time(&mut writer_value);
    assert_eq!(
        writer_value, wanted["value"],
        "{name}: writer payload differs"
    );
    let mut writer_usage = serde_json::to_value(&metadata.usage).unwrap();
    normalize_time(&mut writer_usage);
    assert_eq!(
        writer_usage, wanted["usage"],
        "{name}: writer usage differs"
    );
    assert_eq!(
        serde_json::to_value(metadata.diagnostics).unwrap(),
        wanted["diagnostics"],
        "{name}: writer diagnostics differ"
    );
    assert_eq!(
        typed.value.is_typed(),
        prepared.as_ref().unwrap().uses_typed_path(to)
    );
}

#[test]
fn all_response_routes_match_existing_oracle_cases() {
    for fixture in [
        include_str!("fixtures/differential.json"),
        include_str!("fixtures/response_contract.json"),
        include_str!("fixtures/upgrade_parity.json"),
    ] {
        let corpus: Value = serde_json::from_str(fixture).unwrap();
        for entry in corpus.as_array().unwrap() {
            let case = &entry["case"];
            if case["kind"] == "response" {
                verify(entry);
            }
        }
    }
}

#[test]
fn typed_claude_chat_matches_pinned_go_edges_and_writer() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/typed_response_edges.json")).unwrap();
    for entry in corpus.as_array().unwrap() {
        verify(entry);
    }
}

#[test]
fn typed_claude_chat_matches_golden_payload() {
    let inputs: Value = serde_json::from_str(include_str!("fixtures/inputs.json")).unwrap();
    let input = &inputs["response"]["claude"];
    let bytes = serde_json::to_vec(input).unwrap();
    let prepared = PreparedResponse::from_slice(Format::Claude, &bytes).unwrap();
    let ctx = Context {
        channel_meta_attached: true,
        upstream_model_name: "upstream-model".into(),
        ..Default::default()
    };
    let result = relaykit::convert_prepared_response(&ctx, Format::OpenAi, &prepared).unwrap();
    let mut actual = result.value.to_value().unwrap();
    let mut expected: Value = serde_json::from_str(include_str!(
        "fixtures/golden/response/claude_to_openai.golden.json"
    ))
    .unwrap();
    normalize_time(&mut actual);
    normalize_time(&mut expected);
    assert_eq!(actual, expected);
}

#[test]
fn prepared_source_is_reused_without_memoizing_context_or_changing_input() {
    let bytes = br#" { "model": "m", "content": [{"type":"text","text":"reuse"}], "id": "msg_placeholder" } "#;
    let prepared = PreparedResponse::from_slice(Format::Claude, bytes).unwrap();
    let original = prepared.original_json().to_vec();
    assert!(prepared.uses_typed_path(Format::OpenAi));
    for created in [123, 456] {
        let ctx = Context {
            created,
            ..Default::default()
        };
        let result = relaykit::convert_prepared_response(&ctx, Format::OpenAi, &prepared).unwrap();
        assert!(result.value.is_typed());
        assert_eq!(result.value.to_value().unwrap()["created"], created);
        assert_eq!(prepared.original_json(), original);
    }
    assert_eq!(original.as_slice(), bytes);
}

#[test]
fn writer_error_is_returned_and_does_not_poison_prepared_source() {
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("intentional writer failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let prepared = PreparedResponse::from_slice(
        Format::Claude,
        br#"{"content":[{"type":"text","text":"writer"}]}"#,
    )
    .unwrap();
    let ctx = Context::default();
    let error =
        relaykit::convert_response_to_writer(&ctx, Format::OpenAi, &prepared, FailingWriter)
            .unwrap_err();
    assert!(error.to_string().contains("intentional writer failure"));
    let mut bytes = Vec::new();
    relaykit::convert_response_to_writer(&ctx, Format::OpenAi, &prepared, &mut bytes).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["choices"][0]["message"]["content"],
        "writer"
    );
}

#[test]
fn noncanonical_fields_and_other_routes_keep_value_compatibility() {
    let examples = [
        (
            Format::Claude,
            Format::OpenAi,
            json!({"iD":"msg_placeholder","cOnTeNt":[{"tYpE":"text","tExT":"casefold"}]}),
        ),
        (
            Format::OpenAiResponses,
            Format::OpenAi,
            json!({"ID":"resp_placeholder","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fallback"}]}]}),
        ),
        (
            Format::Claude,
            Format::Gemini,
            json!({"ID":"msg_placeholder","content":[{"type":"text","text":"fallback"}]}),
        ),
    ];
    for (from, to, source) in examples {
        let bytes = serde_json::to_vec(&source).unwrap();
        let prepared = PreparedResponse::from_slice(from, &bytes).unwrap();
        assert!(!prepared.uses_typed_path(to));
        let ctx = Context::default();
        let typed = relaykit::convert_prepared_response(&ctx, to, &prepared).unwrap();
        assert!(!typed.value.is_typed());
        let previous =
            relaykit::convert_response(&ctx, to, &Response::from_value(from, source).unwrap())
                .unwrap();
        let mut actual = json!({"value":typed.value.to_value().unwrap(),"usage":serde_json::to_value(typed.usage).unwrap(),"diagnostics":typed.diagnostics});
        let mut expected = json!({"value":previous.value.into_value(),"usage":previous.usage,"diagnostics":previous.diagnostics});
        normalize_time(&mut actual);
        normalize_time(&mut expected);
        assert_eq!(actual, expected);
        assert_eq!(prepared.original_json(), bytes.as_slice());
    }
}

fn compare_value_route(from: Format, to: Format, source: Value, context: &Context) -> bool {
    let bytes = serde_json::to_vec(&source).unwrap();
    let prepared = PreparedResponse::from_slice(from, &bytes).unwrap();
    let actual = relaykit::convert_prepared_response(context, to, &prepared).unwrap();
    let expected =
        relaykit::convert_response(context, to, &Response::from_value(from, source).unwrap())
            .unwrap();
    let mut a = json!({"value":actual.value.to_value().unwrap(),"usage":serde_json::to_value(&actual.usage).unwrap(),"diagnostics":actual.diagnostics});
    let mut e = json!({"value":expected.value.into_value(),"usage":expected.usage,"diagnostics":expected.diagnostics});
    normalize_time(&mut a);
    normalize_time(&mut e);
    assert_eq!(a, e, "{from} to {to}");
    assert_eq!(actual.converter, expected.converter);
    assert_eq!(actual.quality, expected.quality);
    assert_eq!(actual.steps, expected.steps);
    assert_eq!(prepared.original_json(), bytes);
    actual.value.is_typed()
}

#[test]
fn all_twelve_cross_format_routes_use_direct_views() {
    let context = Context {
        channel_meta_attached: true,
        upstream_model_name: "upstream-model".into(),
        response_id: "chatcmpl_placeholder".into(),
        created: 42,
        ..Default::default()
    };
    let inputs = [
        (
            Format::OpenAi,
            json!({"id":"chat_placeholder","model":"m","created":42,"choices":[{"index":3,"finish_reason":"tool_calls","message":{"role":"assistant","content":"answer","reasoning_content":"think","tool_calls":[{"id":"call_placeholder","type":"function","function":{"name":"lookup","arguments":"{\"query\":\"x\"}"}}]}}],"usage":{"prompt_tokens":11,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3,"cached_creation_tokens":2}}}),
        ),
        (
            Format::OpenAiResponses,
            json!({"id":"response_placeholder","model":"m","created_at":42,"status":"completed","output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"think"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},{"type":"function_call","call_id":"call_placeholder","name":"lookup","arguments":"{\"query\":\"x\"}"}],"usage":{"input_tokens":11,"output_tokens":4,"input_tokens_details":{"cached_tokens":3}}}),
        ),
        (
            Format::Claude,
            json!({"id":"claude_placeholder","model":"m","stop_reason":"tool_use","content":[{"type":"thinking","thinking":"think"},{"type":"text","text":"answer"},{"type":"tool_use","id":"call_placeholder","name":"lookup","input":{"query":"x"}}],"usage":{"input_tokens":11,"output_tokens":4,"cache_read_input_tokens":3}}),
        ),
        (
            Format::Gemini,
            json!({"candidates":[{"index":3,"finishReason":"STOP","content":{"parts":[{"thought":true,"text":"think"},{"text":"answer"},{"functionCall":{"id":"call_placeholder","name":"lookup","args":{"query":"x"}}}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":4,"cachedContentTokenCount":3}}),
        ),
    ];
    for (from, source) in inputs {
        for to in [
            Format::OpenAi,
            Format::OpenAiResponses,
            Format::Claude,
            Format::Gemini,
        ] {
            if from != to {
                assert!(
                    compare_value_route(from, to, source.clone(), &context),
                    "{from} to {to} unexpectedly used compatibility"
                );
            }
        }
    }
}

#[test]
fn sparse_null_usage_and_native_accounting_boundaries_match_value_contract() {
    let context = Context {
        channel_meta_attached: true,
        upstream_model_name: "m".into(),
        response_id: "chat_placeholder".into(),
        created: 42,
        estimate_prompt_tokens: 7,
        ..Default::default()
    };
    for from in [Format::OpenAi, Format::OpenAiResponses, Format::Gemini] {
        let list = match from {
            Format::OpenAi => "choices",
            Format::OpenAiResponses => "output",
            _ => "candidates",
        };
        let usage = if from == Format::Gemini {
            "usageMetadata"
        } else {
            "usage"
        };
        for source in [
            json!({}),
            json!({list:null, usage:null}),
            json!({list:[null], usage:{}}),
            json!({list:[], usage:{}}),
        ] {
            for to in [
                Format::OpenAi,
                Format::OpenAiResponses,
                Format::Claude,
                Format::Gemini,
            ] {
                compare_value_route(from, to, source.clone(), &context);
            }
        }
    }
    let native = json!({"prompt_tokens":12,"completion_tokens":5,"total_tokens":0,"prompt_tokens_details":{"cached_tokens":3,"cached_tokens_details":{"text_tokens":0},"cache_write_tokens":2},"input_tokens_details":{"text_tokens":0},"output_tokens_details":{"reasoning_tokens":9},"cost":0,"billing_usage":{"source":"gemini_chat","semantic":"gemini","gemini_usage_metadata":{"promptTokenCount":7,"candidatesTokenCount":2,"thoughtsTokenCount":3,"promptTokensDetails":null}}});
    for to in [Format::Claude, Format::Gemini, Format::OpenAiResponses] {
        assert!(compare_value_route(
            Format::OpenAi,
            to,
            json!({"id":"chat_placeholder","created":42,"choices":[{"message":{"content":"answer"},"finish_reason":"stop"}],"usage":native}),
            &context
        ));
    }
}

#[test]
fn typed_responses_restores_custom_tools_and_namespaces_from_each_context() {
    let bytes=br#"{"id":"chat_placeholder","created":42,"choices":[{"message":{"content":"answer","tool_calls":[{"id":"call_placeholder","type":"function","function":{"name":"space__lookup","arguments":"{\"input\":\"raw text\"}"}}]}}]}"#;
    let prepared = PreparedResponse::from_slice(Format::OpenAi, bytes).unwrap();
    for custom in [false, true] {
        let context = Context::default();
        let mut state = relaykit::ResponsesToolState::default();
        state
            .namespaces
            .insert("space__lookup".into(), "space".into());
        if custom {
            state.custom_tool_names.insert("space__lookup".into());
        }
        context.set_responses_tool_state(state);
        let result =
            relaykit::convert_prepared_response(&context, Format::OpenAiResponses, &prepared)
                .unwrap();
        let expected = relaykit::convert_response(
            &context,
            Format::OpenAiResponses,
            &Response::from_value(Format::OpenAi, serde_json::from_slice(bytes).unwrap()).unwrap(),
        )
        .unwrap();
        assert!(result.value.is_typed());
        assert_eq!(
            result.value.to_value().unwrap(),
            expected.value.into_value()
        );
        assert_eq!(prepared.original_json(), bytes);
    }
}

#[test]
fn serde_guard_preserves_duplicate_case_and_alias_compatibility() {
    for (from, bytes) in [
        (Format::OpenAi, br#"{"id":"first","id":"last","choices":[]}"#.as_slice()),
        (Format::OpenAiResponses, br#"{"output":[{"type":"message","content":[{"tExT":"case"}]}]}"#.as_slice()),
        (Format::Gemini, br#"{"candidates":[{"content":{"parts":[{"inline_data":{"mime_type":"image/png","data":"AAAA"}}]}}]}"#.as_slice()),
    ] {
        let prepared=PreparedResponse::from_slice(from,bytes).unwrap();
        assert!(!prepared.uses_typed_path(Format::Claude));
        for to in [Format::OpenAi,Format::OpenAiResponses,Format::Claude,Format::Gemini] {compare_value_route(from,to,serde_json::from_slice(bytes).unwrap(),&Context::default());}
    }
}
