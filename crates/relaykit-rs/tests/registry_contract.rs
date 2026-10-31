use relaykit::{Context, Format, Request, convert_request_by_id, convert_request_via};
use serde_json::json;
#[test]
fn explicit_routes_report_upstream_metadata_and_track_hops() {
    let ctx = Context::default();
    let req = Request::from_value(
        Format::OpenAi,
        json!({"model":"test","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}),
    )
    .unwrap();
    let result = convert_request_via(
        &ctx,
        &req,
        &[Format::OpenAi, Format::OpenAiResponses, Format::Claude],
    )
    .unwrap();
    assert_eq!(
        result.converter,
        "openai_chat_completions_to_openai_responses,openai_responses_to_claude_messages"
    );
    assert_eq!(result.quality, None);
    assert_eq!(
        *ctx.conversion_chain.lock().unwrap(),
        vec![Format::OpenAiResponses, Format::Claude]
    );
}
#[test]
fn response_aliases_are_not_request_converter_ids() {
    let req = Request::from_value(Format::OpenAi, json!({"model":"test","messages":[]})).unwrap();
    assert!(
        convert_request_by_id(&Context::default(), "oai_chat_to_oai_responses_resp", &req).is_err()
    );
}
#[test]
fn invalid_late_hop_is_validated_before_mutating_context() {
    let ctx = Context::default();
    let req = Request::from_value(
        Format::OpenAiResponses,
        json!({"model":"test","input":"hi","tools":[{"type":"custom","name":"shell"}]}),
    )
    .unwrap();
    assert!(convert_request_via(&ctx, &req, &[Format::Claude, Format::Gemini]).is_err());
    assert!(ctx.conversion_chain.lock().unwrap().is_empty());
    assert!(ctx.responses_tool_state().custom_tool_names.is_empty());
}
