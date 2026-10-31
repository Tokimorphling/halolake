use relaykit::{
    Context, Format, Request, Response, ResponseStreamState, StreamOptions, convert_request,
    convert_response, responses_tool_state,
};
use serde_json::{Value, json};

fn context() -> Context {
    let mut ctx = Context {
        origin_model_name: "requested-model".into(),
        upstream_model_name: "claude-sonnet-4-5".into(),
        channel_meta_attached: true,
        created: 1_700_000_000,
        ..Context::default()
    };
    ctx.options.preserve_message_structure = true;
    ctx
}

fn state() -> ResponseStreamState {
    ResponseStreamState::new(
        Format::Claude,
        Format::OpenAiResponses,
        StreamOptions {
            id: "resp_fixed".into(),
            model: "requested-model".into(),
            created_at: 1_700_000_000,
            emit_sequence_number: true,
            preserve_message_structure: true,
            ..StreamOptions::default()
        },
    )
    .unwrap()
}

fn start() -> Value {
    json!({
        "type":"message_start",
        "message":{
            "id":"msg_fixed","type":"message","role":"assistant","model":"claude",
            "content":[],"usage":{"input_tokens":9,"cache_read_input_tokens":3,
                "cache_creation_input_tokens":2,"output_tokens":0}
        }
    })
}

fn end() -> [Value; 2] {
    [
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]
}

fn response(events: &[Value]) -> &Value {
    &events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap()["response"]
}

#[test]
fn native_claude_stream_preserves_immediate_lifecycle_and_interleaved_order() {
    let ctx = context();
    let mut state = state();
    let created = state.convert_chunk(&ctx, &start()).unwrap();
    assert_eq!(created[0]["type"], "response.created");
    assert_eq!(created[0]["response"]["model"], "requested-model");

    let thinking = state
        .convert_chunk(
            &ctx,
            &json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        )
        .unwrap();
    assert_eq!(thinking[0]["type"], "response.output_item.added");
    assert_eq!(thinking[0]["item"]["type"], "reasoning");
    assert_eq!(thinking[1]["type"], "response.reasoning_summary_part.added");
    let text = state
        .convert_chunk(
            &ctx,
            &json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        )
        .unwrap();
    assert_eq!(text[0]["output_index"], 1);
    assert!(
        text.iter()
            .all(|event| event["type"] != "response.output_item.done")
    );

    let mut events = created;
    events.extend(thinking);
    events.extend(text);
    for input in [
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}),
        json!({"type":"content_block_stop","index":1}),
    ] {
        events.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    let closed = state
        .convert_chunk(&ctx, &json!({"type":"content_block_stop","index":0}))
        .unwrap();
    let kinds: Vec<_> = closed
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert_eq!(
        kinds,
        [
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.output_item.done",
        ]
    );
    events.extend(closed);
    for input in end() {
        events.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    assert!(state.finalize(&ctx).unwrap().is_empty());
    let output = response(&events);
    assert_eq!(output["output"][0]["type"], "reasoning");
    assert_eq!(output["output"][0]["summary"][0]["text"], "reason");
    assert_eq!(output["output"][0]["encrypted_content"], "signed");
    assert_eq!(output["output"][1]["content"][0]["text"], "answer");
    assert_eq!(output["usage"]["input_tokens"], 14);
    assert_eq!(output["usage"]["output_tokens"], 7);
    assert_eq!(output["usage"]["total_tokens"], 21);
    assert_eq!(state.usage().unwrap()["prompt_tokens"], 14);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], index);
    }
}

