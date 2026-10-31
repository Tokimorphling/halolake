use relaykit::{Context, Format, Request, Response, ResponseStreamState, StreamOptions};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
fn context() -> Context {
    let mut c = Context {
        channel_meta_attached: true,
        upstream_model_name: "upstream-model".into(),
        ..Context::default()
    };
    c.options.gemini.thinking_adapter_budget_tokens_percentage = 0.6;
    c.options.gemini.function_call_thought_signature_enabled = true;
    c.options.gemini.safety_setting = Some(Arc::new(|_| "OFF".into()));
    c.media.get_base64_data = Some(Arc::new(|_, _| Ok(("aGVsbG8=".into(), "image/png".into()))));
    c.media.decode_base64_file_data =
        Some(Arc::new(|_| Ok(("aGVsbG8=".into(), "image/png".into()))));
    c
}
fn normalize(v: &mut Value) {
    // Go exposes Responses stream events through an internal Type/Payload envelope; Rust returns the wire payload.
    if v.get("Type").is_some() && v.get("Payload").is_some() {
        *v = v["Payload"].take();
    }
    match v {
        Value::String(s) => {
            let bytes = s.as_bytes();
            let mut out = String::new();
            let mut start = 0;
            let mut i = 0;
            while i + 32 <= bytes.len() {
                if bytes[i..i + 32]
                    .iter()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
                {
                    out.push_str(&s[start..i]);
                    out.push_str("<uuid>");
                    i += 32;
                    start = i;
                } else {
                    i += 1
                }
            }
            out.push_str(&s[start..]);
            *s = out
        }
        Value::Array(a) => {
            for v in a {
                normalize(v)
            }
        }
        Value::Object(m) => {
            for (k, v) in m {
                if (k == "created" || k == "created_at")
                    && v.as_i64().is_some_and(|v| v >= 100_000_000)
                {
                    *v = json!(0)
                } else {
                    normalize(v)
                }
            }
        }
        _ => {}
    }
}
fn compare(kind: &str, from: Format, to: Format) {
    let inputs: Value = serde_json::from_str(include_str!("fixtures/inputs.json")).unwrap();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/fixtures/golden/{kind}/{from}_to_{to}.golden.json"
    ));
    let mut want: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let ctx = context();
    let mut actual = match kind {
        "request" => relaykit::convert_request(
            &ctx,
            to,
            &Request::from_value(from, inputs[kind][from.as_str()].clone()).unwrap(),
        )
        .unwrap()
        .value
        .into_value(),
        "response" => relaykit::convert_response(
            &ctx,
            to,
            &Response::from_value(from, inputs[kind][from.as_str()].clone()).unwrap(),
        )
        .unwrap()
        .value
        .into_value(),
        "stream" => {
            let mut state = ResponseStreamState::new(
                from,
                to,
                StreamOptions {
                    id: "stream_fixed".into(),
                    model: "stream-model".into(),
                    ..StreamOptions::default()
                },
            )
            .unwrap();
            let mut events = vec![];
            for input in inputs[kind][from.as_str()].as_array().unwrap() {
                // The Go golden helper deep-copies each typed DTO through JSON first.
                let copied = relaykit::wire::normalize_stream(from, input).unwrap();
                events.extend(state.convert_chunk(&ctx, &copied).unwrap());
            }
            events.extend(state.finalize(&ctx).unwrap());
            json!({"events":events})
        }
        _ => unreachable!(),
    };
    if kind == "stream" {
        want = json!({"events":want["events"]});
    }
    normalize(&mut actual);
    normalize(&mut want);
    if actual != want {
        let temp = std::env::temp_dir().join(format!("relaykit-{kind}-{from}-{to}-actual.json"));
        std::fs::write(&temp, serde_json::to_string_pretty(&actual).unwrap()).unwrap();
        panic!(
            "golden mismatch {kind}/{from}->{to}; actual at {} expected at {}",
            temp.display(),
            path.display()
        )
    }
}
macro_rules! check {
    ($name:ident,$kind:literal,$from:ident,$to:ident) => {
        #[test]
        fn $name() {
            compare($kind, Format::$from, Format::$to)
        }
    };
}
check!(req_chat_claude, "request", OpenAi, Claude);
check!(req_chat_responses, "request", OpenAi, OpenAiResponses);
check!(req_responses_chat, "request", OpenAiResponses, OpenAi);
check!(req_responses_claude, "request", OpenAiResponses, Claude);
check!(resp_chat_responses, "response", OpenAi, OpenAiResponses);
check!(resp_claude_chat, "response", Claude, OpenAi);
check!(resp_claude_responses, "response", Claude, OpenAiResponses);
check!(resp_gemini_chat, "response", Gemini, OpenAi);
check!(resp_gemini_responses, "response", Gemini, OpenAiResponses);
check!(resp_gemini_claude, "response", Gemini, Claude);
check!(resp_responses_chat, "response", OpenAiResponses, OpenAi);
check!(resp_responses_claude, "response", OpenAiResponses, Claude);
check!(stream_chat_claude, "stream", OpenAi, Claude);
check!(stream_chat_gemini, "stream", OpenAi, Gemini);
check!(stream_chat_responses, "stream", OpenAi, OpenAiResponses);
check!(stream_claude_chat, "stream", Claude, OpenAi);
check!(stream_claude_gemini, "stream", Claude, Gemini);
check!(stream_claude_responses, "stream", Claude, OpenAiResponses);
check!(stream_gemini_chat, "stream", Gemini, OpenAi);
check!(stream_gemini_claude, "stream", Gemini, Claude);
check!(stream_gemini_responses, "stream", Gemini, OpenAiResponses);
check!(stream_responses_chat, "stream", OpenAiResponses, OpenAi);
check!(stream_responses_claude, "stream", OpenAiResponses, Claude);
check!(stream_responses_gemini, "stream", OpenAiResponses, Gemini);
