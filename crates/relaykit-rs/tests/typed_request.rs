//! Prepared request reuse and direct borrowed output preserve the Value contract.
use relaykit::{Context, Format, PreparedRequest, Request};
use serde_json::{Value, json};
use std::sync::Arc;

fn context(case: &Value) -> Context {
    let model = case["model"].as_str().unwrap_or("");
    let mut ctx = Context {
        channel_meta_attached: !model.is_empty(),
        upstream_model_name: model.into(),
        ..Default::default()
    };
    ctx.media.get_base64_data = Some(Arc::new(|_, _| Ok(("aGVsbG8=".into(), "image/png".into()))));
    ctx.media.decode_base64_file_data =
        Some(Arc::new(|_| Ok(("aGVsbG8=".into(), "image/png".into()))));
    if case["golden"] == true {
        ctx.options.gemini.function_call_thought_signature_enabled = true;
        ctx.options.gemini.thinking_adapter_budget_tokens_percentage = 0.6;
        ctx.options.gemini.safety_setting = Some(Arc::new(|_| "OFF".into()));
    }
    ctx
}
fn verify(case: &Value) {
    if case["kind"] != "request" {
        return;
    }
    let name = case["name"].as_str().unwrap_or("unnamed");
    let from: Format = case["from"].as_str().unwrap().parse().unwrap();
    let to: Format = case["to"].as_str().unwrap().parse().unwrap();
    let bytes = serde_json::to_vec(&case["input"]).unwrap();
    let old_ctx = context(case);
    let new_ctx = context(case);
    let legacy = Request::from_value(from, case["input"].clone())
        .and_then(|input| relaykit::convert_request(&old_ctx, to, &input));
    let prepared = PreparedRequest::from_slice(from, &bytes);
    let direct = prepared
        .as_ref()
        .map_err(|error| error.to_string())
        .and_then(|input| {
            relaykit::convert_prepared_request(&new_ctx, to, input)
                .map_err(|error| error.to_string())
        });
    match (legacy, direct) {
        (Err(_), Err(_)) => {}
        (Ok(old), Ok(new)) => {
            let actual = json!({"value":new.value.to_value().unwrap(), "diagnostics":new.diagnostics, "usage":new.usage,
                "converter":new.converter,"quality":new.quality,"steps":new.steps});
            let expected = json!({"value":old.value.into_value(),"diagnostics":old.diagnostics,"usage":old.usage,
                "converter":old.converter,"quality":old.quality,"steps":old.steps});
            assert_eq!(
                actual, expected,
                "{name}: prepared payload or metadata differ"
            );
            assert_eq!(
                new.value.is_typed(),
                prepared.as_ref().unwrap().uses_typed_path(to)
            );
            assert_eq!(
                *old_ctx.conversion_chain.lock().unwrap(),
                *new_ctx.conversion_chain.lock().unwrap()
            );
            let mut written = Vec::new();
            let metadata = relaykit::convert_request_to_writer(
                &context(case),
                to,
                prepared.as_ref().unwrap(),
                &mut written,
            )
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&written).unwrap(),
                expected["value"],
                "{name}: writer payload differs"
            );
            assert_eq!(
                serde_json::to_value(metadata.diagnostics).unwrap(),
                expected["diagnostics"]
            );
        }
        (old, new) => panic!(
            "{name}: success/error mismatch: legacy={}, prepared={}",
            old.is_ok(),
            new.is_ok()
        ),
    }
}
#[test]
fn prepared_requests_match_all_existing_request_fixtures() {
    for fixture in [
        include_str!("fixtures/differential.json"),
        include_str!("fixtures/request_parity.json"),
        include_str!("fixtures/tool_contract.json"),
        include_str!("fixtures/upgrade_parity.json"),
        include_str!("fixtures/v035_upgrade_parity.json"),
        include_str!("fixtures/reasoning_parity.json"),
    ] {
        let values: Value = serde_json::from_str(fixture).unwrap();
        for entry in values.as_array().unwrap() {
            verify(&entry["case"]);
        }
    }
}
#[test]
fn source_bytes_and_context_dependent_headers_are_reused() {
    let bytes = br#" {"model":"claude-sonnet-4-5","max_tokens":100,"messages":[{"role":"user","content":"hello"}]} "#;
    let prepared = PreparedRequest::from_slice(Format::Claude, bytes).unwrap();
    assert_eq!(prepared.original_json(), bytes);
    assert!(prepared.uses_typed_path(Format::OpenAi));
    let a = Context::default();
    let b = Context {
        origin_model_name: "claude-thinking".into(),
        ..Default::default()
    };
    let first = relaykit::convert_prepared_request(&a, Format::OpenAi, &prepared)
        .unwrap()
        .value
        .to_value()
        .unwrap();
    let second = relaykit::convert_prepared_request(&b, Format::OpenAi, &prepared)
        .unwrap()
        .value
        .to_value()
        .unwrap();
    assert_ne!(first["model"], second["model"]);
    assert_eq!(prepared.original_json(), bytes);
    assert_eq!(first["messages"], second["messages"]);
}
#[test]
fn complex_media_falls_back_and_writer_errors_propagate() {
    let bytes = br#"{"model":"test","max_tokens":10,"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://example.test/image"}}]}]}"#;
    let prepared = PreparedRequest::from_slice(Format::Claude, bytes).unwrap();
    assert!(!prepared.uses_typed_path(Format::OpenAi));
    struct Failing;
    impl std::io::Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("writer failed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(
        relaykit::convert_request_to_writer(
            &Context::default(),
            Format::OpenAi,
            &prepared,
            Failing
        )
        .is_err()
    );
    assert!(
        relaykit::convert_prepared_request(&Context::default(), Format::OpenAi, &prepared).is_ok()
    );
}

