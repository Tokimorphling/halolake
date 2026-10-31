use serde_json::{Value, json};

/// The default upstream encoding/json codec is observable when JSON becomes a
/// string (function arguments and tool outputs). In particular it escapes HTML
/// and uses ECMAScript's fixed/scientific float boundary.
pub(crate) fn stringify(value: &Value) -> String {
    stringify_impl(value, false)
}

/// An interface{} field has float64 numbers even if its normalized Value uses
/// an integer variant. Keep Go's shortest float spelling inside argument text.
pub(crate) fn stringify_interface(value: &Value) -> String {
    stringify_impl(value, true)
}

fn stringify_impl(value: &Value, interface: bool) -> String {
    fn write(value: &Value, output: &mut String, interface: bool) {
        match value {
            Value::String(text) => output.push_str(
                &serde_json::to_string(text)
                    .unwrap()
                    .replace('<', "\\u003c")
                    .replace('>', "\\u003e")
                    .replace('&', "\\u0026")
                    .replace('\u{2028}', "\\u2028")
                    .replace('\u{2029}', "\\u2029"),
            ),
            Value::Array(values) => {
                output.push('[');
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        output.push(',');
                    }
                    write(v, output, interface);
                }
                output.push(']');
            }
            Value::Object(values) => {
                output.push('{');
                let mut fields: Vec<_> = values.iter().collect();
                fields.sort_unstable_by_key(|(key, _)| *key);
                for (i, (key, v)) in fields.into_iter().enumerate() {
                    if i > 0 {
                        output.push(',');
                    }
                    write(&json!(key), output, interface);
                    output.push(':');
                    write(v, output, interface);
                }
                output.push('}');
            }
            Value::Number(number) if interface || number.is_f64() => {
                let n = number.as_f64().unwrap();
                let abs = n.abs();
                if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
                    let text = format!("{n:e}");
                    let (digits, exponent) = text.split_once('e').unwrap();
                    output.push_str(digits);
                    output.push('e');
                    if !exponent.starts_with('-') {
                        output.push('+');
                    }
                    output.push_str(exponent);
                } else {
                    output.push_str(&n.to_string());
                }
            }
            _ => output.push_str(&value.to_string()),
        }
    }
    let mut output = String::new();
    write(value, &mut output, interface);
    output
}
