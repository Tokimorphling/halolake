use relaykit::{Format, wire};
use serde_json::{Value, json};

#[test]
fn owned_normalization_keeps_alias_pointer_raw_and_any_contracts() {
    let input = json!({
        "generationConfig": {
            "topP": 0.9, "top_p": 0.0,
            "thinkingConfig": {"include_thoughts": false, "thinking_budget": 0}
        },
        "system_instruction": {"parts": [{"text":"system"}]},
        "contents": [{"parts": [{"functionCall": {"name":"f", "args": {"n":9007199254740993_u64}}}]}]
    });
    let normalized = wire::normalize_request_owned(Format::Gemini, input).unwrap();
    assert_eq!(normalized["generationConfig"]["topP"], json!(0.0));
    assert_eq!(
        normalized["generationConfig"]["thinkingConfig"]["includeThoughts"],
        json!(false)
    );
    assert_eq!(
        normalized["contents"][0]["parts"][0]["functionCall"]["args"]["n"],
        json!(9007199254740992_u64)
    );
    assert_eq!(
        normalized["systemInstruction"]["parts"][0]["text"],
        "system"
    );
    let raw = json!({"model":"m", "input":null, "metadata":{"n":9007199254740993_u64}});
    let normalized = wire::normalize_request_owned(Format::OpenAiResponses, raw).unwrap();
    assert!(normalized.as_object().unwrap().contains_key("input"));
    assert_eq!(normalized["input"], Value::Null);
    assert_eq!(normalized["metadata"]["n"], json!(9007199254740993_u64));
}

#[test]
fn validation_and_owned_normalization_reject_invalid_nested_fields() {
    for value in [
        json!({"messages":[{"role":false}]}),
        json!({"messages":{}}),
        json!({"max_tokens":"1"}),
        json!({"temperature":[]}),
    ] {
        let error = wire::normalize_request(Format::OpenAi, &value)
            .unwrap_err()
            .to_string();
        assert_eq!(
            wire::validate_request(Format::OpenAi, &value)
                .unwrap_err()
                .to_string(),
            error
        );
        assert_eq!(
            wire::normalize_request_owned(Format::OpenAi, value)
                .unwrap_err()
                .to_string(),
            error
        );
    }
}

#[test]
fn exact_key_precedence_survives_owned_field_removal() {
    let input = json!({"MODEL":"uppercase", "Model":"mixed", "model":"canonical",
        "MAX_TOKENS":1, "max_tokens":2, "thinking_budget":0});
    let normalized = wire::normalize_request_owned(Format::OpenAi, input).unwrap();
    assert_eq!(normalized["model"], "canonical");
    assert_eq!(normalized["max_tokens"], 2);
    assert!(
        !normalized
            .as_object()
            .unwrap()
            .contains_key("thinking_budget")
    );
}