#[test]
fn native_namespace_tool_identity_is_stable_at_every_lifecycle_stage() {
    let ctx = context();
    let tools = responses_tool_state(&json!({
        "tools":[{"type":"namespace","name":"math","tools":[
            {"type":"function","name":"add","parameters":{"type":"object"}}
        ]}]
    }))
    .unwrap();
    let flat = tools.flatten_declared("math", "add").unwrap().to_string();
    ctx.set_responses_tool_state(tools);
    let mut state = state();
    let mut events = state.convert_chunk(&ctx, &start()).unwrap();
    for input in [
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":flat,"input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ] {
        events.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    for event in &events {
        if matches!(
            event["type"].as_str(),
            Some("response.output_item.added" | "response.output_item.done")
        ) {
            assert_eq!(event["item"]["name"], "add");
            assert_eq!(event["item"]["namespace"], "math");
        } else if matches!(
            event["type"].as_str(),
            Some(
                "response.function_call_arguments.delta" | "response.function_call_arguments.done"
            )
        ) {
            assert_eq!(event["name"], "add");
            assert_eq!(event["namespace"], "math");
            assert_eq!(event["call_id"], "call_1");
            if event["type"] == "response.function_call_arguments.done" {
                assert_eq!(event["arguments"], "{\"x\":1}");
            }
        }
    }
    let arguments: String = events
        .iter()
        .filter(|event| event["type"] == "response.function_call_arguments.delta")
        .filter_map(|event| event["delta"].as_str())
        .collect();
    assert_eq!(arguments, "{\"x\":1}");
    assert_eq!(response(&events)["output"][0]["name"], "add");
    assert_eq!(response(&events)["output"][0]["namespace"], "math");
}

#[test]
fn native_custom_and_tool_search_calls_restore_their_distinct_protocols() {
    let ctx = context();
    ctx.set_responses_tool_state(
        responses_tool_state(&json!({
            "tools":[{"type":"custom","name":"exec"},{"type":"tool_search"}]
        }))
        .unwrap(),
    );
    let mut state = state();
    let mut events = state.convert_chunk(&ctx, &start()).unwrap();
    for input in [
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_exec","name":"exec","input":{"input":"ls"}}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_search","name":"tool_search","input":{"query":"files"}}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ] {
        events.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    assert!(events.iter().all(|event| {
        !matches!(
            event["type"].as_str(),
            Some(
                "response.function_call_arguments.delta" | "response.function_call_arguments.done"
            )
        )
    }));
    let input = events
        .iter()
        .find(|event| event["type"] == "response.custom_tool_call_input.done")
        .unwrap();
    assert_eq!(input["name"], "exec");
    assert_eq!(input["input"], "ls");
    let output = response(&events);
    assert_eq!(output["output"][0]["type"], "custom_tool_call");
    assert_eq!(output["output"][0]["input"], "ls");
    assert_eq!(output["output"][1]["type"], "tool_search_call");
    assert_eq!(output["output"][1]["execution"], "client");
    assert_eq!(output["output"][1]["arguments"]["query"], "files");
}

#[test]
fn native_buffered_redacted_reasoning_replays_with_the_original_block_type() {
    let ctx = context();
    let source = Response::from_value(
        Format::Claude,
        json!({
            "id":"msg_fixed","type":"message","role":"assistant","model":"claude",
            "content":[
                {"type":"thinking","thinking":"reason","signature":"signed"},
                {"type":"redacted_thinking","data":"OPAQUE"},
                {"type":"text","text":"answer"}
            ],
            "stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3}
        }),
    )
    .unwrap();
    let result = convert_response(&ctx, Format::OpenAiResponses, &source).unwrap();
    let output = result.value.into_value();
    assert_eq!(output["output"][0]["encrypted_content"], "signed");
    assert_eq!(output["output"][1]["encrypted_content"], "OPAQUE");
    assert_eq!(
        output["output"][1]["claude_block_type"],
        "redacted_thinking"
    );
    let mut history = vec![json!({"role":"user","content":"question"})];
    history.extend(output["output"].as_array().unwrap().iter().cloned());
    history.push(json!({"role":"user","content":"continue"}));
    let request = Request::from_value(
        Format::OpenAiResponses,
        json!({"model":"claude-sonnet-4-5","max_output_tokens":4096,"input":history}),
    )
    .unwrap();
    let restored = convert_request(&ctx, Format::Claude, &request)
        .unwrap()
        .value
        .into_value();
    let assistant = restored["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    let content = assistant["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "thinking", "{restored}");
    assert_eq!(content[0]["thinking"], "reason");
    assert_eq!(content[0]["signature"], "signed");
    assert_eq!(content[1]["type"], "redacted_thinking");
    assert_eq!(content[1]["data"], "OPAQUE");
}

#[test]
fn native_errors_do_not_complete_an_open_tool_call() {
    let ctx = context();
    let mut state = state();
    state.convert_chunk(&ctx, &start()).unwrap();
    state
        .convert_chunk(
            &ctx,
            &json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"exec","input":{}}}),
        )
        .unwrap();
    state
        .convert_chunk(
            &ctx,
            &json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}),
        )
        .unwrap();
    let events = state
        .convert_chunk(
            &ctx,
            &json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        )
        .unwrap();
    assert!(events.iter().any(|event| event["type"] == "error"));
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.failed")
    );
    assert!(!events.iter().any(|event| {
        matches!(
            event["type"].as_str(),
            Some(
                "response.completed"
                    | "response.output_item.done"
                    | "response.function_call_arguments.done"
            )
        )
    }));
    assert!(state.is_failed());
    assert!(state.finalize(&ctx).unwrap().is_empty());
}

