use super::*;
use halolake_protocol::relaykit::Format;

pub(crate) async fn buffered_claude_as_responses(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
    include_encrypted_reasoning: bool,
    tool_mapping: halolake_protocol::ResponsesToolMapping,
) -> Response<GatewayBody> {
    buffered_protocol_response(
        upstream,
        Format::Claude,
        Format::OpenAiResponses,
        requested_model,
        tool_mapping,
        Default::default(),
        include_encrypted_reasoning,
    )
    .await
}

pub(crate) fn stream_claude_as_responses(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
    include_encrypted_reasoning: bool,
    tool_mapping: halolake_protocol::ResponsesToolMapping,
) -> Response<GatewayBody> {
    stream_protocol_response(
        upstream,
        Format::Claude,
        Format::OpenAiResponses,
        requested_model,
        tool_mapping,
        Default::default(),
        include_encrypted_reasoning,
    )
}

#[cfg(test)]
fn write_responses_sse_event(out: &mut Vec<u8>, event: &JsonValue) {
    write_protocol_sse_event(out, Format::OpenAiResponses, event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_typed_responses_sse_frame() {
        let mut output = Vec::new();
        write_responses_sse_event(
            &mut output,
            &serde_json::json!({"type":"response.completed","sequence_number":1}),
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("event: response.completed\ndata: "));
        assert!(output.ends_with("\n\n"));
    }
}
