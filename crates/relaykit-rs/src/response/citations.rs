use super::{arr, n, s};
use crate::Error;
use serde_json::{Value, json};

fn annotation_index(value: &Value) -> Option<usize> {
    value
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .or_else(|| {
            value.as_f64().and_then(|index| {
                (index >= 0. && index.fract() == 0. && index < usize::MAX as f64)
                    .then_some(index as usize)
            })
        })
}

pub(crate) fn to_claude(annotations: &Value, text: &str) -> Vec<Value> {
    let mut out = vec![];
    for a in arr(annotations) {
        if s(&a["type"]).trim() != "url_citation" {
            continue;
        }
        let c = if a["url_citation"].is_object() {
            &a["url_citation"]
        } else {
            a
        };
        let url = s(&c["url"]).trim();
        if url.is_empty() {
            continue;
        }
        let mut v = json!({"type":"web_search_result_location","url":url});
        let title = s(&c["title"]).trim();
        if !title.is_empty() {
            v["title"] = json!(title);
        }
        let mut cited = s(&c["cited_text"]).to_owned();
        if cited.is_empty() {
            let start = annotation_index(&c["start_index"]);
            let end = annotation_index(&c["end_index"]);
            if let (Some(start), Some(end)) = (start, end) {
                if end > start && end <= text.chars().count() {
                    cited = text.chars().skip(start).take(end - start).collect();
                }
            }
        }
        if !cited.is_empty() {
            v["cited_text"] = json!(cited);
        }
        if !s(&c["encrypted_index"]).is_empty() {
            v["encrypted_index"] = c["encrypted_index"].clone();
        }
        out.push(v);
    }
    out
}
pub(crate) fn claude_to_chat(citations: &Value, text: &str, offset: usize) -> Vec<Value> {
    // Go decodes the complete list into []map[string]any before converting it.
    // Null elements are zero maps, while one scalar/array invalidates the list.
    if arr(citations)
        .iter()
        .any(|entry| !entry.is_object() && !entry.is_null())
    {
        return vec![];
    }
    let mut out = vec![];
    for c in arr(citations) {
        let raw_url = go_interface_string(&c["url"]);
        let url = raw_url.trim();
        if url.is_empty() {
            continue;
        }
        let mut v = json!({"url":url});
        let raw_title = go_interface_string(&c["title"]);
        let title = raw_title.trim();
        if !title.is_empty() {
            v["title"] = json!(title);
        }
        let cited = go_interface_string(&c["cited_text"]);
        if !cited.is_empty() {
            v["cited_text"] = json!(cited);
            if let Some(pos) = text.find(cited.as_ref()) {
                let start = offset + text[..pos].chars().count();
                v["start_index"] = json!(start);
                v["end_index"] = json!(start + cited.chars().count());
            }
        }
        let encrypted_index = go_interface_string(&c["encrypted_index"]);
        if !encrypted_index.is_empty() {
            v["encrypted_index"] = json!(encrypted_index);
        }
        out.push(json!({"type":"url_citation","url_citation":v}));
    }
    out
}

// Claude citations are decoded by Go into map[string]any: all JSON numbers
// become float64. Interface2String uses fixed-point formatting for a scalar
// float, but its fmt.Sprintf("%v") fallback uses general formatting inside
// arrays/maps. Ordinary citation strings stay borrowed on this path.
pub(crate) fn go_interface_string(value: &Value) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    match value {
        Value::String(text) => Cow::Borrowed(text),
        Value::Null => Cow::Borrowed(""),
        Value::Bool(value) => Cow::Borrowed(if *value { "true" } else { "false" }),
        Value::Number(value) => Cow::Owned(value.as_f64().unwrap_or(0.).to_string()),
        _ => {
            let mut formatted = String::new();
            go_fmt_value(value, &mut formatted);
            Cow::Owned(formatted)
        }
    }
}
fn go_fmt_value(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("<nil>"),
        Value::String(text) => output.push_str(text),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => {
            let number = value.as_f64().unwrap_or(0.);
            let scientific = format!("{number:e}");
            let (digits, exponent) = scientific.split_once('e').unwrap();
            let exponent: i32 = exponent.parse().unwrap();
            // strconv.FormatFloat('g', -1, 64) uses these exponent bounds.
            if !(-4..6).contains(&exponent) {
                output.push_str(digits);
                output.push('e');
                use std::fmt::Write;
                write!(output, "{exponent:+03}").unwrap();
            } else {
                output.push_str(&number.to_string());
            }
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(' ');
                }
                go_fmt_value(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push_str("map[");
            // fmt sorts string map keys, independent of insertion order.
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(' ');
                }
                output.push_str(key);
                output.push(':');
                go_fmt_value(value, output);
            }
            output.push(']');
        }
    }
}