#[test]
fn native_web_search_and_citations_do_not_leak_client_tool_calls() {
    let ctx = context();
    let mut state = state();
    let mut events = state.convert_chunk(&ctx, &start()).unwrap();
    for input in [
        json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtool_1","name":"web_search","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"docs\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtool_1","content":[]}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":"\u{03bb} Read docs."}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","url":"https://example.com/docs","title":"Docs","cited_text":"Read","encrypted_index":"opaque"}}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ] {
        let converted = state.convert_chunk(&ctx, &input).unwrap();
        if input["type"] == "content_block_stop" && input["index"] == 0 {
            assert!(
                converted.is_empty(),
                "search input stop is not execution completion"
            );
        }
        if input["content_block"]["type"] == "web_search_tool_result" {
            let kinds: Vec<_> = converted
                .iter()
                .filter_map(|event| event["type"].as_str())
                .collect();
            assert_eq!(
                kinds,
                [
                    "response.web_search_call.completed",
                    "response.output_item.done"
                ]
            );
        }
        events.extend(converted);
    }
    let output = response(&events);
    assert_eq!(output["output"][0]["type"], "web_search_call");
    assert_eq!(output["output"][0]["status"], "completed");
    assert_eq!(output["output"][0]["action"]["query"], "docs");
    assert!(
        output["output"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| { item["type"] != "function_call" })
    );
    let citation = &output["output"][1]["content"][0]["annotations"][0];
    assert_eq!(citation["type"], "url_citation");
    assert_eq!(citation["url"], "https://example.com/docs");
    assert_eq!(citation["start_index"], 2);
    assert_eq!(citation["end_index"], 6);
    assert!(
        events
            .iter()
            .any(|event| { event["type"] == "response.output_text.annotation.added" })
    );
}

fn without_ids(mut value: Value) -> Value {
    fn visit(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.remove("id");
                fields.remove("created_at");
                for child in fields.values_mut() {
                    visit(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    visit(child);
                }
            }
            _ => {}
        }
    }
    visit(&mut value);
    value
}

