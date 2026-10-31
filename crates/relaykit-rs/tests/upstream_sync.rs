//! Fixed Go oracle fixtures for the upstream revision recorded in UPSTREAM.
//! No Go process or network connection is required to execute these tests.
use relaykit::{
    Context, Format, PreparedRequest, PreparedResponse, Request, Response, ResponseStreamState,
    StreamOptions,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json, value::RawValue};
use std::collections::BTreeMap;
use std::sync::Arc;

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/upstream_sync.json")).unwrap()
}

fn raw_at<'a>(source: &'a str, pointer: &str) -> Option<&'a str> {
    let mut value: &'a RawValue = serde_json::from_str(source).ok()?;
    for segment in pointer.split('/').skip(1) {
        value = if value.get().trim_start().starts_with('[') {
            let items: Vec<&'a RawValue> = serde_json::from_str(value.get()).ok()?;
            *items.get(segment.parse::<usize>().ok()?)?
        } else {
            let mut fields: BTreeMap<String, &'a RawValue> =
                serde_json::from_str(value.get()).ok()?;
            fields.remove(&segment.replace("~1", "/").replace("~0", "~"))?
        };
    }
    Some(value.get())
}

fn normalize(value: &mut Value) {
    if value.get("Type").is_some() && value.get("Payload").is_some() {
        *value = value["Payload"].take();
    }
    match value {
        Value::String(text) => {
            let bytes = text.as_bytes();
            let mut output = String::new();
            let mut start = 0;
            let mut index = 0;
            while index + 32 <= bytes.len() {
                if bytes[index..index + 32]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
                {
                    output.push_str(&text[start..index]);
                    output.push_str("<uuid>");
                    index += 32;
                    start = index;
                } else {
                    index += 1;
                }
            }
            output.push_str(&text[start..]);
            *text = output;
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        Value::Number(number) if number.as_i64().is_none() && number.as_u64().is_none() => {
            if let Some(number) = number.as_f64().and_then(serde_json::Number::from_f64) {
                *value = Value::Number(number);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "created" | "created_at")
                    && value.as_i64().is_some_and(|value| value >= 100_000_000)
                {
                    *value = json!(0);
                } else {
                    normalize(value);
                }
            }
        }
        _ => {}
    }
}

fn dto_bytes<T: DeserializeOwned + Serialize>(input: &str) -> Result<Vec<u8>, relaykit::Error> {
    Ok(serde_json::to_vec(&serde_json::from_str::<T>(input)?)?)
}

fn execute(case: &Value) -> Result<(Value, String), relaykit::Error> {
    let from: Format = case["from"].as_str().unwrap().parse()?;
    let to: Format = case["to"].as_str().unwrap().parse()?;
    let model = case["model"].as_str().unwrap_or("");
    let mut context = Context {
        channel_meta_attached: !model.is_empty(),
        upstream_model_name: model.into(),
        ..Context::default()
    };
    if case["supports_json_schema"] == true {
        context.options.gemini.supports_json_schema = Some(Arc::new(|_| true));
    }
    match case["kind"].as_str().unwrap() {
        "dto" => {
            let input = case["input_json"].as_str().unwrap();
            let bytes = match model {
                "GeneralOpenAIRequest" => dto_bytes::<relaykit::dto::GeneralOpenAIRequest>(input)?,
                "InputTokenDetails" => dto_bytes::<relaykit::dto::InputTokenDetails>(input)?,
                "FunctionRequest" => dto_bytes::<relaykit::dto::FunctionRequest>(input)?,
                "Tool" => dto_bytes::<relaykit::dto::Tool>(input)?,
                "ClaudeRequest" => dto_bytes::<relaykit::dto::ClaudeRequest>(input)?,
                _ => unreachable!("unsupported upstream DTO fixture"),
            };
            Ok((
                json!({"value":serde_json::from_slice::<Value>(&bytes)?}),
                String::from_utf8(bytes).unwrap(),
            ))
        }
        "request" => {
            let input = case["input_json"].as_str().unwrap().as_bytes();
            let prepared = PreparedRequest::from_slice(from, input)?;
            let result = relaykit::convert_prepared_request(&context, to, &prepared)?;
            let bytes = result.value.to_vec()?;
            let actual = json!({
                "value":serde_json::from_slice::<Value>(&bytes)?,
                "diagnostics":result.diagnostics,
            });
            // Exercise the ordinary API as well as the prepared wire writer.
            let request = Request::from_value(from, case["input"].clone())?;
            let value = relaykit::convert_request(&context, to, &request)?
                .value
                .into_value();
            assert_eq!(value, actual["value"], "prepared/value API divergence");
            Ok((actual, String::from_utf8(bytes).unwrap()))
        }
        "response" => {
            let input = Response::from_value(from, case["input"].clone())?;
            let result = relaykit::convert_response(&context, to, &input)?;
            let bytes = serde_json::to_vec(&result.value)?;
            let prepared = PreparedResponse::from_slice(
                from,
                case["input_json"].as_str().unwrap().as_bytes(),
            )?;
            let prepared_result = relaykit::convert_prepared_response(&context, to, &prepared)?;
            let mut prepared_value = prepared_result.value.to_value()?;
            let mut value = result.value.as_value().clone();
            normalize(&mut prepared_value);
            normalize(&mut value);
            assert_eq!(prepared_value, value, "{}: prepared/value response divergence", case["name"]);
            Ok((
                json!({
                    "value":result.value.into_value(),
                    "usage":result.usage,
                    "diagnostics":result.diagnostics,
                }),
                String::from_utf8(bytes).unwrap(),
            ))
        }
        "stream" => {
            let mut state = ResponseStreamState::new(
                from,
                to,
                StreamOptions {
                    id: "stream_fixed".into(),
                    model: "stream-model".into(),
                    ..StreamOptions::default()
                },
            )?;
            let mut events = vec![];
            for chunk in case["chunks"].as_array().unwrap() {
                events.extend(state.convert_chunk(&context, chunk)?);
            }
            events.extend(state.finalize(&context)?);
            Ok((
                json!({
                    "value":events,
                    "usage":state.usage(),
                    "diagnostics":state.diagnostics(),
                }),
                String::new(),
            ))
        }
        _ => unreachable!("unsupported upstream fixture kind"),
    }
}

