//! Regression coverage for the RelayKit-backed gateway adapters.
use halolake_api_contract::{JsonValue, claude, openai};
use halolake_protocol::*;
use serde_json::json;

mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> openai::ChatMessage {
        openai::ChatMessage {
            role:                role.into(),
            content:             Some(openai::MessageContent::Text(content.into())),
            name:                None,
            tool_calls:          None,
            tool_call_id:        None,
            reasoning_content:   None,
            reasoning_signature: None,
        }
    }

    #[test]
    fn converts_openai_chat_to_claude_messages() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![
                openai::ChatMessage {
                    role:                "system".into(),
                    content:             Some(openai::MessageContent::Text(
                        "You are concise.".into(),
                    )),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
                openai::ChatMessage {
                    role:                "user".into(),
                    content:             Some(openai::MessageContent::Text("Hello".into())),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
            ],
            max_tokens:            Some(64),
            max_completion_tokens: None,
            temperature:           Some(0.2),
            top_p:                 None,
            top_k:                 None,
            stream:                Some(false),
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      None,
            reasoning:             None,
        };

        let claude = openai_chat_to_claude_messages(&req, "claude-sonnet-4").unwrap();
        assert_eq!(claude.model, "claude-sonnet-4");
        assert_eq!(
            claude
                .system
                .as_ref()
                .map(claude::SystemContent::as_text_lossy),
            Some("You are concise.".to_string())
        );
        assert_eq!(claude.messages.len(), 1);
    }

    #[test]
    fn normalizes_messages_like_new_api_claude_relay() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![
                msg("assistant", "first"),
                msg("user", "hello"),
                msg("user", "again"),
            ],
            max_tokens:            None,
            max_completion_tokens: None,
            temperature:           None,
            top_p:                 None,
            top_k:                 None,
            stream:                None,
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      None,
            reasoning:             None,
        };

        let claude = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert_eq!(claude.messages[0].role, "user");
        assert!(matches!(
            claude.messages[0].content[0],
            claude::ContentBlock::Text { ref text, .. } if text == "..."
        ));
        assert_eq!(claude.messages[2].role, "user");
        assert!(matches!(
            claude.messages[2].content[0],
            claude::ContentBlock::Text { ref text, .. } if text == "hello again"
        ));
    }

    #[test]
    fn converts_tool_calls_with_object_arguments_only() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".into(),
            messages:              vec![openai::ChatMessage {
                role:                "assistant".into(),
                content:             None,
                name:                None,
                tool_calls:          Some(vec![openai::ToolCall {
                    id:                "call_1".into(),
                    tool_type:         "function".into(),
                    function:          openai::ToolCallFunction {
                        name:      "lookup".into(),
                        arguments: "\"bad\"".into(),
                    },
                    index:             None,
                    thought_signature: None,
                }]),
                tool_call_id:        None,
                reasoning_content:   None,
                reasoning_signature: None,
            }],
            max_tokens:            None,
            max_completion_tokens: None,
            temperature:           None,
            top_p:                 None,
            top_k:                 None,
            stream:                None,
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      None,
            reasoning:             None,
        };

        let claude = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert!(matches!(
            claude.messages[1].content[0],
            claude::ContentBlock::ToolUse { ref input, .. } if input == &json!({})
        ));
    }

    #[test]
    fn maps_tool_choice_and_stop_reason_like_new_api() {
        let (request, _) = convert_request_json(
            relaykit::Format::OpenAi,
            relaykit::Format::Claude,
            json!({
                "messages": [{"role": "user", "content": "hello"}],
                "tools": [{"type": "function", "function": {"name": "lookup"}}],
                "tool_choice": "required",
                "parallel_tool_calls": false,
            }),
            "claude",
        )
        .unwrap();
        assert_eq!(
            request["tool_choice"],
            json!({"type": "any", "disable_parallel_tool_use": true})
        );
        let response = convert_response_json(
            relaykit::Format::Claude,
            relaykit::Format::OpenAi,
            json!({"content": [], "stop_reason": "refusal"}),
            "claude",
            &ResponsesToolMapping::default(),
        )
        .unwrap();
        assert_eq!(response["choices"][0]["finish_reason"], "content_filter");
    }

    #[test]
    fn preserves_multimodal_content_and_thinking_history() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt".into(),
            messages:              vec![
                openai::ChatMessage {
                    role:                "user".into(),
                    content:             Some(openai::MessageContent::Parts(vec![
                        openai::ContentPart::Text {
                            part_type: "text".into(),
                            text:      "look".into(),
                        },
                        openai::ContentPart::ImageUrl {
                            part_type: "image_url".into(),
                            image_url: openai::ImageUrl {
                                url:    "data:image/png;base64,AAAA".into(),
                                detail: Some("high".into()),
                            },
                        },
                    ])),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
                openai::ChatMessage {
                    role:                "user".into(),
                    content:             Some(openai::MessageContent::Text("again".into())),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   None,
                    reasoning_signature: None,
                },
                openai::ChatMessage {
                    role:                "assistant".into(),
                    content:             Some(openai::MessageContent::Text("answer".into())),
                    name:                None,
                    tool_calls:          None,
                    tool_call_id:        None,
                    reasoning_content:   Some("thought".into()),
                    reasoning_signature: Some("sig".into()),
                },
            ],
            max_tokens:            Some(64),
            max_completion_tokens: None,
            temperature:           None,
            top_p:                 None,
            top_k:                 None,
            stream:                None,
            tools:                 None,
            tool_choice:           None,
            parallel_tool_calls:   None,
            stop:                  None,
            n:                     None,
            seed:                  None,
            response_format:       None,
            stream_options:        None,
            reasoning_effort:      Some("high".into()),
            reasoning:             None,
        };

        let converted = openai_chat_to_claude_messages(&req, "claude").unwrap();
        assert!(matches!(
            converted.messages[0].content[1],
            claude::ContentBlock::Image { ref source, .. } if source.data.as_deref() == Some("AAAA")
        ));
        assert!(matches!(
            converted.messages[1].content[0],
            claude::ContentBlock::Thinking { ref thinking, signature: Some(ref sig), .. }
                if thinking == "thought" && sig == "sig"
        ));
        assert_eq!(converted.max_tokens, 11264);
    }

    #[test]
    fn claude_stream_keeps_parallel_tool_indices_and_signature() {
        let mut translator = ClaudeSseTranslator::new("requested");
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":null}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"why"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"one","input":{}}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_2","name":"two","input":{}}}),
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
        let chunks: Vec<JsonValue> = output
            .iter()
            .filter(|payload| payload.as_str() != "[DONE]")
            .map(|payload| serde_json::from_str(payload).unwrap())
            .collect();
        assert!(
            chunks
                .iter()
                .any(|chunk| { chunk["choices"][0]["delta"]["reasoning_signature"] == "sig" })
        );
        let indices: Vec<u64> = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"][0]["index"].as_u64())
            .collect();
        assert_eq!(indices, vec![0, 1]);
        assert_eq!(output.last().map(String::as_str), Some("[DONE]"));
    }

    #[test]
    fn stream_translators_finalize_on_eof() {
        let mut claude_to_chat = ClaudeSseTranslator::new("requested");
        claude_to_chat
            .translate_sse_payload(
                &json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"partial"}}).to_string(),
            )
            .unwrap();
        let chat_terminal = claude_to_chat.finish().unwrap();
        assert_eq!(chat_terminal.last().map(String::as_str), Some("[DONE]"));

        let mut chat_to_claude = OpenAiSseToClaudeTranslator::new("requested");
        chat_to_claude
            .translate_sse_payload(
                &json!({"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"upstream","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"stop"}]}).to_string(),
            )
            .unwrap();
        let claude_terminal = chat_to_claude.finish().unwrap();
        let parsed = claude_terminal
            .iter()
            .map(|event| serde_json::from_str::<JsonValue>(event).unwrap())
            .collect::<Vec<_>>();
        assert!(parsed.iter().any(|event| event["type"] == "message_delta"));
        assert_eq!(parsed.last().unwrap()["type"], "message_stop");
    }
}