#[test]
fn native_buffered_and_streaming_outputs_share_the_same_block_conversion() {
    let ctx = context();
    let content = [
        json!({"type":"thinking","thinking":"reason","signature":"signed"}),
        json!({"type":"text","text":"one"}),
        json!({"type":"tool_use","id":"call_1","name":"exec","input":{"x":1}}),
        json!({"type":"text","text":"two"}),
    ];
    let usage = json!({
        "input_tokens":9,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,
        "output_tokens":7
    });
    let source = Response::from_value(
        Format::Claude,
        json!({
            "id":"msg_fixed","type":"message","role":"assistant","model":"claude",
            "content":content,"usage":usage,"stop_reason":"end_turn"
        }),
    )
    .unwrap();
    let buffered = convert_response(&ctx, Format::OpenAiResponses, &source)
        .unwrap()
        .value
        .into_value();
    let mut state = state();
    let mut events = state.convert_chunk(&ctx, &start()).unwrap();
    for (index, block) in content.into_iter().enumerate() {
        events.extend(
            state
                .convert_chunk(
                    &ctx,
                    &json!({
                        "type":"content_block_start","index":index,"content_block":block,
                    }),
                )
                .unwrap(),
        );
        events.extend(
            state
                .convert_chunk(
                    &ctx,
                    &json!({
                        "type":"content_block_stop","index":index,
                    }),
                )
                .unwrap(),
        );
    }
    for input in end() {
        events.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    assert_eq!(
        without_ids(buffered),
        without_ids(response(&events).clone())
    );
}

#[test]
fn native_hosted_search_failure_is_not_reported_as_success() {
    let ctx = context();
    let mut state = state();
    let mut events = state.convert_chunk(&ctx, &start()).unwrap();
    events.extend(state.convert_chunk(&ctx, &json!({
        "type":"content_block_start","index":0,
        "content_block":{"type":"server_tool_use","id":"search_1","name":"web_search","input":{"query":"docs"}}
    })).unwrap());
    assert!(
        state
            .convert_chunk(
                &ctx,
                &json!({
                    "type":"content_block_stop","index":0
                })
            )
            .unwrap()
            .is_empty()
    );
    let failed = state.convert_chunk(&ctx, &json!({
        "type":"content_block_start","index":1,
        "content_block":{"type":"web_search_tool_result","tool_use_id":"search_1",
            "content":{"type":"web_search_tool_result_error","error_code":"rate_limit_exceeded"}}
    })).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["type"], "response.output_item.done");
    assert_eq!(failed[0]["item"]["status"], "failed");
    events.extend(failed);
    for input in end() {
        events.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    assert!(
        events
            .iter()
            .all(|event| event["type"] != "response.web_search_call.completed")
    );
    assert_eq!(response(&events)["output"][0]["status"], "failed");
}

#[test]
fn native_hosted_search_requires_a_paired_valid_result() {
    let ctx = context();
    let mut missing = state();
    missing.convert_chunk(&ctx, &start()).unwrap();
    missing.convert_chunk(&ctx, &json!({
        "type":"content_block_start","index":0,
        "content_block":{"type":"server_tool_use","id":"search_1","name":"web_search","input":{"query":"docs"}}
    })).unwrap();
    missing
        .convert_chunk(&ctx, &json!({"type":"content_block_stop","index":0}))
        .unwrap();
    assert!(
        missing
            .convert_chunk(&ctx, &json!({"type":"message_stop"}))
            .is_err()
    );
    let failed = missing.fail_stream("invalid_upstream_event", "missing hosted result", "");
    assert!(
        failed
            .iter()
            .any(|event| event["type"] == "response.failed")
    );
    assert!(!failed.iter().any(|event| {
        matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.web_search_call.completed")
        )
    }));
    assert!(missing.finalize(&ctx).unwrap().is_empty());

    let mut orphan = state();
    orphan.convert_chunk(&ctx, &start()).unwrap();
    assert!(orphan.convert_chunk(&ctx, &json!({
        "type":"content_block_start","index":0,
        "content_block":{"type":"web_search_tool_result","tool_use_id":"unknown","content":[]}
    })).is_err());

    let mut invalid = state();
    invalid.convert_chunk(&ctx, &start()).unwrap();
    invalid.convert_chunk(&ctx, &json!({
        "type":"content_block_start","index":0,
        "content_block":{"type":"server_tool_use","id":"search_1","name":"web_search","input":{}}
    })).unwrap();
    invalid
        .convert_chunk(
            &ctx,
            &json!({
                "type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta","partial_json":"{\"query\":"}
            }),
        )
        .unwrap();
    assert!(
        invalid
            .convert_chunk(&ctx, &json!({"type":"content_block_stop","index":0}))
            .is_err()
    );
}

