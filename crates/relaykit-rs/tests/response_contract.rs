use relaykit::{Context, Format, Response};
use serde_json::{Value, json};
fn normalize(v: &mut Value) {
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
                    start = i
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
#[test]
fn hosted_citations_and_usage_match_go() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/response_contract.json")).unwrap();
    let mut failures = vec![];
    for entry in corpus.as_array().unwrap() {
        let c = &entry["case"];
        if std::env::var("RESPONSE_CASE_FILTER")
            .is_ok_and(|f| !c["name"].as_str().unwrap().contains(&f))
        {
            continue;
        }
        let from: Format = c["from"].as_str().unwrap().parse().unwrap();
        let to = c["to"].as_str().unwrap().parse().unwrap();
        let ctx = Context {
            channel_meta_attached: true,
            upstream_model_name: "upstream-model".into(),
            ..Default::default()
        };
        let result = Response::from_value(from, c["input"].clone())
            .and_then(|input| relaykit::convert_response(&ctx, to, &input));
        let expected = &entry["expected"];
        let mut actual = match result {
            Ok(r) => {
                json!({"value":r.value.into_value(),"usage":r.usage,"diagnostics":r.diagnostics})
            }
            Err(e) => json!({"error":e.to_string()}),
        };
        let mut want = if expected.get("error").is_some() {
            json!({"error":expected["error"]})
        } else {
            json!({"value":expected["value"],"usage":expected["usage"],"diagnostics":expected["diagnostics"].as_array().cloned().unwrap_or_default()})
        };
        normalize(&mut actual);
        normalize(&mut want);
        if actual.get("error").is_some() && want.get("error").is_some() {
            continue;
        }
        if actual != want {
            failures.push(json!({"case":c,"actual":actual,"expected":want}));
        }
    }
    if !failures.is_empty() {
        let path = std::env::temp_dir().join("relaykit-response-contract-diff.json");
        std::fs::write(&path, serde_json::to_string_pretty(&failures).unwrap()).unwrap();
        panic!(
            "{} response parity failures: {}",
            failures.len(),
            path.display()
        );
    }
}