#[test]
fn pinned_go_fixtures_have_explicit_provenance_and_raw_oracles() {
    let corpus = corpus();
    let commit = include_str!("../UPSTREAM")
        .lines()
        .find_map(|line| line.strip_prefix("Commit: "))
        .unwrap();
    assert_eq!(
        corpus["provenance"]["target_commit"],
        commit,
    );
    assert_eq!(corpus["provenance"]["baseline_tag"], "relaykit/v0.3.5");
    let cases = corpus["cases"].as_array().unwrap();
    assert!(cases.len() >= 21);
    for key in ["source_go_sha256", "oracle_sha256"] {
        let hash = corpus["provenance"][key].as_str().unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    for entry in cases {
        assert_eq!(entry["case"]["name"], entry["expected"]["name"]);
        assert!(entry["expected"].get("error").is_none(), "{entry}");
        let serialized = entry["expected"]["serialized_value"].as_str().unwrap();
        serde_json::from_str::<Value>(serialized).unwrap();
        if let Some(pointer) = entry["case"]["raw_pointer"].as_str() {
            assert!(
                raw_at(serialized, pointer).is_some_and(|value| !value.is_empty()),
                "missing oracle path {pointer} in {}",
                entry["case"]["name"],
            );
        }
    }
}

#[test]
fn runtime_matches_pinned_go_after_upstream_sync() {
    let corpus = corpus();
    let mut failures = vec![];
    for entry in corpus["cases"].as_array().unwrap() {
        let case = &entry["case"];
        if std::env::var("UPSTREAM_CASE_FILTER")
            .is_ok_and(|filter| !case["name"].as_str().unwrap().contains(&filter))
        {
            continue;
        }
        let expected = &entry["expected"];
        let mut wanted = json!({
            "value":expected["value"],
            "diagnostics":expected["diagnostics"].as_array().cloned().unwrap_or_default(),
        });
        if matches!(case["kind"].as_str(), Some("response" | "stream")) {
            wanted["usage"] = expected["usage"].clone();
        }
        if case["kind"] == "dto" {
            wanted.as_object_mut().unwrap().remove("diagnostics");
        }
        let (mut actual, serialized) = match execute(case) {
            Ok(result) => result,
            Err(error) => (json!({"error":error.to_string()}), String::new()),
        };
        normalize(&mut wanted);
        normalize(&mut actual);
        let raw_diff = case["raw_pointer"].as_str().and_then(|pointer| {
            if serialized.is_empty() || actual.get("error").is_some() {
                return None;
            }
            let wanted = raw_at(expected["serialized_value"].as_str().unwrap(), pointer);
            let actual = raw_at(&serialized, pointer);
            (actual != wanted).then(|| json!({"pointer":pointer,"actual":actual,"expected":wanted}))
        });
        if actual != wanted || raw_diff.is_some() {
            failures.push(json!({
                "name":case["name"],
                "actual":actual,
                "expected":wanted,
                "raw_diff":raw_diff,
            }));
        }
    }
    if !failures.is_empty() {
        let path = std::env::temp_dir().join("relaykit-upstream-sync-diff.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&failures).unwrap()).unwrap();
        panic!(
            "{} latest-Go cases differ; see {}:\n{}",
            failures.len(),
            path.display(),
            failures
                .iter()
                .map(|failure| failure["name"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
}
