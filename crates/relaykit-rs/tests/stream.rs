use relaykit::{Context, Format, ResponseStreamState, StreamOptions};
use serde_json::{Value, json};
fn options() -> StreamOptions {
    StreamOptions {
        id: "stream_fixed".into(),
        model: "stream-model".into(),
        created_at: 1_700_000_000,
        ..Default::default()
    }
}
fn context() -> Context {
    Context {
        channel_meta_attached: true,
        upstream_model_name: "upstream-model".into(),
        ..Default::default()
    }
}
fn volatile(v: &mut Value) {
    match v {
        Value::Array(a) => {
            for v in a {
                volatile(v)
            }
        }
        Value::Object(o) => {
            for (k, v) in o {
                if k == "created" || k == "created_at" {
                    *v = json!(0);
                } else {
                    volatile(v);
                }
            }
        }
        _ => {}
    }
}
fn fixture(f: Format) -> Vec<Value> {
    match f {
        Format::OpenAi => vec![
            json!({"id":"chatcmpl-fixed","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}),
            json!({"id":"chatcmpl-fixed","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{"content":" world"}}]}),
            json!({"id":"chatcmpl-fixed","object":"chat.completion.chunk","created":1700000000,"model":"gpt-test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}),
        ],
        Format::Claude => vec![
            json!({"type":"message_start","message":{"id":"msg_fixed","type":"message","role":"assistant","model":"claude-test","content":[],"usage":{"input_tokens":4,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello world"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        Format::Gemini => vec![
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]}),
            json!({"candidates":[{"finishReason":"STOP","content":{"role":"model","parts":[{"text":" world"}]}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6}}),
        ],
        Format::OpenAiResponses => vec![
            json!({"type":"response.output_text.delta","delta":"Hello"}),
            json!({"type":"response.output_text.delta","delta":" world"}),
            json!({"type":"response.completed","response":{"id":"resp_fixed","object":"response","status":"completed","model":"gpt-test","usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}),
        ],
    }
}
#[test]
fn upstream_stream_golden_matrix() {
    for from in Format::ALL {
        for to in Format::ALL {
            if from == to {
                continue;
            }
            let mut state = ResponseStreamState::new(from, to, options()).unwrap();
            let ctx = context();
            let mut events = Vec::new();
            for input in fixture(from) {
                let input = relaykit::wire::normalize_stream(from, &input).unwrap();
                events.extend(state.convert_chunk(&ctx, &input).unwrap());
            }
            events.extend(state.finalize(&ctx).unwrap());
            assert!(state.finalize(&ctx).unwrap().is_empty());
            let mut got = json!(events);
            volatile(&mut got);
            let file = format!(
                "{}/tests/fixtures/golden/stream/{}_to_{}.golden.json",
                env!("CARGO_MANIFEST_DIR"),
                from.as_str(),
                to.as_str()
            );
            let golden: Value =
                serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            let want = json!(
                golden["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e.get("Payload").unwrap_or(e).clone())
                    .collect::<Vec<_>>()
            );
            if got != want {
                std::fs::write(
                    format!("/tmp/stream-{}_to_{}.actual.json", from, to),
                    serde_json::to_string_pretty(&got).unwrap(),
                )
                .unwrap();
            }
            assert_eq!(got, want, "{from} -> {to}");
        }
    }
}
#[test]
fn gemini_waits_for_complete_function_arguments() {
    let ctx = Context::default();
    let mut s = ResponseStreamState::new(Format::OpenAi, Format::Gemini, options()).unwrap();
    assert!(s.convert_chunk(&ctx,&json!({"choices":[{"delta":{"tool_calls":[{"index":3,"id":"call_a","function":{"name":"weather","arguments":"{\"city\":"}}]}}]})).unwrap().is_empty());
    assert!(s.convert_chunk(&ctx,&json!({"choices":[{"delta":{"tool_calls":[{"index":3,"function":{"arguments":"\"Paris\"}"}}]}}]})).unwrap().is_empty());
    let out = s.finalize(&ctx).unwrap();
    assert_eq!(
        out[0]["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
        json!({"city":"Paris"})
    );
    assert!(s.finalize(&ctx).unwrap().is_empty());
    assert!(s.convert_chunk(&ctx, &json!({"choices":[]})).is_err());
}
#[test]
fn claude_tools_use_dense_indices() {
    let ctx = Context::default();
    let mut s = ResponseStreamState::new(Format::Claude, Format::OpenAi, options()).unwrap();
    s.convert_chunk(&ctx,&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"hi"}})).unwrap();
    let out=s.convert_chunk(&ctx,&json!({"type":"content_block_start","index":8,"content_block":{"type":"tool_use","id":"t","name":"f","input":{}}})).unwrap();
    assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    let out=s.convert_chunk(&ctx,&json!({"type":"content_block_delta","index":8,"delta":{"type":"input_json_delta","partial_json":"{}"}})).unwrap();
    assert_eq!(out[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    assert!(s.convert_chunk(&ctx,&json!({"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"{}"}})).is_err());
}
#[test]
fn responses_sequence_numbers_and_terminal_error() {
    let ctx = Context::default();
    let mut opts = options();
    opts.emit_sequence_number = true;
    let mut s = ResponseStreamState::new(Format::OpenAi, Format::OpenAiResponses, opts).unwrap();
    let mut out = s
        .convert_chunk(
            &ctx,
            &json!({"choices":[{"delta":{"reasoning_content":"think","content":"answer"}}]}),
        )
        .unwrap();
    out.extend(
        s.fail_responses_stream("upstream_error", "failed", "")
            .unwrap(),
    );
    for (i, event) in out.iter().enumerate() {
        assert_eq!(event["sequence_number"], i as u64);
    }
    assert_eq!(out.last().unwrap()["type"], "response.failed");
    assert_eq!(out.last().unwrap()["response"]["status"], "failed");
    assert!(s.finalize(&ctx).unwrap().is_empty());
}
#[test]
fn responses_to_claude_keeps_parallel_blocks_open_until_done() {
    let ctx = Context::default();
    let mut s =
        ResponseStreamState::new(Format::OpenAiResponses, Format::Claude, options()).unwrap();
    s.convert_chunk(&ctx,&json!({"type":"response.output_item.added","output_index":1,"item":{"id":"a","type":"function_call","call_id":"a","name":"f","arguments":""}})).unwrap();
    s.convert_chunk(&ctx,&json!({"type":"response.output_item.added","output_index":2,"item":{"id":"b","type":"function_call","call_id":"b","name":"g","arguments":""}})).unwrap();
    let a = s
        .convert_chunk(
            &ctx,
            &json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
        )
        .unwrap();
    let b = s
        .convert_chunk(
            &ctx,
            &json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{}"}),
        )
        .unwrap();
    assert_eq!(a[0]["index"], 0);
    assert_eq!(b[0]["index"], 1);
    let end = s.finalize(&ctx).unwrap();
    assert_eq!(
        end.iter()
            .filter(|e| e["type"] == "content_block_stop")
            .count(),
        2
    );
}
#[test]
fn gemini_partial_arguments_accumulate_strings_and_nested_arrays() {
    let ctx = Context::default();
    let mut s = ResponseStreamState::new(Format::Gemini, Format::OpenAi, options()).unwrap();
    s.convert_chunk(&ctx,&json!({"candidates":[{"index":0,"content":{"parts":[{"functionCall":{"name":"f","id":"a","willContinue":true,"partialArgs":[{"jsonPath":"$.items[0].name","stringValue":"hel"}]}}]}}]})).unwrap();
    let out=s.convert_chunk(&ctx,&json!({"candidates":[{"index":0,"content":{"parts":[{"functionCall":{"willContinue":false,"partialArgs":[{"jsonPath":"$.items[0].name","stringValue":"lo"}]}}]}}]})).unwrap();
    assert_eq!(
        out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"items\":[{\"name\":\"hello\"}]}"
    );
    assert!(s.finalize(&ctx).is_ok());
}

#[test]
fn hosted_bridges_preserve_provider_tool_lifecycle() {
    use relaykit::stream::{ClaudeHostedStreamBridge, GeminiHostedStreamBridge};
    let mut state =
        ResponseStreamState::new(Format::Claude, Format::OpenAiResponses, options()).unwrap();
    let mut bridge = ClaudeHostedStreamBridge::new();
    let (out,handled)=bridge.convert(&json!({"type":"content_block_start","index":4,"content_block":{"type":"server_tool_use","id":"search_1","name":"web_search","input":{}}}),&mut state).unwrap();
    assert!(handled && out.is_empty());
    bridge.convert(&json!({"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust\"}"}}),&mut state).unwrap();
    let (out, handled) = bridge
        .convert(&json!({"type":"content_block_stop","index":4}), &mut state)
        .unwrap();
    assert!(handled);
    assert_eq!(
        out.iter()
            .map(|v| v["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "response.created",
            "response.output_item.added",
            "response.web_search_call.in_progress",
            "response.web_search_call.searching"
        ]
    );
    assert_eq!(
        out[1]["item"]["action"],
        json!({"type":"search","query":"rust"})
    );
    let(out,handled)=bridge.convert(&json!({"type":"content_block_start","index":5,"content_block":{"type":"web_search_tool_result","tool_use_id":"search_1","content":[]}}),&mut state).unwrap();
    assert!(handled);
    assert_eq!(out[0]["type"], "response.web_search_call.completed");
    let end = state.finalize(&Context::default()).unwrap();
    assert_eq!(
        end.last().unwrap()["response"]["output"][0]["status"],
        "completed"
    );
    let mut state =
        ResponseStreamState::new(Format::Gemini, Format::OpenAiResponses, options()).unwrap();
    let mut bridge = GeminiHostedStreamBridge::new();
    for _ in 0..2 {
        bridge.observe(
            &json!({"candidates":[{"groundingMetadata":{"webSearchQueries":["rust","tokio"]}}]}),
        );
    }
    let out = bridge.finalize(&mut state).unwrap();
    assert_eq!(
        out[1]["item"]["action"]["queries"],
        json!(["rust", "tokio"])
    );
}
#[test]
fn uncompleted_hosted_tool_makes_response_incomplete() {
    use relaykit::stream::HostedToolStart;
    let mut s =
        ResponseStreamState::new(Format::OpenAi, Format::OpenAiResponses, options()).unwrap();
    s.start_hosted_tool(&HostedToolStart {
        kind: "mcp_call".into(),
        id: "mcp1".into(),
        name: "lookup".into(),
        server_label: "server".into(),
        action: json!({"x":1}),
        ..Default::default()
    })
    .unwrap();
    let out = s.finalize(&Context::default()).unwrap();
    assert_eq!(out.last().unwrap()["type"], "response.incomplete");
    assert_eq!(
        out.last().unwrap()["response"]["output"][0]["status"],
        "incomplete"
    );
}
#[test]
fn stream_diagnostics_survive_dropped_events_and_deduplicate() {
    let mut s =
        ResponseStreamState::new(Format::OpenAiResponses, Format::OpenAi, options()).unwrap();
    let event = json!({"type":"response.web_search_call.searching","item_id":"a"});
    for _ in 0..2 {
        assert!(
            s.convert_chunk(&Context::default(), &event)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(s.diagnostics().len(), 1);
    assert_eq!(s.diagnostics()[0].code, "hosted_tool_event_unrepresentable");
    assert_eq!(s.take_diagnostics().len(), 1);
    assert!(s.take_diagnostics().is_empty());
    assert_eq!(s.diagnostics().len(), 1);
}
#[test]
fn malformed_stream_inputs_return_errors_without_panics() {
    let samples = [
        Value::Null,
        json!([]),
        json!(1),
        json!("text"),
        json!({"choices":"wrong","candidates":"wrong","response":"wrong","content_block":"wrong"}),
    ];
    for from in Format::ALL {
        for to in Format::ALL {
            for sample in &samples {
                let mut s = ResponseStreamState::new(from, to, options()).unwrap();
                assert!(
                    s.convert_chunk(&Context::default(), sample).is_err(),
                    "{from} to {to}: {sample}"
                );
            }
        }
    }
}

#[test]
fn claude_stream_keeps_reasoning_and_text_in_the_same_chunk() {
    let ctx = Context::default();
    let mut state = ResponseStreamState::new(Format::OpenAi, Format::Claude, options()).unwrap();
    state
        .convert_chunk(
            &ctx,
            &json!({"choices":[{"delta":{"reasoning_content":"first "}}]}),
        )
        .unwrap();
    let mut events = state
        .convert_chunk(
            &ctx,
            &json!({"choices":[{"delta":{"reasoning_content":"last","content":"answer"},"finish_reason":"stop"}]}),
        )
        .unwrap();
    events.extend(state.finalize(&ctx).unwrap());
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    assert_eq!(events[0]["index"], 0);
    assert_eq!(events[0]["delta"]["thinking"], "last");
    assert_eq!(events[1]["index"], 0);
    assert_eq!(events[2]["index"], 1);
    assert_eq!(events[2]["content_block"]["type"], "text");
    assert_eq!(events[3]["index"], 1);
    assert_eq!(events[3]["delta"]["text"], "answer");
    assert_eq!(events[5]["delta"]["stop_reason"], "end_turn");
}

#[test]
fn stream_usage_accepts_explicit_zero_cached_modalities() {
    let ctx = Context::default();
    let mut opts = options();
    opts.include_usage = true;
    let mut state =
        ResponseStreamState::new(Format::OpenAiResponses, Format::OpenAi, opts).unwrap();
    state
        .convert_chunk(
            &ctx,
            &json!({"type":"response.created","response":{"usage":{"input_tokens":20,"output_tokens":2,"total_tokens":22,"input_tokens_details":{"cached_tokens":15,"cached_tokens_details":{"text_tokens":10,"image_tokens":5}}}}}),
        )
        .unwrap();
    let events = state
        .convert_chunk(
            &ctx,
            &json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":25,"output_tokens":4,"total_tokens":29,"input_tokens_details":{"cached_tokens":0,"cached_tokens_details":{"text_tokens":0}}}}}),
        )
        .unwrap();
    for usage in [state.usage().unwrap(), &events.last().unwrap()["usage"]] {
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 15);
        let expected = json!({"text_tokens":0,"image_tokens":5});
        assert_eq!(
            usage["prompt_tokens_details"]["cached_tokens_details"],
            expected
        );
        assert_eq!(
            usage["billing_usage"]["openai_usage"]["input_tokens_details"]["cached_tokens_details"],
            expected
        );
    }
}

#[test]
fn stream_gemini_billing_usage_merges_cumulative_modalities() {
    let ctx = Context::default();
    let mut state = ResponseStreamState::new(Format::Gemini, Format::OpenAi, options()).unwrap();
    state
        .convert_chunk(
            &ctx,
            &json!({"candidates":[{"content":{"parts":[{"text":"answer"}]}}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2,"thoughtsTokenCount":10,"totalTokenCount":32,"promptTokensDetails":[{"modality":"TEXT","tokenCount":20},{"modality":"IMAGE","tokenCount":3}],"toolUsePromptTokensDetails":[{"modality":"AUDIO","tokenCount":4}],"candidatesTokensDetails":[{"modality":"TEXT","tokenCount":2},{"modality":"IMAGE","tokenCount":1}]}}),
        )
        .unwrap();
    state
        .convert_chunk(
            &ctx,
            &json!({"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":25,"candidatesTokenCount":4,"thoughtsTokenCount":0,"totalTokenCount":29,"promptTokensDetails":[{"modality":" text ","tokenCount":10},{"modality":"TEXT","tokenCount":15},{"modality":"IMAGE","tokenCount":0}],"candidatesTokensDetails":[{"modality":"TEXT","tokenCount":4}]}}),
        )
        .unwrap();
    let usage = &state.usage().unwrap()["billing_usage"]["gemini_usage_metadata"];
    assert_eq!(usage["thoughtsTokenCount"], 0);
    assert_eq!(usage["totalTokenCount"], 29);
    assert_eq!(
        usage["promptTokensDetails"],
        json!([
            {"modality":"IMAGE","tokenCount":3},
            {"modality":" text ","tokenCount":10},
            {"modality":"TEXT","tokenCount":15},
        ])
    );
    assert_eq!(
        usage["toolUsePromptTokensDetails"],
        json!([{"modality":"AUDIO","tokenCount":4}])
    );
    assert_eq!(
        usage["candidatesTokensDetails"],
        json!([
            {"modality":"IMAGE","tokenCount":1},
            {"modality":"TEXT","tokenCount":4},
        ])
    );
}

#[test]
fn stream_gemini_billing_usage_computes_missing_totals_and_retains_zero_counters() {
    let ctx = Context::default();
    let mut state = ResponseStreamState::new(Format::Gemini, Format::OpenAi, options()).unwrap();
    state
        .convert_chunk(
            &ctx,
            &json!({"candidates":[{"content":{"parts":[{"text":"answer"}]}}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2,"thoughtsTokenCount":10}}),
        )
        .unwrap();
    assert_eq!(
        state.usage().unwrap()["billing_usage"]["gemini_usage_metadata"]["totalTokenCount"],
        32
    );
    state
        .convert_chunk(&ctx, &json!({"usageMetadata":{"thoughtsTokenCount":0}}))
        .unwrap();
    let native = &state.usage().unwrap()["billing_usage"]["gemini_usage_metadata"];
    assert_eq!(native["thoughtsTokenCount"], 10);
    assert_eq!(native["totalTokenCount"], 32);
}

#[test]
fn gemini_stream_distinguishes_empty_finish_reason_from_missing() {
    let ctx = Context::default();
    for finish in [None, Some(Value::Null), Some(json!(""))] {
        let mut state =
            ResponseStreamState::new(Format::Gemini, Format::OpenAi, options()).unwrap();
        let mut candidate = json!({"content":{"parts":[{"text":"answer"}]}});
        if let Some(finish) = &finish {
            candidate["finishReason"] = finish.clone();
        }
        let events = state
            .convert_chunk(&ctx, &json!({"candidates":[candidate]}))
            .unwrap();
        if finish.as_ref().is_none_or(Value::is_null) {
            assert!(events[0]["choices"][0]["finish_reason"].is_null());
            assert_eq!(
                state.finalize(&ctx).unwrap()[0]["choices"][0]["finish_reason"],
                "stop"
            );
        } else {
            assert_eq!(events[0]["choices"][0]["finish_reason"], "content_filter");
            assert!(state.finalize(&ctx).unwrap().is_empty());
        }
    }
}

#[test]
fn prepared_stream_chunks_replay_without_mutation() {
    use relaykit::stream::PreparedStreamChunk;
    let ctx = context();
    for from in Format::ALL {
        let source = fixture(from);
        let prepared = source
            .iter()
            .map(|value| PreparedStreamChunk::from_value(from, value.clone()).unwrap())
            .collect::<Vec<_>>();
        let fingerprint = prepared
            .iter()
            .map(|chunk| chunk.as_value().clone())
            .collect::<Vec<_>>();
        for to in Format::ALL {
            let run = |prepared_path: bool| {
                let mut state = ResponseStreamState::new(from, to, options()).unwrap();
                let mut events = Vec::new();
                for (value, chunk) in source.iter().zip(&prepared) {
                    events.extend(if prepared_path {
                        state.convert_prepared_chunk(&ctx, chunk).unwrap()
                    } else {
                        state.convert_chunk(&ctx, value).unwrap()
                    });
                }
                events.extend(state.finalize(&ctx).unwrap());
                (events, state.usage().cloned(), state.diagnostics().to_vec())
            };
            assert_eq!(run(true), run(false), "{from} -> {to}");
            assert_eq!(run(true), run(false), "replayed {from} -> {to}");
        }
        assert_eq!(
            prepared
                .iter()
                .map(|chunk| chunk.as_value().clone())
                .collect::<Vec<_>>(),
            fingerprint
        );
    }
    assert!(PreparedStreamChunk::from_value(Format::OpenAi, Value::Null).is_err());
    let wrong = PreparedStreamChunk::from_value(Format::Gemini, json!({})).unwrap();
    let mut state = ResponseStreamState::new(Format::Claude, Format::OpenAi, options()).unwrap();
    assert!(state.convert_prepared_chunk(&ctx, &wrong).is_err());
}

#[test]
fn prepared_stream_diagnostics_preserve_raw_field_presence() {
    use relaykit::stream::PreparedStreamChunk;
    // DTO case folding can reveal fields that the raw diagnostic inspector does
    // not see. Preparing an event must not change the existing API's diagnostics.
    let value = json!({"TYPE":"response.output_item.added","ITEM":{"type":"web_search_call","id":"hosted_fixture","status":"in_progress"}});
    let prepared = PreparedStreamChunk::from_value(Format::OpenAiResponses, value.clone()).unwrap();
    let ctx = context();
    let mut raw =
        ResponseStreamState::new(Format::OpenAiResponses, Format::OpenAi, options()).unwrap();
    let mut cached =
        ResponseStreamState::new(Format::OpenAiResponses, Format::OpenAi, options()).unwrap();
    assert_eq!(
        raw.convert_chunk(&ctx, &value).unwrap(),
        cached.convert_prepared_chunk(&ctx, &prepared).unwrap()
    );
    assert_eq!(raw.diagnostics(), cached.diagnostics());
    assert_eq!(raw.take_diagnostics(), cached.take_diagnostics());
}

#[test]
fn stream_usage_setter_keeps_caller_snapshot_until_the_next_merge() {
    let seed = json!({"prompt_tokens":0,"completion_tokens":0,"extension":"ignored by canonical usage",
        "input_tokens_details":{"cached_tokens_details":{"text_tokens":0}}});
    let mut state =
        ResponseStreamState::new(Format::OpenAi, Format::OpenAiResponses, options()).unwrap();
    state.set_usage(seed.clone());
    assert_eq!(state.usage(), Some(&seed));
    state
        .convert_chunk(
            &context(),
            &json!({"choices":[{"delta":{"content":"answer"}}]}),
        )
        .unwrap();
    let usage = state.usage().unwrap();
    assert!(usage.get("extension").is_none());
    assert_eq!(
        usage["input_tokens_details"]["cached_tokens_details"]["text_tokens"],
        0
    );
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 0);
    assert_eq!(usage["total_tokens"], 0);
}

#[test]
fn identity_stream_usage_matches_pinned_go_before_and_after_preparation() {
    use relaykit::stream::PreparedStreamChunk;
    let cases: Value = serde_json::from_str(include_str!("fixtures/stream_identity.json")).unwrap();
    for entry in cases.as_array().unwrap() {
        let case = &entry["case"];
        let from: Format = case["from"].as_str().unwrap().parse().unwrap();
        let options: StreamOptions =
            serde_json::from_value(case["stream_options"].clone()).unwrap();
        let chunks = case["input"].as_array().unwrap();
        let prepared = chunks
            .iter()
            .map(|chunk| {
                PreparedStreamChunk::from_slice(from, &serde_json::to_vec(chunk).unwrap()).unwrap()
            })
            .collect::<Vec<_>>();
        let mut expected = json!({"value":entry["expected"]["value"],
            "usage":entry["expected"]["usage"], "diagnostics":entry["expected"]["diagnostics"]});
        if expected["diagnostics"].is_null() {
            expected["diagnostics"] = json!([]);
        }
        for prepared_path in [false, true] {
            let mut state = ResponseStreamState::new(from, from, options.clone()).unwrap();
            let mut events = Vec::new();
            for (chunk, prepared) in chunks.iter().zip(&prepared) {
                events.extend(if prepared_path {
                    state
                        .convert_prepared_chunk(&Context::default(), prepared)
                        .unwrap()
                } else {
                    state.convert_chunk(&Context::default(), chunk).unwrap()
                });
            }
            events.extend(state.finalize(&Context::default()).unwrap());
            let actual =
                json!({"value":events,"usage":state.usage(),"diagnostics":state.diagnostics()});
            assert_eq!(
                actual, expected,
                "{} prepared={prepared_path}",
                case["name"]
            );
        }
    }
}
