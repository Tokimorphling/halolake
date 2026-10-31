//! Regression coverage for the RelayKit-backed gateway adapters.
use halolake_api_contract::{JsonValue, gemini, openai};
use halolake_protocol::*;
use serde_json::json;
const GEMINI_DUMMY_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

mod tests {
    use super::*;

    fn image_req(size: &str, quality: &str, n: Option<u32>) -> openai::ImageRequest {
        openai::ImageRequest {
            model: "gpt-image-1".to_string(),
            prompt: "a lake under stars".to_string(),
            n,
            size: size.to_string(),
            quality: quality.to_string(),
            response_format: String::new(),
            style: None,
            user: None,
            extra_fields: None,
            background: None,
            moderation: None,
            output_format: None,
            output_compression: None,
            partial_images: None,
            stream: None,
            images: None,
            mask: None,
            input_fidelity: None,
            watermark: None,
            watermark_enabled: None,
            user_id: None,
            image: None,
        }
    }

    #[test]
    fn converts_openai_tool_call_to_gemini_function_call_like_new_api() {
        let req = openai::ChatCompletionRequest {
            model:                 "gpt-4o-mini".to_string(),
            messages:              vec![openai::ChatMessage {
                role:                "assistant".to_string(),
                content:             Some(openai::MessageContent::Text("".to_string())),
                name:                None,
                tool_calls:          Some(vec![openai::ToolCall {
                    id:                "call_1".to_string(),
                    tool_type:         "function".to_string(),
                    function:          openai::ToolCallFunction {
                        name:      "lookup".to_string(),
                        arguments: "{\"q\":\"rust\"}".to_string(),
                    },
                    index:             None,
                    thought_signature: Some("sig-openai".to_string()),
                }]),
                tool_call_id:        None,
                reasoning_content:   Some("private reasoning".to_string()),
                reasoning_signature: Some("sig-reasoning".to_string()),
            }],
            max_tokens:            Some(128),
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

        let gemini = openai_chat_to_gemini_request(&req).unwrap();
        let reasoning = &gemini.contents[0].parts[0];
        let call_part = &gemini.contents[0].parts[1];
        let call = call_part.function_call.as_ref().unwrap();
        assert_eq!(gemini.contents[0].role.as_deref(), Some("model"));
        assert_eq!(reasoning.text.as_deref(), Some("private reasoning"));
        assert_eq!(reasoning.thought, Some(true));
        assert_eq!(
            reasoning.thought_signature.as_deref(),
            Some("sig-reasoning")
        );
        assert_eq!(call.name, "lookup");
        assert_eq!(call.args, json!({"q": "rust"}));
        assert_eq!(call_part.thought_signature.as_deref(), Some("sig-openai"));
    }

    #[test]
    fn converts_gemini_function_call_to_openai_tool_calls_like_new_api() {
        let resp = gemini::GeminiChatResponse {
            candidates:      vec![gemini::GeminiCandidate {
                content:        gemini::GeminiChatContent {
                    role:  Some("model".to_string()),
                    parts: vec![gemini::GeminiPart {
                        function_call: Some(gemini::GeminiFunctionCall {
                            name: "lookup".to_string(),
                            args: json!({"q": "rust"}),
                        }),
                        thought_signature: Some("sig-gemini".to_string()),
                        ..gemini::GeminiPart::default()
                    }],
                },
                finish_reason:  Some("STOP".to_string()),
                index:          0,
                safety_ratings: Vec::new(),
            }],
            prompt_feedback: None,
            usage_metadata:  gemini::GeminiUsageMetadata {
                prompt_token_count:         3,
                candidates_token_count:     4,
                total_token_count:          7,
                thoughts_token_count:       0,
                cached_content_token_count: 0,
            },
        };

        let openai = gemini_response_to_openai_chat(resp, "gemini-2.5-flash");
        let tool_calls = openai.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].function.name, "lookup");
        assert_eq!(tool_calls[0].function.arguments, "{\"q\":\"rust\"}");
        assert_eq!(
            tool_calls[0].thought_signature.as_deref(),
            Some("sig-gemini")
        );
        assert_eq!(
            openai.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
        assert_eq!(openai.usage.unwrap().total_tokens, 7);
    }

