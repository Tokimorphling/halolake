use super::*;
use futures_util::StreamExt;

pub(crate) async fn buffered_claude_as_responses(
    mut upstream: Response<HttpBody>,
    requested_model: FastStr,
    include_encrypted_reasoning: bool,
    tool_mapping: halolake_protocol::ResponsesToolMapping,
) -> Response<GatewayBody> {
    let payload = match upstream.body_mut().to_ready().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Bytes::new(),
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("failed reading Claude response: {err}"),
            );
        }
    };
    let claude_response: claude::MessagesResponse = match serde_json::from_slice(&payload) {
        Ok(response) => response,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                &format!("invalid Claude response: {err}"),
            );
        }
    };
    let usage = claude_response.usage.map(ResponseUsage::from_claude);
    let mut response = json_response(
        StatusCode::OK,
        claude_messages_to_openai_responses_with_options_and_tools(
            claude_response,
            requested_model,
            include_encrypted_reasoning,
            &tool_mapping,
        ),
    );
    attach_response_usage(&mut response, usage);
    response
}

pub(crate) fn stream_claude_as_responses(
    upstream: Response<HttpBody>,
    requested_model: FastStr,
    include_encrypted_reasoning: bool,
    tool_mapping: halolake_protocol::ResponsesToolMapping,
) -> Response<GatewayBody> {
    let body = stream_body_from_async(move |mut sender| async move {
        let mut stream = HttpBodyStream::from(upstream.into_body());
        let mut decoder = SseBuffer::default();
        let mut translator = ClaudeSseToResponsesTranslator::with_options(
            requested_model,
            include_encrypted_reasoning,
            tool_mapping,
        );
        let mut translation_failed = false;

        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    let mut out = Vec::new();
                    for payload in decoder.push(&bytes) {
                        match translator.translate_sse_payload(&payload) {
                            Ok(events) => {
                                for event in events {
                                    write_responses_sse_event(&mut out, &event);
                                }
                            }
                            Err(err) => {
                                warn!(?err, "failed to translate Claude SSE event to Responses");
                                for event in translator.fail(
                                    "invalid_upstream_event",
                                    format!("invalid Anthropic SSE event: {err}"),
                                ) {
                                    write_responses_sse_event(&mut out, &event);
                                }
                                translation_failed = true;
                                break;
                            }
                        }
                    }
                    if !out.is_empty() {
                        sender.feed_data(Some(Bytes::from(out)));
                    }
                    if translation_failed {
                        break;
                    }
                }
                Err(err) => {
                    sender.feed_error(err);
                    return;
                }
            }
        }

        let mut out = Vec::new();
        for event in translator.finish() {
            write_responses_sse_event(&mut out, &event);
        }
        if !out.is_empty() {
            sender.feed_data(Some(Bytes::from(out)));
        }
        sender.feed_data(None);
    });

    let mut resp = response_builder(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|err| {
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &err.to_string(),
            )
        });
    resp
}

fn write_responses_sse_event(out: &mut Vec<u8>, event: &JsonValue) {
    let event_type = event
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("message");
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event_type.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    if let Ok(data) = serde_json::to_vec(event) {
        out.extend_from_slice(&data);
    }
    out.extend_from_slice(b"\n\n");
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