pub(crate) fn to_responses(a: &Value) -> Result<Vec<Value>, Error> {
    if !a.is_null() && !a.is_array() {
        return Err(Error::message("invalid Chat annotations: expected array"));
    }
    arr(a)
        .iter()
        .map(|v| {
            if !v.is_object() {
                return Err(Error::message("invalid Chat annotations: expected object"));
            }
            if s(&v["type"]).trim() == "url_citation" && v["url_citation"].is_object() {
                let mut out = json!({"type":"url_citation"});
                for (k, v) in v["url_citation"].as_object().unwrap() {
                    out[k] = v.clone();
                }
                Ok(out)
            } else {
                Ok(v.clone())
            }
        })
        .collect()
}
pub(crate) fn response_annotation_to_chat(v: &Value) -> Result<Value, Error> {
    if !v.is_object() {
        return Err(Error::message(
            "invalid Responses annotation: expected object",
        ));
    }
    if s(&v["type"]).trim() != "url_citation" {
        return Ok(v.clone());
    }
    let mut c = v.clone();
    c.as_object_mut().unwrap().remove("type");
    Ok(json!({"type":"url_citation","url_citation":c}))
}
pub(crate) fn gemini_to_chat(metadata: &Value, content: &Value, rendered: &str) -> Vec<Value> {
    let parts = arr(&content["parts"]);
    let mut located = vec![None; parts.len()];
    let mut cursor = 0;
    for (i, p) in parts.iter().enumerate() {
        let t = s(&p["text"]);
        if t.is_empty() || t == "\n" || p["thought"] == true {
            continue;
        }
        if let Some(pos) = rendered[cursor..].find(t) {
            let start = cursor + pos;
            located[i] = Some((start, t));
            cursor = start + t.len();
        }
    }
    let valid: Vec<_> = located
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.map(|_| i))
        .collect();
    let chunks = arr(&metadata["groundingChunks"]);
    let mut out = vec![];
    let mut seen = std::collections::BTreeSet::new();
    for support in arr(&metadata["groundingSupports"]) {
        let segment = &support["segment"];
        let index = if segment["partIndex"].is_null() {
            if valid.len() != 1 {
                continue;
            }
            valid[0]
        } else {
            n(&segment["partIndex"]) as usize
        };
        let Some(Some((part_offset, text))) = located.get(index) else {
            continue;
        };
        let start = n(&segment["startIndex"]);
        let end = n(&segment["endIndex"]);
        if start < 0
            || end <= start
            || end as usize > text.len()
            || !text.is_char_boundary(start as usize)
            || !text.is_char_boundary(end as usize)
        {
            continue;
        }
        if !s(&segment["text"]).is_empty()
            && text[start as usize..end as usize] != *s(&segment["text"])
        {
            continue;
        }
        let start = rendered[..part_offset + start as usize].chars().count();
        let end = rendered[..part_offset + end as usize].chars().count();
        for ci in arr(&support["groundingChunkIndices"]) {
            let Some(chunk) = chunks.get(n(ci) as usize) else {
                continue;
            };
            let source = if chunk["web"].is_null() {
                &chunk["retrievedContext"]
            } else {
                &chunk["web"]
            };
            let url = s(&source["uri"]);
            if url.is_empty() {
                continue;
            }
            let key = format!("{url}:{start}:{end}");
            if !seen.insert(key) {
                continue;
            }
            let citation =
                json!({"url":url,"start_index":start,"end_index":end,"title":s(&source["title"])});
            out.push(json!({"type":"url_citation","url_citation":citation}));
        }
    }
    out
}

#[cfg(test)]
mod claude_citation_tests {
    use super::*;
    #[test]
    fn pinned_interface_string_probes_match_scalar_and_nested_go_formats() {
        for (value, expected) in [
            (Value::Null, ""),
            (json!(false), "false"),
            (json!(true), "true"),
            (json!("  plain  "), "  plain  "),
            (json!(230), "230"),
            (json!(9007199254740993u64), "9007199254740992"),
            (json!(u64::MAX), "18446744073709552000"),
            (json!(0.00001), "0.00001"),
            (json!(1000000.), "1000000"),
            (json!(1e20), "100000000000000000000"),
            (json!([]), "[]"),
            (json!({}), "map[]"),
            (json!({"β":2,"a":null,"A":" "}), "map[A:  a:<nil> β:2]"),
            (
                json!([0.0001, 0.00001, 100000., 1000000., -0.0, 1.23456789e-7]),
                "[0.0001 1e-05 100000 1e+06 -0 1.23456789e-07]",
            ),
        ] {
            assert_eq!(go_interface_string(&value), expected);
        }
    }
    #[test]
    fn interface_coercion_and_unicode_offsets_follow_go() {
        let source = json!([{"url":230,"title":true,"cited_text":0,"encrypted_index":["a",null,{"z":1000000,"b":[false,0.00001]}]}]);
        assert_eq!(claude_to_chat(&source, "中0文", 2), json!([{"type":"url_citation","url_citation":{"url":"230","title":"true","cited_text":"0","start_index":3,"end_index":4,"encrypted_index":"[a <nil> map[b:[false 1e-05] z:1e+06]]"}}]).as_array().unwrap().clone());
        assert_eq!(
            go_interface_string(&json!(9007199254740993u64)),
            "9007199254740992"
        );
        assert_eq!(go_interface_string(&json!(0.00001)), "0.00001");
        assert_eq!(go_interface_string(&json!([0.00001])), "[1e-05]");
    }
    #[test]
    fn scalar_or_array_entries_reject_the_whole_list_but_null_is_allowed() {
        let valid = json!({"url":"https://example.test","cited_text":"answer"});
        assert!(claude_to_chat(&json!([valid, 7]), "answer", 0).is_empty());
        assert!(claude_to_chat(&json!([valid, []]), "answer", 0).is_empty());
        assert_eq!(claude_to_chat(&json!([null, valid]), "answer", 0).len(), 1);
    }
}
