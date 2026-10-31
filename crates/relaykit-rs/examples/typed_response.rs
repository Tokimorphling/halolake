use relaykit::{Context, Format, PreparedResponse, ResponseUsage, convert_response_to_writer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = br#"{
        "id":"msg_example",
        "model":"claude-example",
        "content":[{"type":"text","text":"Hello from the typed response path."}],
        "stop_reason":"end_turn",
        "usage":{"input_tokens":12,"output_tokens":8}
    }"#;
    let prepared = PreparedResponse::from_slice(Format::Claude, source)?;
    assert!(prepared.uses_typed_path(Format::OpenAi));

    let mut output = Vec::new();
    let metadata =
        convert_response_to_writer(&Context::default(), Format::OpenAi, &prepared, &mut output)?;
    println!("{}", std::str::from_utf8(&output)?);
    if let Some(usage) = metadata.usage.as_ref().and_then(ResponseUsage::canonical) {
        eprintln!(
            "prompt={}, completion={}",
            usage.prompt_tokens, usage.completion_tokens
        );
    }
    Ok(())
}