#[test]
fn prepared_tool_attachment_preserves_callbacks_state_and_strict_policy() {
    use std::sync::Mutex;
    let bytes = br#"{"model":"test","max_output_tokens":50,"input":[{"role":"user","content":"hello"}],"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","const":{}}}]}"#;
    let prepared = PreparedRequest::from_slice(Format::OpenAiResponses, bytes).unwrap();
    assert!(prepared.uses_typed_path(Format::Gemini));
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
    let result = relaykit::convert_prepared_request(&ctx, Format::Gemini, &prepared).unwrap();
    assert_eq!(*seen.lock().unwrap(), ["gemini-3-pro"]);
    assert_eq!(
        result.value.to_value().unwrap()["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        json!({"type":"object","const":{}})
    );

    let custom = br#"{"model":"test","input":"hello","tools":[{"type":"namespace","name":"shell","tools":[{"type":"custom","name":"execute"}]}]}"#;
    let prepared = PreparedRequest::from_slice(Format::OpenAiResponses, custom).unwrap();
    relaykit::convert_prepared_request(&ctx, Format::OpenAi, &prepared).unwrap();
    assert!(
        ctx.responses_tool_state()
            .custom_tool_names
            .contains("shell__execute")
    );
    assert_eq!(
        ctx.responses_tool_state()
            .namespaces
            .get("shell__execute")
            .unwrap(),
        "shell"
    );
    let empty = br#"{"model":"test","input":"hello"}"#;
    let prepared = PreparedRequest::from_slice(Format::OpenAiResponses, empty).unwrap();
    relaykit::convert_prepared_request(&ctx, Format::OpenAi, &prepared).unwrap();
    assert!(ctx.responses_tool_state().custom_tool_names.is_empty());
    assert!(ctx.responses_tool_state().namespaces.is_empty());

    // A tool diagnostic is rejected in Strict policy; body/reasoning warnings
    // retain the upstream tool-policy scope.
    let lossy = br#"{"model":"test","input":"hello","tools":[{"type":"file_search"}]}"#;
    let prepared = PreparedRequest::from_slice(Format::OpenAiResponses, lossy).unwrap();
    ctx.options.tool_loss_policy = relaykit::LossPolicy::Strict;
    assert!(relaykit::convert_prepared_request(&ctx, Format::OpenAi, &prepared).is_err());
    let trailing = br#"{"model":"test","max_output_tokens":10,"input":[{"role":"user","content":"hello"},{"role":"assistant","content":"answer"}]}"#;
    let prepared = PreparedRequest::from_slice(Format::OpenAiResponses, trailing).unwrap();
    let result = relaykit::convert_prepared_request(&ctx, Format::Claude, &prepared).unwrap();
    assert_eq!(result.diagnostics[0].code, "trailing_assistant_omitted");
}

#[test]
fn mutable_web_search_encoder_receives_complete_compatibility_payload() {
    let bytes = br#"{"model":"test","input":[{"role":"user","content":"original"}],"tools":[{"type":"web_search"}]}"#;
    let prepared = PreparedRequest::from_slice(Format::OpenAiResponses, bytes).unwrap();
    assert!(!prepared.uses_typed_path(Format::OpenAi));
    let mut ctx = Context::default();
    ctx.options.web_search = Some(Arc::new(|_, out| {
        assert_eq!(out["messages"][0]["content"], "original");
        out["messages"][0]["content"] = json!("replaced by encoder");
        Ok(Some(relaykit::WebSearchEncoding::default()))
    }));
    let result = relaykit::convert_prepared_request(&ctx, Format::OpenAi, &prepared).unwrap();
    assert_eq!(
        result.value.to_value().unwrap()["messages"][0]["content"],
        "replaced by encoder"
    );
}

#[test]
fn dynamic_tool_islands_and_empty_text_blocks_keep_compatibility_semantics() {
    for output in [
        json!([{"type":"text","text":20}]),
        json!([{"type":"text","text":false}]),
        json!([{"type":"text","text":""}]),
        json!({"large":100000000000000016i64,"negative_zero":-0.0}),
    ] {
        for target in ["openai", "claude", "gemini"] {
            verify(
                &json!({"name":"tool-output-island","kind":"request","from":"openai_responses","to":target,
                "model":"upstream-model","input":{"model":"test","max_output_tokens":20,"input":[
                    {"type":"function_call","name":"lookup","call_id":"call_test","arguments":"{\"x\":1}"},
                    {"type":"function_call_output","call_id":"call_test","output":output}]}}),
            );
        }
    }
    for content in [
        json!([{"type":"text","text":""}]),
        json!([{"type":"text","cache_control":{"type":"ephemeral"}}]),
        json!([{"type":"text","text":"hello"},{"type":"text","text":""}]),
    ] {
        for target in ["openai", "openai_responses"] {
            verify(
                &json!({"name":"empty-text-block","kind":"request","from":"claude","to":target,
                "input":{"model":"test","max_tokens":20,"messages":[{"role":"user","content":content}]}}),
            );
        }
    }
}

#[test]
fn text_null_missing_and_nonstring_fields_match_pinned_go() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/typed_request_edges.json")).unwrap();
    for entry in corpus.as_array().unwrap() {
        let case = &entry["case"];
        verify(case);
        let target: Format = case["to"].as_str().unwrap().parse().unwrap();
        let bytes = serde_json::to_vec(&case["input"]).unwrap();
        let prepared = PreparedRequest::from_slice(Format::OpenAiResponses, &bytes).unwrap();
        let result = relaykit::convert_prepared_request(&context(case), target, &prepared).unwrap();
        assert_eq!(
            result.value.to_value().unwrap(),
            entry["expected"]["value"],
            "{}: pinned Go text coercion differs",
            case["name"]
        );
        assert!(result.diagnostics.is_empty());
    }
}
