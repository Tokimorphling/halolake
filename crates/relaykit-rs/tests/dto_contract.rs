use relaykit::{Format, dto, wire};
use serde_json::{Value, json};
#[test]
fn raw_null_zero_and_nil_have_distinct_wire_shapes() {
    let request = dto::GeneralOpenAIRequest {
        model: "gpt-test".into(),
        stream: Some(false),
        temperature: Some(0.),
        messages: Some(vec![dto::Message {
            role: "user".into(),
            content: Value::Null,
            ..Default::default()
        }]),
        ..Default::default()
    };
    let v = serde_json::to_value(request).unwrap();
    assert_eq!(v["stream"], false);
    assert_eq!(v["temperature"], 0.0);
    assert!(v["messages"][0].get("content").unwrap().is_null());
    assert!(v.get("top_p").is_none());
    let request: dto::OpenAIResponsesRequest =
        serde_json::from_value(json!({"model":"gpt-test","input":null})).unwrap();
    // RawMessage null is distinct from an absent field on the dynamic API.
    assert_eq!(
        wire::normalize_request(
            Format::OpenAiResponses,
            &json!({"model":"gpt-test","input":null})
        )
        .unwrap()["input"],
        Value::Null
    );
    assert_eq!(request.model, "gpt-test");
    assert_eq!(
        serde_json::to_value(request).unwrap().get("input"),
        Some(&Value::Null)
    );
}
#[test]
fn special_response_items_do_not_leak_message_fields() {
    let item = dto::ResponsesOutput {
        r#type: "web_search_call".into(),
        id: "ws_1".into(),
        status: "completed".into(),
        action: Some(json!({"type":"search","query":"rust"})),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(item).unwrap(),
        json!({"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"rust"}})
    );
}
#[test]
fn native_tools_are_emitted_without_function_wrapper() {
    let tool = dto::ToolCallRequest {
        native: Some(json!({"type":"web_search","name":"search"})),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(tool).unwrap(),
        json!({"type":"web_search","name":"search"})
    );
}
#[test]
fn gemini_alias_precedence_matches_go() {
    let v = json!({"contents":[],"systemInstruction":{"parts":[{"text":"old"}]},"system_instruction":{"parts":[{"text":"new"}]},"generationConfig":{"topP":0.9,"top_p":0.0,"thinking_config":{"includeThoughts":true,"include_thoughts":false}}});
    let dto: dto::GeminiChatRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(dto).unwrap(),
        wire::normalize_request(Format::Gemini, &v).unwrap()
    );
}
#[test]
fn flexible_scalar_values_match_upstream() {
    assert_eq!(
        serde_json::from_value::<dto::IntValue>(json!("123"))
            .unwrap()
            .0,
        123
    );
    assert_eq!(
        serde_json::from_value::<dto::IntValue>(json!(12.9))
            .unwrap()
            .0,
        12
    );
    assert_eq!(
        serde_json::from_value::<dto::StringValue>(json!(12))
            .unwrap()
            .0,
        "12"
    );
    assert!(
        !serde_json::from_value::<dto::BoolValue>(json!("false"))
            .unwrap()
            .0
    );
    assert!(serde_json::from_value::<dto::BoolValue>(json!("TRUE")).is_err());
}
