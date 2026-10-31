//! Regression coverage for the RelayKit-backed gateway adapters.
use halolake_api_contract::{JsonValue, claude};
use halolake_protocol::*;
use serde_json::json;
const TOOL_NAME_MAX_LEN: usize = 64;

mod tests {
    use super::*;

    fn strip_cache_controls(value: &mut JsonValue) {
        match value {
            JsonValue::Object(object) => {
                object.remove("cache_control");
                for child in object.values_mut() {
                    strip_cache_controls(child);
                }
            }
            JsonValue::Array(values) => {
                for child in values {
                    strip_cache_controls(child);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn converts_typed_history_images_reasoning_and_tools() {
        let request = json!({
            "model":"gpt-5",
            "instructions":"system prompt",
            "input":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"thought"}],"encrypted_content":"sig"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"calling"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"},
                {"type":"function_call_output","call_id":"call_1","output":[{"type":"input_text","text":"ok"}]},
                {"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}
            ],
            "tools":[{"type":"function","name":"lookup","parameters":{"properties":{"q":{"type":"number"}}}}],
            "reasoning":{"effort":"xhigh"},
            "stream":true
        });
        let converted = openai_responses_to_claude_messages_with_cache_mode(
            &serde_json::to_vec(&request).unwrap(),
            "claude-sonnet",
            PromptCacheMode::Automatic,
        )
        .unwrap();
        assert_eq!(converted.model, "claude-sonnet");
        let system = converted.system.as_ref().unwrap();
        assert_eq!(system.as_text_lossy(), "system prompt");
        assert!(matches!(
            system,
            claude::SystemContent::Blocks(blocks)
                if blocks.len() == 1
                    && blocks[0].extra.get("cache_control")
                        == Some(&json!({"type":"ephemeral"}))
        ));
        assert_eq!(converted.cache_control, Some(json!({"type":"ephemeral"})));
        assert_eq!(
            converted
                .tools
                .as_ref()
                .and_then(|tools| tools.last())
                .and_then(|tool| tool.cache_control.as_ref()),
            Some(&json!({"type":"ephemeral"}))
        );
        let wire = serde_json::to_value(&converted).unwrap();
        assert_eq!(wire["cache_control"], json!({"type":"ephemeral"}));
        assert_eq!(
            wire["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            wire["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            converted.output_config.unwrap().effort.as_deref(),
            Some("max")
        );
        assert_eq!(converted.thinking.unwrap().budget_tokens, Some(32768));
        assert_eq!(converted.max_tokens, 64_000);
        assert!(converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::Thinking { signature: Some(sig), .. } if sig == "sig")
        )));
        assert!(converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::Image { source, .. } if source.data.as_deref() == Some("AAAA"))
        )));
    }

    #[test]
    fn normalizes_codex_reasoning_aliases_for_anthropic() {
        for (effort, expected, thinking) in [
            ("ultra", "max", true),
            ("minimal", "low", false),
            ("none", "low", false),
        ] {
            let request = json!({
                "input":"hello",
                "reasoning":{"effort":effort}
            });
            let converted = openai_responses_to_claude_messages(
                &serde_json::to_vec(&request).unwrap(),
                "claude-sonnet",
            )
            .unwrap();
            assert_eq!(
                converted.output_config.unwrap().effort.as_deref(),
                Some(expected)
            );
            assert_eq!(converted.thinking.is_some(), thinking);
        }
    }

    #[test]
    fn enables_automatic_prompt_cache_without_system_or_tools() {
        let request = json!({"input":"hello"});
        let converted = openai_responses_to_claude_messages_with_cache_mode(
            &serde_json::to_vec(&request).unwrap(),
            "claude",
            PromptCacheMode::Automatic,
        )
        .unwrap();

        assert_eq!(converted.cache_control, Some(json!({"type":"ephemeral"})));
        assert!(converted.system.is_none());
        assert!(converted.tools.is_none());
    }

    #[test]
    fn content_blocks_round_trip_cache_control_and_ttl() {
        let raw = json!({
            "type":"text",
            "text":"stable context",
            "cache_control":{"type":"ephemeral","ttl":"1h"}
        });
        let block: claude::ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(block).unwrap(), raw);
    }

    #[test]
    fn explicit_cache_mode_preserves_requested_ttl() {
        let request = json!({
            "cache_control":{"type":"ephemeral","ttl":"1h"},
            "input":"hello"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert!(converted.cache_control.is_none());
        let wire = serde_json::to_value(converted).unwrap();
        assert_eq!(
            wire["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
    }

    #[test]
    fn keeps_mid_conversation_developer_item_out_of_system_prefix() {
        let request = json!({
            "instructions":"initial instructions",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"mid developer"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(
            converted.system.as_ref().unwrap().as_text_lossy(),
            "initial instructions"
        );
        let wire = serde_json::to_value(converted).unwrap();
        assert_eq!(wire["messages"][2]["content"][0]["text"], "mid developer");
    }

    #[test]
    fn keeps_stable_prefix_when_responses_history_is_appended() {
        let base = json!({
            "instructions":"stable instructions",
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer 1"}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer 2"}]}
            ]
        });
        let mut next = base.clone();
        next["input"].as_array_mut().unwrap().push(json!({
            "type":"message",
            "role":"user",
            "content":[{"type":"input_text","text":"follow-up"}]
        }));

        let first =
            openai_responses_to_claude_messages(&serde_json::to_vec(&base).unwrap(), "claude")
                .unwrap();
        let second =
            openai_responses_to_claude_messages(&serde_json::to_vec(&next).unwrap(), "claude")
                .unwrap();
        let first_wire = serde_json::to_value(first).unwrap();
        let second_wire = serde_json::to_value(second).unwrap();

        assert_eq!(first_wire["cache_control"], second_wire["cache_control"]);
        assert_eq!(first_wire["system"], second_wire["system"]);
        assert_eq!(first_wire["tools"], second_wire["tools"]);
        assert_eq!(
            first_wire["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(first_wire["cache_control"].is_null());
        assert_eq!(
            first_wire["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            first_wire["tools"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(
            second_wire["messages"][2]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        let mut first_without_markers = first_wire.clone();
        let mut second_without_markers = second_wire.clone();
        strip_cache_controls(&mut first_without_markers);
        strip_cache_controls(&mut second_without_markers);
        let first_messages = first_without_markers["messages"].as_array().unwrap();
        let second_messages = second_without_markers["messages"].as_array().unwrap();
        assert_eq!(
            &first_messages[..],
            &second_messages[..first_messages.len()]
        );
        let first_messages = first_wire["messages"].as_array().unwrap();
        let second_messages = second_wire["messages"].as_array().unwrap();
        assert_eq!(
            first_messages[3]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(
            second_messages[3]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn marks_text_breakpoints_in_short_growing_history() {
        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"first"},
                    {"type":"input_image","image_url":"data:image/png;base64,AAAA"}
                ]},
                {"type":"message","role":"assistant","content":[
                    {"type":"output_text","text":"answer"}
                ]},
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"follow-up"}
                ]}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let wire = serde_json::to_value(converted).unwrap();

        assert_eq!(
            wire["messages"][0]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert!(
            wire["messages"][0]["content"][1]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(
            wire["messages"][2]["content"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }

    #[test]
    fn deduplicates_identical_additional_tools_without_reordering() {
        let lookup = json!({
            "type":"function",
            "name":"lookup",
            "description":"stable",
            "parameters":{"type":"object","properties":{}}
        });
        let fetch = json!({
            "type":"function",
            "name":"fetch",
            "parameters":{"type":"object","properties":{}}
        });
        let request = json!({
            "tools":[lookup.clone()],
            "input":[
                {"type":"additional_tools","tools":[lookup.clone(), fetch.clone()]},
                {"type":"additional_tools","tools":[lookup, fetch]},
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"go"}
                ]}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let names = converted
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["lookup", "fetch"]);
    }

    #[test]
    fn groups_parallel_calls_with_their_results() {
        let request = json!({
            "input":[
                {"type":"function_call","call_id":"call_1","name":"one","arguments":"{}"},
                {"type":"function_call","call_id":"call_2","name":"two","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"a"},
                {"type":"function_call_output","call_id":"call_2","output":"b"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(converted.messages.len(), 2);
        assert_eq!(converted.messages[0].role, "assistant");
        assert_eq!(converted.messages[1].role, "user");
        assert_eq!(converted.messages[0].content.len(), 2);
        assert_eq!(converted.messages[1].content.len(), 2);
    }

    #[test]
    fn drops_unanswered_parallel_siblings_and_orphan_results() {
        let request = json!({
            "input":[
                {"type":"function_call","call_id":"call_1","name":"one","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"a"},
                {"type":"function_call","call_id":"call_2","name":"two","arguments":"{}"},
                {"type":"function_call_output","call_id":"orphan","output":"ignored"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert!(converted.messages.iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, claude::ContentBlock::ToolUse { id, .. } if id == "call_1"),
            )
        }));
        assert!(!converted.messages.iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, claude::ContentBlock::ToolUse { id, .. } if id == "call_2"),
            )
        }));
        assert!(!converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_orphan")
        )));
    }

    #[test]
    fn drops_dangling_tool_call_when_no_results_exist() {
        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"start"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(converted.messages.len(), 1);
        assert_eq!(converted.messages[0].role, "user");
        assert!(!converted.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, claude::ContentBlock::ToolUse { .. }))
        }));
    }

    #[test]
    fn rejects_stateful_reference_and_maps_parallel_tool_policy() {
        let request = json!({
            "input":"hello",
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],
            "parallel_tool_calls":false
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(
            converted
                .tool_choice
                .as_ref()
                .and_then(|choice| choice.get("disable_parallel_tool_use")),
            Some(&JsonValue::Bool(true))
        );

        let request = json!({"previous_response_id":"resp_1","input":"continue"});
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(matches!(error, ProtocolError::UnsupportedRequest(_)));
    }

    #[test]
    fn rejects_malformed_tool_arguments_and_unsupported_content() {
        let malformed = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"not-json"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&malformed).unwrap(), "claude")
                .unwrap_err();
        assert!(matches!(error, ProtocolError::UnsupportedRequest(_)));

        let unsupported = json!({
            "input":[{"type":"message","role":"user","content":[
                {"type":"input_text","text":"keep"},
                {"type":"input_audio","data":"AAAA","format":"wav"}
            ]}]
        });
        let error = openai_responses_to_claude_messages(
            &serde_json::to_vec(&unsupported).unwrap(),
            "claude",
        )
        .unwrap_err();
        assert!(matches!(error, ProtocolError::UnsupportedRequest(_)));
    }

    #[test]
    fn stream_keeps_requested_model_and_surfaces_claude_error() {
        let mut translator = ClaudeSseToResponsesTranslator::new("requested-model");
        let created = translator
            .translate_sse_payload(
                &json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"upstream-model","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}).to_string(),
            )
            .unwrap();
        assert_eq!(created[0]["response"]["model"], "requested-model");

        let events = translator
            .translate_sse_payload(
                &json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})
                    .to_string(),
            )
            .unwrap();
        assert_eq!(events[0]["type"], "error");
        assert_eq!(events[0]["code"], "overloaded_error");
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "response.failed");
        assert_eq!(terminal["response"]["status"], "failed");
        assert_eq!(terminal["response"]["model"], "requested-model");
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn explicit_translation_failure_emits_failed_terminal() {
        let mut translator = ClaudeSseToResponsesTranslator::new("requested-model");
        let events = translator.fail("invalid_upstream_event", "bad event");
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[1]["type"], "error");
        assert_eq!(events[1]["code"], "invalid_upstream_event");
        assert_eq!(events[2]["type"], "response.failed");
        assert_eq!(events[2]["response"]["status"], "failed");
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn converts_buffered_thinking_signature_and_usage() {
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![
                claude::ContentBlock::Thinking {
                    thinking:      "why".to_string(),
                    signature:     Some("cipher".to_string()),
                    cache_control: None,
                },
                claude::ContentBlock::ToolUse {
                    id:            "toolu_1".to_string(),
                    name:          "lookup".to_string(),
                    input:         json!({"q":1}),
                    cache_control: None,
                },
            ],
            stop_reason:   Some("tool_use".to_string()),
            usage:         Some(claude::Usage {
                input_tokens:                10,
                output_tokens:               4,
                cache_creation_input_tokens: 2,
                cache_read_input_tokens:     3,
            }),
        };
        let converted = claude_messages_to_openai_responses(response, "requested");
        assert_eq!(converted["output"][0]["encrypted_content"], "cipher");
        assert_eq!(converted["output"][1]["call_id"], "toolu_1");
        assert_eq!(converted["usage"]["input_tokens"], 15);
    }

    #[test]
    fn omits_encrypted_reasoning_when_not_requested() {
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![claude::ContentBlock::Thinking {
                thinking:      "why".to_string(),
                signature:     Some("cipher".to_string()),
                cache_control: None,
            }],
            stop_reason:   Some("end_turn".to_string()),
            usage:         None,
        };
        let converted =
            claude_messages_to_openai_responses_with_options(response, "requested", false);
        assert!(converted["output"][0].get("encrypted_content").is_none());
    }

    #[test]
    fn buffered_output_keeps_claude_block_order() {
        let response = claude::MessagesResponse {
            id:            "msg_order".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![
                claude::ContentBlock::Text {
                    text:          "before".to_string(),
                    cache_control: None,
                },
                claude::ContentBlock::ToolUse {
                    id:            "toolu_1".to_string(),
                    name:          "lookup".to_string(),
                    input:         json!({}),
                    cache_control: None,
                },
                claude::ContentBlock::Text {
                    text:          "after".to_string(),
                    cache_control: None,
                },
            ],
            stop_reason:   Some("end_turn".to_string()),
            usage:         None,
        };
        let converted = claude_messages_to_openai_responses(response, "requested");
        assert_eq!(converted["output"][0]["type"], "message");
        assert_eq!(converted["output"][1]["type"], "function_call");
        assert_eq!(converted["output"][2]["type"], "message");
    }

    #[test]
    fn stream_emits_signature_on_reasoning_item_and_terminal_output() {
        let mut translator = ClaudeSseToResponsesTranslator::new("claude");
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":null}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let mut output = Vec::new();
        for event in events {
            output.extend(
                translator
                    .translate_sse_payload(&event.to_string())
                    .unwrap(),
            );
        }
        let item_done = output
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["encrypted_content"], "sig");
        let terminal = output.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(
            terminal["response"]["output"][0]["summary"][0]["text"],
            "why"
        );
    }

    #[test]
    fn namespace_declaration_flattens_to_function_tools() {
        let request = json!({
            "model":"gpt-5",
            "tools":[
                {"type":"function","name":"plain","description":"keep"},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","description":"spawn","parameters":{"type":"object"}}
                ]}
            ],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "plain");
        assert_eq!(tools[1].name, "collaboration__spawn_agent");
        assert_eq!(tools[1].description.as_deref(), Some("spawn"));
    }

    #[test]
    fn namespace_qualified_history_and_choice_rewrite_to_flat_name() {
        let request = json!({
            "tools":[{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
            ]}],
            "tool_choice":{"type":"function","name":"spawn_agent","namespace":"collaboration"},
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]},
                {"type":"function_call","call_id":"call_1","name":"spawn_agent","namespace":"collaboration","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(
            converted.tool_choice.as_ref().unwrap()["name"],
            "collaboration__spawn_agent"
        );
        assert!(
            converted
                .tool_choice
                .as_ref()
                .unwrap()
                .get("namespace")
                .is_none()
        );
        assert!(converted.messages.iter().any(|message| message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::ToolUse { name, .. } if name == "collaboration__spawn_agent")
        )));
    }

    #[test]
    fn namespace_group_choice_falls_back_to_auto() {
        let request = json!({
            "tools":[{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}},
                {"type":"function","name":"send_message","parameters":{"type":"object"}}
            ]}],
            "tool_choice":{"type":"namespace","name":"collaboration"},
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert_eq!(converted.tool_choice.as_ref().unwrap()["type"], "auto");
    }

    #[test]
    fn namespace_rejects_top_level_flat_name_collision() {
        let request = json!({
            "tools":[
                {"type":"function","name":"collaboration__spawn_agent"},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent"}
                ]}
            ],
            "input":"hi"
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(message) if message.contains("conflicts with a top-level tool"))
        );
    }

    #[test]
    fn namespace_rejects_cross_namespace_collision() {
        let request = json!({
            "tools":[
                {"type":"namespace","name":"a","tools":[{"type":"function","name":"b__c"}]},
                {"type":"namespace","name":"a__b","tools":[{"type":"function","name":"c"}]}
            ],
            "input":"hi"
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(message) if message.contains("both flatten"))
        );
    }

    #[test]
    fn namespace_rejects_duplicate_equivalent_declaration() {
        let request = json!({
            "tools":[
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]}
            ],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "collaboration__spawn_agent");
    }

    #[test]
    fn namespace_buffered_response_restores_name_and_namespace() {
        let mapping = responses_tool_mapping(
            &serde_json::to_vec(&json!({
                "tools":[{"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]}],
                "input":"hi"
            }))
            .unwrap(),
        )
        .unwrap();
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![claude::ContentBlock::ToolUse {
                id:            "toolu_1".to_string(),
                name:          "collaboration__spawn_agent".to_string(),
                input:         json!({}),
                cache_control: None,
            }],
            stop_reason:   Some("tool_use".to_string()),
            usage:         None,
        };
        let converted = claude_messages_to_openai_responses_with_options_and_tools(
            response,
            "requested",
            true,
            &mapping,
        );
        assert_eq!(converted["output"][0]["type"], "function_call");
        assert_eq!(converted["output"][0]["name"], "spawn_agent");
        assert_eq!(converted["output"][0]["namespace"], "collaboration");
    }

    #[test]
    fn namespace_sse_response_restores_identity_on_done_and_terminal() {
        let mapping = responses_tool_mapping(
            &serde_json::to_vec(&json!({
                "tools":[{"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]}],
                "input":"hi"
            }))
            .unwrap(),
        )
        .unwrap();
        let mut translator = ClaudeSseToResponsesTranslator::with_options("claude", true, mapping);
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"collaboration__spawn_agent","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let mut output = Vec::new();
        for event in events {
            output.extend(
                translator
                    .translate_sse_payload(&event.to_string())
                    .unwrap(),
            );
        }
        let item_done = output
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["type"], "function_call");
        assert_eq!(item_done["item"]["name"], "spawn_agent");
        assert_eq!(item_done["item"]["namespace"], "collaboration");
        let terminal = output.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(terminal["response"]["output"][0]["name"], "spawn_agent");
        assert_eq!(
            terminal["response"]["output"][0]["namespace"],
            "collaboration"
        );
    }

    #[test]
    fn namespace_unknown_qualified_history_is_left_untouched() {
        let request = json!({
            "tools":[{"type":"namespace","name":"collaboration","tools":[
                {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
            ]}],
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]},
                {"type":"function_call","call_id":"call_1","name":"other","namespace":"unknown","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"}
            ]
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        assert!(converted.messages.iter().any(|message| {
            message.content.iter().any(
            |block| matches!(block, claude::ContentBlock::ToolUse { name, .. } if name == "other")
        )
        }));
    }

    #[test]
    fn namespace_long_name_uses_stable_hash_suffix() {
        let long_namespace = "a".repeat(40);
        let long_name = "b".repeat(40);
        let request = json!({
            "tools":[{"type":"namespace","name":long_namespace,"tools":[
                {"type":"function","name":long_name,"parameters":{"type":"object"}}
            ]}],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].name.len() <= TOOL_NAME_MAX_LEN);
        assert!(tools[0].name.contains("__"));
        assert!(tools[0].name.ends_with(|c: char| c.is_ascii_hexdigit()));
    }

    #[test]
    fn rejects_unsupported_server_tools_at_request_stage() {
        for tool_type in [
            "file_search",
            "computer",
            "image_generation",
            "code_interpreter",
        ] {
            let request = json!({
                "tools":[{"type":tool_type,"name":"x"}],
                "input":"hi"
            });
            let error = openai_responses_to_claude_messages(
                &serde_json::to_vec(&request).unwrap(),
                "claude",
            )
            .unwrap_err();
            assert!(
                matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains(tool_type)),
                "{tool_type} should be rejected"
            );
        }
    }

    #[test]
    fn web_search_declaration_maps_to_anthropic_web_search_tool() {
        let request = json!({
            "tools":[{"type":"web_search"}],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type.as_deref(), Some("web_search_20250305"));
        assert_eq!(tools[0].name, "web_search");
    }

    #[test]
    fn sse_server_tool_accumulates_input_json_delta_fragments() {
        let mut translator = ClaudeSseToResponsesTranslator::new("claude");
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvu_1","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"quer"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"y\":\"rust\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let mut output = Vec::new();
        for event in events {
            output.extend(
                translator
                    .translate_sse_payload(&event.to_string())
                    .unwrap(),
            );
        }
        let item_done = output
            .iter()
            .find(|event| event["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item_done["item"]["type"], "web_search_call");
        assert_eq!(item_done["item"]["action"]["query"], "rust");
        let terminal = output.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        assert_eq!(terminal["response"]["output"][0]["action"]["query"], "rust");
    }

    #[test]
    fn buffered_server_tool_use_maps_to_web_search_call() {
        let response = claude::MessagesResponse {
            id:            "msg_1".to_string(),
            response_type: "message".to_string(),
            role:          "assistant".to_string(),
            model:         "claude".to_string(),
            content:       vec![claude::ContentBlock::ServerToolUse {
                id:            "srvu_1".to_string(),
                name:          "web_search".to_string(),
                input:         json!({"query":"halolake"}),
                cache_control: None,
            }],
            stop_reason:   Some("end_turn".to_string()),
            usage:         None,
        };
        let converted = claude_messages_to_openai_responses(response, "requested");
        assert_eq!(converted["output"][0]["type"], "web_search_call");
        assert_eq!(converted["output"][0]["action"]["query"], "halolake");
    }

    #[test]
    fn rejects_item_reference_and_unknown_input_item_types() {
        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"item_reference","id":"msg_abc"}
            ]
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains("item_reference"))
        );

        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"unknown_future_type","content":"x"}
            ]
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains("unknown_future_type"))
        );
    }

    #[test]
    fn previous_response_id_rejected_without_requesting_upstream() {
        let request = json!({
            "previous_response_id":"resp_123",
            "input":"continue"
        });
        let error =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap_err();
        assert!(
            matches!(error, ProtocolError::UnsupportedRequest(msg) if msg.contains("previous_response_id"))
        );
    }

    #[test]
    fn custom_tool_input_restoration_mirrors_sub2api_fallback() {
        // Mirrors sub2api extractCustomToolCallInput test cases:
        //   {"input":"dir"}          → "dir"
        //   {"input":<non-string>}   → original arguments verbatim
        //   {}                       → ""
        //   {"other":"x"}            → original arguments verbatim
        //   non-JSON / null          → ""
        let mapping = ResponsesToolMapping {
            custom_tool_names: ["lookup".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let mk = |input: &str| -> JsonValue {
            let response = claude::MessagesResponse {
                id:            "msg".to_string(),
                response_type: "message".to_string(),
                role:          "assistant".to_string(),
                model:         "claude".to_string(),
                content:       vec![claude::ContentBlock::ToolUse {
                    id:            "toolu_1".to_string(),
                    name:          "lookup".to_string(),
                    input:         serde_json::from_str(input).unwrap_or(json!({})),
                    cache_control: None,
                }],
                stop_reason:   Some("end_turn".to_string()),
                usage:         None,
            };
            let converted = claude_messages_to_openai_responses_with_options_and_tools(
                response,
                "requested",
                true,
                &mapping,
            );
            converted["output"][0]["input"].clone()
        };
        assert_eq!(mk(r#"{"input":"dir"}"#), "dir");
        assert_eq!(mk(r#"{"input":42}"#), r#"{"input":42}"#);
        assert_eq!(mk(r#"{}"#), "");
        assert_eq!(mk(r#"{"other":"x"}"#), r#"{"other":"x"}"#);
        assert_eq!(mk(r#"null"#), "");
    }

    #[test]
    fn custom_tool_always_uses_input_string_schema() {
        // sub2api forces the {"input":string} schema for custom tools even
        // when the caller supplies its own parameters.
        let request = json!({
            "model":"gpt-5",
            "tools":[{"type":"custom","name":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}],
            "input":"hi"
        });
        let converted =
            openai_responses_to_claude_messages(&serde_json::to_vec(&request).unwrap(), "claude")
                .unwrap();
        let tools = converted.tools.unwrap();
        assert_eq!(tools[0].name, "lookup");
        let schema = &tools[0].input_schema;
        assert_eq!(schema["properties"]["input"]["type"], "string");
        assert_eq!(schema["required"][0], "input");
    }
}