    #[test]
    fn preserves_gemini_thought_text_and_signature_in_openai_message() {
        let resp = gemini::GeminiChatResponse {
            candidates: vec![gemini::GeminiCandidate {
                content:        gemini::GeminiChatContent {
                    role:  Some("model".to_string()),
                    parts: vec![
                        gemini::GeminiPart {
                            text: Some("inspect the inputs".to_string()),
                            thought: Some(true),
                            thought_signature: Some("thought-sig".to_string()),
                            ..gemini::GeminiPart::default()
                        },
                        gemini::GeminiPart::text("answer"),
                    ],
                },
                finish_reason:  Some("STOP".to_string()),
                index:          0,
                safety_ratings: Vec::new(),
            }],
            ..gemini::GeminiChatResponse::default()
        };

        let openai = gemini_response_to_openai_chat(resp, "gemini-2.5-flash");
        let message = &openai.choices[0].message;
        assert_eq!(
            message.reasoning_content.as_deref(),
            Some("inspect the inputs")
        );
        assert_eq!(message.reasoning_signature.as_deref(), Some("thought-sig"));
        assert_eq!(message.content.as_ref().unwrap().as_text_lossy(), "answer");
        assert_eq!(openai.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn cached_tokens_are_details_not_added_to_prompt_tokens() {
        let usage = gemini_response_to_openai_chat(
            gemini::GeminiChatResponse {
                usage_metadata: gemini::GeminiUsageMetadata {
                    prompt_token_count:         100,
                    candidates_token_count:     20,
                    total_token_count:          125,
                    thoughts_token_count:       5,
                    cached_content_token_count: 40,
                },
                ..Default::default()
            },
            "gemini",
        )
        .usage
        .unwrap();

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.total_tokens, 125);
        assert_eq!(usage.prompt_tokens_details.cached_tokens, 40);
        assert_eq!(usage.cached_tokens, 40);
    }

    #[test]
    fn stream_stop_after_tool_call_maps_to_tool_calls() {
        let mut translator = GeminiSseToOpenAiTranslator::new("gemini-2.5-flash");
        let tool_event = json!({
            "candidates": [{
                "index": 0,
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {"name": "lookup", "args": {"q": "rust"}},
                        "thoughtSignature": "tool-sig"
                    }]
                }
            }]
        })
        .to_string();
        let finish_event = json!({
            "candidates": [{
                "index": 0,
                "content": {"role": "model", "parts": []},
                "finishReason": "STOP"
            }]
        })
        .to_string();

        let tool_chunks = translator.translate_sse_payload(&tool_event).unwrap();
        let tool_chunk: JsonValue = serde_json::from_str(&tool_chunks[0]).unwrap();
        assert_eq!(
            tool_chunk["choices"][0]["delta"]["tool_calls"][0]["thoughtSignature"],
            "tool-sig"
        );

        let finish_chunks = translator.translate_sse_payload(&finish_event).unwrap();
        let finish_chunk: JsonValue = serde_json::from_str(&finish_chunks[0]).unwrap();
        assert_eq!(finish_chunk["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(finish_chunks.last().map(String::as_str), Some("[DONE]"));
    }

    #[test]
    fn stream_parallel_tool_calls_keep_distinct_ids_and_indices() {
        let mut translator = GeminiSseToOpenAiTranslator::new("gemini-2.5-flash");
        let event = json!({
            "candidates": [{
                "index": 0,
                "content": {
                    "role": "model",
                    "parts": [
                        {"functionCall": {"name": "first", "args": {"a": 1}}},
                        {"functionCall": {"name": "second", "args": {"b": 2}}}
                    ]
                }
            }]
        })
        .to_string();

        let chunks = translator.translate_sse_payload(&event).unwrap();
        let chunk: JsonValue = serde_json::from_str(&chunks[0]).unwrap();
        let calls = chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[1]["index"], 1);
        assert_ne!(calls[0]["id"], calls[1]["id"]);

        let next = json!({
            "candidates": [{
                "index": 0,
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "third", "args": {}}}]
                }
            }]
        })
        .to_string();
        let chunks = translator.translate_sse_payload(&next).unwrap();
        let chunk: JsonValue = serde_json::from_str(&chunks[0]).unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["tool_calls"][0]["index"], 2);
    }

    #[test]
    fn gemini_parallel_function_responses_match_calls_by_name() {
        let req = gemini::GeminiChatRequest {
            contents: vec![
                gemini::GeminiChatContent {
                    role:  Some("model".to_string()),
                    parts: vec![
                        gemini::GeminiPart {
                            function_call: Some(gemini::GeminiFunctionCall {
                                name: "first".to_string(),
                                args: json!({}),
                            }),
                            ..gemini::GeminiPart::default()
                        },
                        gemini::GeminiPart {
                            function_call: Some(gemini::GeminiFunctionCall {
                                name: "second".to_string(),
                                args: json!({}),
                            }),
                            ..gemini::GeminiPart::default()
                        },
                    ],
                },
                gemini::GeminiChatContent {
                    role:  Some("user".to_string()),
                    parts: vec![
                        gemini::GeminiPart {
                            function_response: Some(gemini::GeminiFunctionResponse {
                                name:     "first".to_string(),
                                response: json!({"ok": 1}),
                            }),
                            ..gemini::GeminiPart::default()
                        },
                        gemini::GeminiPart {
                            function_response: Some(gemini::GeminiFunctionResponse {
                                name:     "second".to_string(),
                                response: json!({"ok": 2}),
                            }),
                            ..gemini::GeminiPart::default()
                        },
                    ],
                },
            ],
            ..gemini::GeminiChatRequest::default()
        };

        let converted = gemini_request_to_openai_chat(&req, "gpt").unwrap();
        let tool_messages: Vec<_> = converted
            .messages
            .iter()
            .filter(|message| message.role == "tool")
            .collect();
        assert_eq!(tool_messages[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_messages[1].tool_call_id.as_deref(), Some("call_2"));
    }

    #[test]
    fn openai_stream_accumulates_partial_tool_arguments_and_injects_signature() {
        let mut translator = OpenAiSseToGeminiTranslator::new();
        let first = json!({
            "id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"gpt",
            "choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]},"finish_reason":null}]
        })
        .to_string();
        let second = json!({
            "id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"gpt",
            "choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"","type":"function","function":{"name":"","arguments":"1}"}}]},"finish_reason":"tool_calls"}]
        })
        .to_string();
        assert!(translator.translate_sse_payload(&first).unwrap().is_empty());
        let output = translator.translate_sse_payload(&second).unwrap();
        let response: JsonValue = serde_json::from_str(&output[0]).unwrap();
        assert_eq!(
            response["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
            json!({"q":1})
        );
        assert_eq!(
            response["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
            GEMINI_DUMMY_THOUGHT_SIGNATURE
        );
    }

    #[test]
    fn converts_openai_image_request_to_gemini_imagen_like_new_api() {
        let req = image_req("1024x1792", "hd", Some(3));

        let gemini = openai_image_to_gemini_imagen_request(&req, "imagen-4.0-generate-001")
            .expect("imagen model should convert");

        assert_eq!(gemini.instances[0].prompt, "a lake under stars");
        assert_eq!(gemini.parameters.sample_count, Some(3));
        assert_eq!(gemini.parameters.aspect_ratio, "9:16");
        assert_eq!(gemini.parameters.person_generation, "allow_adult");
        assert_eq!(gemini.parameters.image_size, "2K");
    }

    #[test]
    fn maps_openai_image_size_and_quality_to_gemini_defaults_like_new_api() {
        let req = image_req("3:2", "unknown", None);

        let gemini = openai_image_to_gemini_imagen_request(&req, "imagen-4.0-generate-001")
            .expect("imagen model should convert");

        assert_eq!(gemini.parameters.sample_count, Some(1));
        assert_eq!(gemini.parameters.aspect_ratio, "3:2");
        assert_eq!(gemini.parameters.image_size, "1K");
    }

    #[test]
    fn rejects_non_imagen_image_upstream_like_new_api() {
        let req = image_req("", "", None);

        let err = openai_image_to_gemini_imagen_request(&req, "gemini-2.5-flash")
            .expect_err("non-imagen model should be rejected");

        assert!(matches!(err, ProtocolError::UnsupportedImageModel));
    }

    #[test]
    fn converts_gemini_imagen_response_to_openai_image_response_like_new_api() {
        let resp = gemini::GeminiImageResponse {
            predictions: vec![
                gemini::GeminiImagePrediction {
                    bytes_base64_encoded: "image-1".to_string(),
                    ..gemini::GeminiImagePrediction::default()
                },
                gemini::GeminiImagePrediction {
                    bytes_base64_encoded: "filtered".to_string(),
                    rai_filtered_reason: "safety".to_string(),
                    ..gemini::GeminiImagePrediction::default()
                },
            ],
        };

        let openai = gemini_imagen_to_openai_image_response(resp, 123).unwrap();

        assert_eq!(openai.created, 123);
        assert_eq!(openai.data.len(), 1);
        assert_eq!(openai.data[0].b64_json, "image-1");
    }

    #[test]
    fn gemini_stream_terminates_on_non_stop_finish_reason() {
        let mut translator = GeminiSseToOpenAiTranslator::new("gemini-1.5-pro");
        let payload = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hi"}]},
                "finishReason": "MAX_TOKENS"
            }]
        })
        .to_string();

        let events = translator
            .translate_sse_payload(&payload)
            .expect("translate");
        // Gemini never sends a native [DONE]; a MAX_TOKENS finish must still
        // terminate the stream or the downstream client blocks until close.
        assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
        let chunk: serde_json::Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "length");

        // Further payloads after termination are ignored.
        assert!(
            translator
                .translate_sse_payload(&payload)
                .expect("translate")
                .is_empty()
        );
    }
}