#[test]
fn native_usage_retains_cumulative_cache_ttl_counters_without_double_counting() {
    let ctx = context();
    let mut state = state();
    let mut events = state.convert_chunk(&ctx, &start()).unwrap();
    for input in [
        json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{
            "input_tokens":0,"cache_read_input_tokens":0,"output_tokens":4,
            "cache_creation":{"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":4}
        }}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{
            "output_tokens":7,
            "cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":3}
        }}),
        json!({"type":"message_stop"}),
    ] {
        events.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    let usage = &response(&events)["usage"];
    assert_eq!(usage["input_tokens"], 18);
    assert_eq!(usage["input_tokens_details"]["cached_tokens"], 3);
    assert_eq!(usage["cache_creation_input_tokens"], 6);
    assert_eq!(usage["output_tokens"], 7);
    assert_eq!(usage["total_tokens"], 25);
    let canonical = state.usage().unwrap();
    assert_eq!(canonical["prompt_tokens"], 18);
    assert_eq!(
        canonical["billing_usage"]["claude_usage"]["input_tokens"],
        9
    );
    assert_eq!(
        canonical["billing_usage"]["claude_usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
        4
    );
}

fn chat_state(from: Format, to: Format) -> ResponseStreamState {
    ResponseStreamState::new(
        from,
        to,
        StreamOptions {
            id: "reply_fixed".into(),
            model: "requested-model".into(),
            created_at: 1_700_000_000,
            include_usage: true,
            preserve_message_structure: true,
            ..StreamOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn claude_chat_stream_keeps_parallel_indices_signatures_and_exact_replay_blocks() {
    let ctx = context();
    let mut state = chat_state(Format::Claude, Format::OpenAi);
    let mut chunks = state.convert_chunk(&ctx, &start()).unwrap();
    for input in [
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"thinking","thinking":"initial","signature":"prefix-","extension":{"x":1}
        }}),
        json!({"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"call_a","name":"one","input":{}}}),
        json!({"type":"content_block_start","index":12,"content_block":{"type":"tool_use","id":"call_b","name":"two","input":{}}}),
        json!({"type":"content_block_delta","index":12,"delta":{"type":"input_json_delta","partial_json":"{\"b\":2}"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"-later"}}),
        json!({"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"suffix"}}),
        json!({"type":"content_block_start","index":18,"content_block":{"type":"text","text":"answer"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":2,"content_block":{
            "type":"redacted_thinking","data":"OPAQUE","extension":{"opaque":true}
        }}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_stop","index":7}),
        json!({"type":"content_block_stop","index":12}),
        json!({"type":"content_block_stop","index":18}),
    ] {
        chunks.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    for input in end() {
        chunks.extend(state.convert_chunk(&ctx, &input).unwrap());
    }
    assert!(state.finalize(&ctx).unwrap().is_empty());
    let thinking: String = chunks
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["reasoning_content"].as_str())
        .collect();
    assert_eq!(thinking, "initial-later");
    let signatures: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["reasoning_signature"].as_str())
        .collect();
    assert_eq!(signatures, ["prefix-", "suffix", "OPAQUE"]);
    let calls: Vec<_> = chunks
        .iter()
        .flat_map(|chunk| {
            chunk["choices"][0]["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect();
    let announced: Vec<_> = calls
        .iter()
        .filter(|call| call["id"].is_string())
        .map(|call| {
            (
                call["index"].as_u64().unwrap(),
                call["id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(announced, [(0, "call_a"), (1, "call_b")]);
    let arguments: Vec<_> = calls
        .iter()
        .filter_map(|call| {
            call["function"]["arguments"]
                .as_str()
                .filter(|args| !args.is_empty())
                .map(|args| (call["index"].as_u64().unwrap(), args))
        })
        .collect();
    assert_eq!(arguments, [(1, "{\"b\":2}"), (0, "{\"a\":1}")]);
    let raw: Vec<_> = chunks
        .iter()
        .flat_map(|chunk| {
            chunk["choices"][0]["delta"]["claude_reasoning_blocks"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned()
        })
        .collect();
    assert_eq!(
        raw,
        [
            json!({"type":"thinking","thinking":"initial-later","signature":"prefix-suffix","extension":{"x":1}}),
            json!({"type":"redacted_thinking","data":"OPAQUE","extension":{"opaque":true}}),
        ]
    );
    assert!(chunks.iter().all(|chunk| {
        chunk["id"] == "reply_fixed"
            && chunk["model"] == "requested-model"
            && chunk["created"] == 1_700_000_000
    }));
    let usage = chunks
        .iter()
        .rev()
        .find_map(|chunk| chunk.get("usage").filter(|usage| usage.is_object()))
        .unwrap();
    assert_eq!(usage["prompt_tokens"], 14);
    assert_eq!(usage["completion_tokens"], 7);
    assert_eq!(usage["total_tokens"], 21);

    let replay = Request::from_value(Format::OpenAi, json!({
        "model":"claude-sonnet-4-5","max_completion_tokens":4096,
        "messages":[
            {"role":"user","content":"question"},
            {"role":"assistant","content":"answer","reasoning_content":thinking,
                "reasoning_signature":"OPAQUE","claude_reasoning_blocks":raw,
                "tool_calls":[
                    {"type":"function","id":"call_a","function":{"name":"one","arguments":"{\"a\":1}"}},
                    {"type":"function","id":"call_b","function":{"name":"two","arguments":"{\"b\":2}"}}
                ]},
            {"role":"tool","tool_call_id":"call_a","content":"ok"},
            {"role":"tool","tool_call_id":"call_b","content":"ok"},
            {"role":"user","content":"continue"},
        ]
    })).unwrap();
    let replay = convert_request(&ctx, Format::Claude, &replay)
        .unwrap()
        .value
        .into_value();
    let assistant = replay["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(&assistant["content"].as_array().unwrap()[..2], &raw);
}

#[test]
fn chat_claude_stream_keeps_late_signature_on_its_open_thinking_index() {
    let ctx = context();
    let mut state = chat_state(Format::OpenAi, Format::Claude);
    let mut events = Vec::new();
    for input in [
        json!({"id":"chatcmpl-1","model":"upstream","choices":[{"index":0,"delta":{
            "role":"assistant","reasoning_content":"why","content":"answer",
            "tool_calls":[{"index":9,"id":"call_1","type":"function","function":{"name":"exec","arguments":"{\"x\":"}}]
        },"finish_reason":null}]}),
        json!({"id":"chatcmpl-1","choices":[{"index":0,"delta":{
            "reasoning_signature":"cipher",
            "tool_calls":[{"index":9,"function":{"arguments":"1}"}}]
        },"finish_reason":"tool_calls"}]}),
        json!({"id":"chatcmpl-1","choices":[],"usage":{
            "prompt_tokens":12,"completion_tokens":7,"total_tokens":19,
            "prompt_tokens_details":{"cached_tokens":4,"cached_creation_tokens":2}
        }}),
    ] {
        let converted = state.convert_chunk(&ctx, &input).unwrap();
        assert!(
            !converted
                .iter()
                .any(|event| event["type"] == "message_stop")
        );
        events.extend(converted);
    }
    let before = events.len();
    events.extend(state.finalize(&ctx).unwrap());
    assert_eq!(events.last().unwrap()["type"], "message_stop");
    assert!(state.finalize(&ctx).unwrap().is_empty());
    let blocks: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .collect();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0]["content_block"]["type"], "thinking");
    assert_eq!(blocks[0]["index"], 0);
    assert_eq!(blocks[1]["content_block"]["type"], "text");
    assert_eq!(blocks[2]["content_block"]["type"], "tool_use");
    let signature = events
        .iter()
        .position(|event| event["delta"]["type"] == "signature_delta")
        .unwrap();
    assert_eq!(events[signature]["index"], 0);
    assert_eq!(events[signature]["delta"]["signature"], "cipher");
    let stopped = events
        .iter()
        .position(|event| event["type"] == "content_block_stop" && event["index"] == 0)
        .unwrap();
    assert!(signature < stopped && stopped >= before);
    let arguments: String = events
        .iter()
        .filter(|event| event["delta"]["type"] == "input_json_delta")
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(arguments, "{\"x\":1}");
    let usage = events
        .iter()
        .find(|event| event["type"] == "message_delta")
        .unwrap();
    assert_eq!(usage["delta"]["stop_reason"], "tool_use");
    assert_eq!(usage["usage"]["input_tokens"], 6);
    assert_eq!(usage["usage"]["cache_read_input_tokens"], 4);
    assert_eq!(usage["usage"]["cache_creation_input_tokens"], 2);
    assert_eq!(usage["usage"]["output_tokens"], 7);
}

#[test]
fn chat_claude_stream_reassembles_fragmented_redacted_data_without_plaintext() {
    let ctx = context();
    let mut state = chat_state(Format::OpenAi, Format::Claude);
    let mut events = Vec::new();
    for input in [
        json!({"id":"chatcmpl-1","model":"upstream","choices":[{"index":0,"delta":{
            "reasoning_block_type":"redacted_thinking","reasoning_signature":"OPA"
        },"finish_reason":null}]}),
        json!({"id":"chatcmpl-1","choices":[{"index":0,"delta":{
            "reasoning_content_signature":"QUE","content":"answer"
        },"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}),
    ] {
        let converted = state.convert_chunk(&ctx, &input).unwrap();
        assert!(
            !converted
                .iter()
                .any(|event| event["type"] == "message_stop")
        );
        events.extend(converted);
    }
    events.extend(state.finalize(&ctx).unwrap());
    let redacted: Vec<_> = events
        .iter()
        .filter(|event| event["content_block"]["type"] == "redacted_thinking")
        .collect();
    assert_eq!(redacted.len(), 1);
    assert_eq!(redacted[0]["content_block"]["data"], "OPAQUE");
    assert!(events.iter().all(|event| {
        !matches!(
            event["delta"]["type"].as_str(),
            Some("thinking_delta" | "signature_delta")
        ) && event["content_block"]["type"] != "thinking"
    }));
    assert_eq!(events.last().unwrap()["type"], "message_stop");
}
