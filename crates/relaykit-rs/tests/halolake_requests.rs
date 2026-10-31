use relaykit::{
    Context, Error, Format, MediaResolver, PreparedRequest, Request, convert_prepared_request,
    convert_request, convert_request_via, responses_tool_state,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn context() -> Context {
    let mut ctx = Context {
        origin_model_name: "requested-model".into(),
        upstream_model_name: "gemini-3-pro".into(),
        channel_meta_attached: true,
        ..Context::default()
    };
    ctx.options.preserve_message_structure = true;
    ctx.media = MediaResolver {
        get_base64_data: Some(Arc::new(|source, mime| {
            if let Some(data) = source.strip_prefix("data:") {
                let (header, data) = data
                    .split_once(";base64,")
                    .ok_or_else(|| Error::message("invalid test data URL"))?;
                Ok((data.into(), header.into()))
            } else if !mime.is_empty() {
                Ok((source.into(), mime.into()))
            } else {
                Err(Error::message("test media resolver cannot fetch URLs"))
            }
        })),
        decode_base64_file_data: None,
    };
    ctx
}

fn convert(ctx: &Context, from: Format, to: Format, value: Value) -> Result<Value, Error> {
    let request = Request::from_value(from, value)?;
    Ok(convert_request(ctx, to, &request)?.value.into_value())
}

fn assistant_messages(value: &Value) -> Vec<&Value> {
    value["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect()
}
fn assert_chat_tool_schema(value: &Value) {
    for message in value["messages"].as_array().unwrap() {
        if message["role"] == "tool" {
            assert!(
                message["content"].is_string()
                    || message["content"].as_array().is_some_and(|parts| {
                        parts.iter().all(|part| part["type"] == "text")
                    }),
                "Chat tool content must be text-only: {message}"
            );
        }
    }
}

fn contains_key_value(value: &Value, key: &str, expected: &Value) -> bool {
    match value {
        Value::Object(object) => object.get(key) == Some(expected)
            || object.values().any(|value| contains_key_value(value, key, expected)),
        Value::Array(array) => array.iter().any(|value| contains_key_value(value, key, expected)),
        _ => false,
    }
}

#[test]
fn chat_to_responses_retains_typed_roles_at_original_positions() {
    let output = convert(
        &context(),
        Format::OpenAi,
        Format::OpenAiResponses,
        json!({
            "model":"claude-sonnet-4-5","max_completion_tokens":4096,
            "messages":[
                {"role":"system","content":"first rule"},
                {"role":"user","content":"question"},
                {"role":"developer","content":"second rule"},
                {"role":"assistant","content":"answer"},
                {"role":"system","content":"third rule"},
            ],
        }),
    )
    .unwrap();
    assert_eq!(output["max_output_tokens"], 4096);
    assert!(output.get("instructions").is_none());
    let items = output["input"].as_array().unwrap();
    let roles: Vec<_> = items.iter().map(|item| item["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["developer", "user", "developer", "assistant", "developer"]);
    assert!(items.iter().all(|item| item["type"] == "message"));
    assert_eq!(items[3]["content"][0]["type"], "output_text");
    assert_eq!(items[4]["content"][0]["text"], "third rule");
}

#[test]
fn namespace_custom_tools_history_and_forced_choice_share_one_mapping() {
    let ctx = context();
    let output = convert(
        &ctx,
        Format::OpenAiResponses,
        Format::Claude,
        json!({
            "model":"claude-sonnet-4-5","max_output_tokens":4096,
            "tools":[{"type":"namespace","name":"shell","tools":[
                {"type":"custom","name":"exec","description":"Execute a script"},
                {"type":"function","name":"cwd","parameters":{"type":"object"}},
            ]}],
            "tool_choice":{"type":"custom","namespace":"shell","name":"exec"},
            "input":[
                {"role":"user","content":"run it"},
                {"type":"custom_tool_call","call_id":"call_exec","namespace":"shell","name":"exec","input":"echo hello"},
                {"type":"custom_tool_call_output","call_id":"call_exec","output":"hello"},
                {"role":"user","content":"continue"},
            ],
        }),
    )
    .unwrap();
    let state = ctx.responses_tool_state();
    let name = state.flatten_declared("shell", "exec").unwrap();
    assert!(state.is_custom_tool(name));
    let tool = output["tools"].as_array().unwrap().iter().find(|tool| tool["name"] == name).unwrap();
    assert_eq!(tool["input_schema"]["properties"]["input"]["type"], "string");
    assert_eq!(tool["input_schema"]["required"], json!(["input"]));
    assert_eq!(tool["input_schema"]["additionalProperties"], false);
    assert_eq!(output["tool_choice"]["name"], name);
    let assistant = assistant_messages(&output);
    assert_eq!(assistant[0]["content"][0]["name"], name);
    assert_eq!(assistant[0]["content"][0]["input"], json!({"input":"echo hello"}));
    assert_eq!(state.responses_tool_name(name), ("shell".into(), "exec".into()));
}

#[test]
fn namespace_tool_names_are_reversible_bounded_and_collision_checked() {
    let namespace = "n".repeat(50);
    let tool = "tool".repeat(20);
    let state = responses_tool_state(&json!({
        "tools":[{"type":"namespace","name":namespace,"tools":[
            {"type":"function","name":tool,"parameters":{"type":"object"}}
        ]}]
    }))
    .unwrap();
    let flattened = state.flatten_declared(&namespace, &tool).unwrap();
    assert!(flattened.len() <= 64);
    assert_eq!(state.responses_tool_name(flattened), (namespace, tool));
    for tools in [
        json!([
            {"type":"function","name":"ns__run"},
            {"type":"namespace","name":"ns","tools":[{"type":"function","name":"run"}]},
        ]),
        json!([
            {"type":"namespace","name":"a","tools":[{"type":"function","name":"b__c"}]},
            {"type":"namespace","name":"a__b","tools":[{"type":"function","name":"c"}]},
        ]),
        json!([{"type":"function","name":"tool_search"},{"type":"tool_search"}]),
    ] {
        assert!(responses_tool_state(&json!({"tools":tools})).is_err());
    }
}

#[test]
fn additional_tools_history_deduplicates_identical_declarations() {
    let tool = json!({"type":"function","name":"lookup","parameters":{"type":"object"}});
    let output = convert(
        &context(),
        Format::OpenAiResponses,
        Format::Claude,
        json!({
            "model":"claude-sonnet-4-5","max_output_tokens":4096,"tools":[tool],
            "input":[
                {"role":"user","content":"lookup"},
                {"type":"additional_tools","tools":[tool, tool]},
            ],
        }),
    )
    .unwrap();
    assert_eq!(output["tools"].as_array().unwrap().len(), 1);
    assert_eq!(output["tools"][0]["name"], "lookup");
}

#[test]
fn tool_search_client_calls_translate_to_all_upstreams() {
    for target in [Format::OpenAi, Format::Claude, Format::Gemini] {
        let ctx = context();
        let output = convert(
            &ctx,
            Format::OpenAiResponses,
            target,
            json!({
                "model":"upstream-model","max_output_tokens":4096,
                "tools":[{"type":"tool_search","execution":"client"}],
                "tool_choice":{"type":"tool_search"},
                "input":[
                    {"role":"user","content":"find a tool"},
                    {"type":"tool_search_call","call_id":"search_1","execution":"client","arguments":{"query":"files"}},
                    {"type":"tool_search_output","call_id":"search_1","output":[{"type":"input_text","text":"found"}]},
                    {"role":"user","content":"continue"},
                ],
            }),
        )
        .unwrap();
        assert!(ctx.responses_tool_state().tool_search);
        match target {
            Format::OpenAi => {
                assert_eq!(assistant_messages(&output)[0]["tool_calls"][0]["function"]["name"], "tool_search");
                assert_eq!(output["tool_choice"]["function"]["name"], "tool_search");
            }
            Format::Claude => {
                assert_eq!(assistant_messages(&output)[0]["content"][0]["name"], "tool_search");
                assert_eq!(output["tool_choice"]["name"], "tool_search");
            }
            Format::Gemini => {
                assert_eq!(output["contents"][1]["parts"][0]["functionCall"]["name"], "tool_search");
                assert_eq!(output["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"], json!(["tool_search"]));
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn raw_claude_reasoning_blocks_replay_exactly_without_synthesized_duplicates() {
    let blocks = json!([
        {"type":"thinking","thinking":"first","signature":"opaque-1","extension":{"x":1}},
        {"type":"redacted_thinking","data":"opaque-redacted"},
        {"type":"thinking","thinking":"second","signature":"opaque-2"},
    ]);
    let request = json!({
        "model":"claude-sonnet-4-5","max_completion_tokens":4096,
        "messages":[
            {"role":"user","content":"question"},
            {"role":"assistant","content":"answer","reasoning_content":"firstsecond",
                "reasoning_signature":"opaque-2","claude_reasoning_blocks":blocks},
            {"role":"user","content":"continue"},
        ],
    });
    let claude = convert(&context(), Format::OpenAi, Format::Claude, request.clone()).unwrap();
    let assistant = assistant_messages(&claude);
    assert_eq!(&assistant[0]["content"].as_array().unwrap()[..3], blocks.as_array().unwrap());
    assert_eq!(assistant[0]["content"].as_array().unwrap().len(), 4);

    let responses = convert(&context(), Format::OpenAi, Format::OpenAiResponses, request).unwrap();
    let reasoning: Vec<_> = responses["input"].as_array().unwrap().iter().filter(|item| item["type"] == "reasoning").collect();
    assert_eq!(reasoning.len(), 3);
    assert_eq!(reasoning[0]["encrypted_content"], "opaque-1");
    assert_eq!(reasoning[1]["claude_block_type"], "redacted_thinking");
    assert_eq!(reasoning[1]["encrypted_content"], "opaque-redacted");
    assert_eq!(reasoning[2]["encrypted_content"], "opaque-2");
}

#[test]
fn scalar_redacted_reasoning_and_signature_aliases_survive_normalization() {
    for signature_key in ["reasoning_signature", "reasoning_content_signature", "reasoningSignature", "encrypted_content"] {
        let mut message = json!({"role":"assistant","content":"answer","reasoning_block_type":"redacted_thinking"});
        message[signature_key] = json!("opaque-redacted");
        let request = json!({
            "model":"claude-sonnet-4-5","max_completion_tokens":4096,
            "messages":[{"role":"user","content":"question"},message,{"role":"user","content":"continue"}],
        });
        let output = convert(&context(), Format::OpenAi, Format::Claude, request).unwrap();
        assert_eq!(assistant_messages(&output)[0]["content"][0], json!({"type":"redacted_thinking","data":"opaque-redacted"}));
    }
}

#[test]
fn reasoning_turns_remain_separate_across_parallel_tool_replies() {
    for target in [Format::OpenAi, Format::Claude] {
        let output = convert(
            &context(),
            Format::OpenAiResponses,
            target,
            json!({
                "model":"claude-sonnet-4-5","max_output_tokens":4096,
                "input":[
                    {"role":"user","content":"go"},
                    {"type":"reasoning","summary":[{"type":"summary_text","text":"first thought"}],"encrypted_content":"sig-1"},
                    {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
                    {"type":"function_call","call_id":"call_2","name":"fetch","arguments":"{}"},
                    {"type":"function_call_output","call_id":"call_1","output":"a"},
                    {"type":"function_call_output","call_id":"call_2","output":"b"},
                    {"type":"reasoning","summary":[{"type":"summary_text","text":"second thought"}],"encrypted_content":"sig-2"},
                ],
            }),
        )
        .unwrap();
        let assistant = assistant_messages(&output);
        assert_eq!(assistant.len(), 2, "{output}");
        if target == Format::OpenAi {
            assert_eq!(assistant[0]["reasoning_content"], "first thought");
            assert_eq!(assistant[0]["reasoning_signature"], "sig-1");
            assert_eq!(assistant[0]["tool_calls"].as_array().unwrap().len(), 2);
            assert_eq!(assistant[1]["reasoning_content"], "second thought");
            assert_eq!(assistant[1]["reasoning_signature"], "sig-2");
        } else {
            assert_eq!(assistant[0]["content"][0]["thinking"], "first thought");
            assert_eq!(assistant[0]["content"].as_array().unwrap().len(), 3);
            assert_eq!(assistant[1]["content"][0]["thinking"], "second thought");
            assert_eq!(assistant[1]["content"][0]["signature"], "sig-2");
        }
    }
}

#[test]
fn media_urls_stay_host_owned_and_use_native_claude_and_gemini_sources() {
    let mut ctx = context();
    ctx.media.get_base64_data = Some(Arc::new(|_, _| panic!("native URLs must not invoke a fetch callback")));
    for (from, input) in [
        (Format::OpenAi, json!({
            "model":"upstream-model","max_completion_tokens":4096,
            "messages":[{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"https://example.test/image.png"}},
                {"type":"file","file":{"file_url":"https://example.test/document.pdf"}},
            ]}],
        })),
        (Format::OpenAiResponses, json!({
            "model":"upstream-model","max_output_tokens":4096,
            "input":[{"role":"user","content":[
                {"type":"input_image","image_url":"https://example.test/image.png"},
                {"type":"input_file","file_url":"https://example.test/document.pdf"},
            ]}],
        })),
    ] {
        let claude = convert(&ctx, from, Format::Claude, input.clone()).unwrap();
        assert_eq!(claude["messages"][0]["content"][0]["source"]["type"], "url");
        assert_eq!(claude["messages"][0]["content"][1]["source"]["url"], "https://example.test/document.pdf");
        let gemini = convert(&ctx, from, Format::Gemini, input).unwrap();
        assert_eq!(gemini["contents"][0]["parts"][0]["fileData"]["fileUri"], "https://example.test/image.png");
        assert_eq!(gemini["contents"][0]["parts"][1]["fileData"]["fileUri"], "https://example.test/document.pdf");
    }
}

#[test]
fn claude_tool_result_media_and_error_flag_round_trip_with_valid_chat_content() {
    let input = json!({
        "model":"claude-sonnet-4-5","max_tokens":4096,
        "messages":[
            {"role":"user","content":"inspect"},
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"inspect","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","is_error":true,"content":[
                {"type":"text","text":"failed"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}},
                {"type":"document","title":"report","source":{"type":"base64","media_type":"application/pdf","data":"cGRm"}},
            ]}]},
        ],
    });
    let ctx = context();
    for target in [Format::OpenAi, Format::OpenAiResponses] {
        let output = convert(&ctx, Format::Claude, target, input.clone()).unwrap();
        if target == Format::OpenAi {
            assert_chat_tool_schema(&output);
        }
        let restored = convert(&ctx, target, Format::Claude, output).unwrap();
        let results: Vec<_> = restored["messages"].as_array().unwrap().iter().flat_map(|message| message["content"].as_array().unwrap()).filter(|part| part["type"] == "tool_result").collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["is_error"], true);
        assert_eq!(results[0]["content"][0]["text"], "failed");
        if target == Format::OpenAiResponses {
            assert_eq!(results[0]["content"].as_array().unwrap().len(), 3);
            assert_eq!(results[0]["content"][1]["source"]["data"], "aW1hZ2U=");
            assert_eq!(results[0]["content"][2]["title"], "report");
        }
        assert!(contains_key_value(&restored, "data", &json!("aW1hZ2U=")));
        assert!(contains_key_value(&restored, "data", &json!("cGRm")));
        assert!(contains_key_value(&restored, "title", &json!("report")));
    }
}

#[test]
fn unsupported_content_and_unresolved_state_fail_in_preservation_mode() {
    for part in [
        json!({"type":"input_audio","input_audio":{"data":"YQ==","format":"wav"}}),
        json!({"type":"video_url","video_url":"https://example.test/video.mp4"}),
        json!({"type":"future_part","payload":{"important":true}}),
    ] {
        let request = json!({
            "model":"claude-sonnet-4-5","max_completion_tokens":4096,
            "messages":[{"role":"user","content":[part]}],
        });
        assert!(convert(&context(), Format::OpenAi, Format::Claude, request).is_err());
    }
    for target in [Format::OpenAi, Format::Claude, Format::Gemini] {
        for input in [
            json!([{"type":"item_reference","id":"item_1"}]),
            json!([{"type":"future_item","payload":{"important":true}}]),
            json!([{"role":"user","content":[{"type":"future_part"}]}]),
        ] {
            assert!(convert(
                &context(),
                Format::OpenAiResponses,
                target,
                json!({"model":"upstream-model","max_output_tokens":4096,"input":input}),
            ).is_err());
        }
        for (key, value) in [
            ("previous_response_id", json!("resp_old")),
            ("conversation", json!({"id":"conversation_old"})),
            ("prompt", json!({"id":"prompt_old"})),
        ] {
            let mut request = json!({"model":"upstream-model","max_output_tokens":4096,"input":"question"});
            request[key] = value;
            assert!(convert(&context(), Format::OpenAiResponses, target, request).is_err());
        }
    }
}

#[test]
fn prepared_requests_preserve_the_same_opaque_reasoning_as_value_requests() {
    let ctx = context();
    let input = json!({
        "model":"claude-sonnet-4-5","max_completion_tokens":4096,
        "messages":[
            {"role":"user","content":"question"},
            {"role":"assistant","content":"answer","reasoning_signature":"opaque","reasoning_content":"thought"},
            {"role":"user","content":"continue"},
        ],
    });
    let bytes = serde_json::to_vec(&input).unwrap();
    let prepared = PreparedRequest::from_slice(Format::OpenAi, &bytes).unwrap();
    let output = convert_prepared_request(&ctx, Format::Claude, &prepared).unwrap();
    assert!(!output.value.is_typed());
    assert_eq!(output.value.to_value().unwrap(), convert(&ctx, Format::OpenAi, Format::Claude, input).unwrap());
}

#[test]
fn tool_output_urls_stay_in_tool_history_and_never_invoke_a_fetch_callback() {
    let mut ctx = context();
    ctx.media.get_base64_data = Some(Arc::new(|_, _| panic!("native URLs must remain host-owned")));
    let request = json!({
        "model":"gemini-3-pro","max_output_tokens":4096,
        "input":[
            {"role":"user","content":"inspect"},
            {"type":"function_call","call_id":"call_1","name":"inspect","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_1","is_error":true,"output":[
                {"type":"input_text","text":"failed"},
                {"type":"input_image","image_url":"https://example.test/image.png"},
                {"type":"input_file","file_url":"https://example.test/report.pdf","title":"report"},
            ]},
        ],
    });
    let chat = convert(&ctx, Format::OpenAiResponses, Format::OpenAi, request.clone()).unwrap();
    let tools: Vec<_> = chat["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "tool").collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["is_error"], true);
    assert_chat_tool_schema(&chat);
    assert!(contains_key_value(&chat, "url", &json!("https://example.test/image.png")));
    assert!(contains_key_value(&chat, "file_url", &json!("https://example.test/report.pdf")));
    assert!(contains_key_value(&chat, "title", &json!("report")));
    assert_eq!(chat["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "user").count(), 2);

    let claude = convert(&ctx, Format::OpenAiResponses, Format::Claude, request.clone()).unwrap();
    let result = &claude["messages"][2]["content"][0];
    assert_eq!(result["is_error"], true);
    assert_eq!(result["content"][1]["source"]["url"], "https://example.test/image.png");
    assert_eq!(result["content"][2]["title"], "report");

    for (from, request) in [(Format::OpenAiResponses, request), (Format::OpenAi, chat)] {
        let gemini = convert(&ctx, from, Format::Gemini, request).unwrap();
        let result = &gemini["contents"][2]["parts"][0]["functionResponse"];
        assert!(result["response"]["content"].as_str().unwrap().contains("failed"));
        if from == Format::OpenAiResponses {
            assert_eq!(result["parts"].as_array().unwrap().len(), 2);
        }
        assert!(contains_key_value(&gemini, "fileUri", &json!("https://example.test/image.png")));
        assert!(contains_key_value(&gemini, "fileUri", &json!("https://example.test/report.pdf")));
    }
}

#[test]
fn scalar_json_tool_outputs_keep_their_payload_in_all_preserved_edges() {
    let payload = json!({"result":[1,2,3],"important":true});
    let request = json!({
        "model":"upstream-model","max_output_tokens":4096,
        "input":[
            {"role":"user","content":"inspect"},
            {"type":"function_call","call_id":"call_1","name":"inspect","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_1","output":payload},
        ],
    });
    for target in [Format::OpenAi, Format::Claude, Format::Gemini] {
        let output = convert(&context(), Format::OpenAiResponses, target, request.clone()).unwrap();
        let actual = match target {
            Format::OpenAi => serde_json::from_str(output["messages"][2]["content"].as_str().unwrap()).unwrap(),
            Format::Claude => serde_json::from_str(output["messages"][2]["content"][0]["content"].as_str().unwrap()).unwrap(),
            Format::Gemini => output["contents"][2]["parts"][0]["functionResponse"]["response"].clone(),
            _ => unreachable!(),
        };
        assert_eq!(actual, payload, "{output}");
    }
}

#[test]
fn late_system_and_developer_instructions_stay_in_gemini_history() {
    for (from, input) in [
        (Format::OpenAi, json!({
            "model":"upstream-model","max_completion_tokens":4096,
            "messages":[
                {"role":"system","content":"first rule"},
                {"role":"user","content":"question"},
                {"role":"developer","content":"second rule"},
                {"role":"assistant","content":"answer"},
                {"role":"system","content":"third rule"},
            ],
        })),
        (Format::OpenAiResponses, json!({
            "model":"upstream-model","max_output_tokens":4096,
            "input":[
                {"type":"message","role":"system","content":"first rule"},
                {"type":"message","role":"user","content":"question"},
                {"type":"message","role":"developer","content":"second rule"},
                {"type":"message","role":"assistant","content":"answer"},
                {"type":"message","role":"system","content":"third rule"},
            ],
        })),
    ] {
        let output = convert(&context(), from, Format::Gemini, input).unwrap();
        assert_eq!(output["systemInstruction"]["parts"][0]["text"], "first rule");
        let contents = output["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 4, "{output}");
        assert_eq!(contents[0]["parts"][0]["text"], "question");
        assert_eq!(contents[1]["parts"][0]["text"], "[developer]\nsecond rule");
        assert_eq!(contents[2]["parts"][0]["text"], "answer");
        assert_eq!(contents[3]["parts"][0]["text"], "[system]\nthird rule");
    }
}

#[test]
fn gemini_signed_reasoning_and_nested_tool_media_survive_chat_roundtrip() {
    let input = json!({
        "contents":[
            {"role":"user","parts":[{"text":"inspect"}]},
            {"role":"model","parts":[
                {"text":"first","thought":true,"thoughtSignature":"sig-1"},
                {"text":"second","thought":true,"thoughtSignature":"sig-2"},
                {"functionCall":{"id":"call_1","name":"inspect","args":{}},"thoughtSignature":"call-sig"},
            ]},
            {"role":"user","parts":[{"functionResponse":{
                "id":"call_1","name":"inspect","response":{"content":"result"},
                "parts":[
                    {"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}},
                    {"fileData":{"mimeType":"application/pdf","fileUri":"https://example.test/report.pdf"}},
                ],
            }}]},
        ],
        "generationConfig":{"maxOutputTokens":4096},
    });
    let ctx = context();
    let chat = convert(&ctx, Format::Gemini, Format::OpenAi, input).unwrap();
    let assistant = assistant_messages(&chat);
    assert_eq!(assistant[0]["reasoning_blocks"].as_array().unwrap().len(), 2);
    assert_eq!(assistant[0]["reasoning_blocks"][0]["signature"], "sig-1");
    assert_eq!(assistant[0]["reasoning_blocks"][1]["signature"], "sig-2");
    assert_eq!(assistant[0]["tool_calls"][0]["thoughtSignature"], "call-sig");
    let tool = chat["messages"].as_array().unwrap().iter().find(|message| message["role"] == "tool").unwrap();
    assert_chat_tool_schema(&chat);
    assert_eq!(tool["content"][0]["text"], "result");
    assert!(contains_key_value(&chat, "url", &json!("data:image/png;base64,aW1hZ2U=")));
    assert!(contains_key_value(&chat, "file_url", &json!("https://example.test/report.pdf")));

    let gemini = convert(&ctx, Format::OpenAi, Format::Gemini, chat).unwrap();
    let model = &gemini["contents"][1]["parts"];
    assert_eq!(model[0]["thoughtSignature"], "sig-1");
    assert_eq!(model[1]["thoughtSignature"], "sig-2");
    assert_eq!(model[2]["thoughtSignature"], "call-sig");
    let result = &gemini["contents"][2]["parts"][0]["functionResponse"];
    assert!(result["response"]["content"].as_str().unwrap().contains("result"));
    assert!(contains_key_value(&gemini, "data", &json!("aW1hZ2U=")));
    assert!(contains_key_value(&gemini, "fileUri", &json!("https://example.test/report.pdf")));
}

#[test]
fn responses_redacted_reasoning_replays_through_chat_without_becoming_signed_thinking() {
    let input = json!({
        "model":"claude-sonnet-4-5","max_output_tokens":4096,
        "input":[
            {"role":"user","content":"question"},
            {"type":"reasoning","summary":[],"encrypted_content":"opaque","claude_block_type":"redacted_thinking"},
            {"role":"assistant","content":"answer"},
            {"role":"user","content":"continue"},
        ],
    });
    let ctx = context();
    let chat = convert(&ctx, Format::OpenAiResponses, Format::OpenAi, input).unwrap();
    let claude = convert(&ctx, Format::OpenAi, Format::Claude, chat).unwrap();
    let assistant = assistant_messages(&claude);
    assert_eq!(assistant[0]["content"][0], json!({"type":"redacted_thinking","data":"opaque"}));
    assert_eq!(assistant[0]["content"].as_array().unwrap().len(), 2);
}

#[test]
fn prepared_and_explicit_routes_keep_the_same_preserved_extensions() {
    let ctx = context();
    for (from, target, input) in [
        (Format::Claude, Format::OpenAi, json!({
            "model":"claude-sonnet-4-5","max_tokens":4096,
            "messages":[
                {"role":"user","content":"question"},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"thought","signature":"opaque","extension":{"x":1}},
                    {"type":"tool_use","id":"call_1","name":"inspect","input":{}},
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","is_error":true,"content":[
                    {"type":"text","text":"failed"},
                    {"type":"document","title":"report","source":{"type":"base64","media_type":"application/pdf","data":"cGRm"}},
                ]}]},
            ],
        })),
        (Format::OpenAi, Format::Claude, json!({
            "model":"claude-sonnet-4-5","max_completion_tokens":4096,
            "messages":[
                {"role":"user","content":"question"},
                {"role":"assistant","content":"answer","claude_reasoning_blocks":[
                    {"type":"redacted_thinking","data":"opaque","extension":{"x":1}},
                ]},
                {"role":"user","content":"continue"},
            ],
        })),
    ] {
        let bytes = serde_json::to_vec(&input).unwrap();
        let prepared = PreparedRequest::from_slice(from, &bytes).unwrap();
        let request = Request::from_value(from, input).unwrap();
        let expected = convert_request(&ctx, target, &request).unwrap().value.into_value();
        let actual = convert_prepared_request(&ctx, target, &prepared).unwrap().value.to_value().unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            convert_request_via(&ctx, &request, &[from, target]).unwrap().value.into_value(),
            expected,
        );
    }
}

#[test]
fn same_format_prepared_requests_still_apply_the_native_wire_contract() {
    let ctx = context();
    for (format, input) in [
        (Format::OpenAi, json!({"model":"upstream-model","messages":[],"stream":false})),
        (Format::OpenAiResponses, json!({"model":"upstream-model","input":[],"tools":[]})),
        (Format::Claude, json!({"model":"upstream-model","messages":[],"tools":null})),
        (Format::Gemini, json!({"contents":[],"generationConfig":{},"safetySettings":[]})),
    ] {
        let bytes = serde_json::to_vec(&input).unwrap();
        let prepared = PreparedRequest::from_slice(format, &bytes).unwrap();
        let request = Request::from_value(format, input).unwrap();
        let expected = convert_request(&ctx, format, &request).unwrap().value.into_value();
        let actual = convert_prepared_request(&ctx, format, &prepared).unwrap().value.to_value().unwrap();
        assert_eq!(actual, expected, "{format}");
    }
}
