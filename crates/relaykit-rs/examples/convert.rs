use relaykit::{Context, Format, Request, convert_request};
use serde_json::json;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request = Request::from_value(
        Format::OpenAi,
        json!({
            "model":"claude-sonnet-4-5", "max_tokens":1024,
            "messages":[{"role":"user","content":"Hello!"}]
        }),
    )?;
    let result = convert_request(&Context::default(), Format::Claude, &request)?;
    println!("{}", serde_json::to_string_pretty(&result.value)?);
    Ok(())
}
