//! Borrowed request output and reusable normalized stream input.
use relaykit::{
    Context, Format, PreparedRequest, PreparedStreamChunk, ResponseStreamState, StreamOptions,
    convert_request_to_writer,
};
fn main() -> Result<(), relaykit::Error> {
    let ctx = Context::default();
    let bytes =
        br#"{"model":"example","max_tokens":1024,"messages":[{"role":"user","content":"Hello"}]}"#;
    let input = PreparedRequest::from_slice(Format::Claude, bytes)?;
    assert!(input.uses_typed_path(Format::OpenAi));
    let mut body = Vec::new();
    convert_request_to_writer(&ctx, Format::OpenAi, &input, &mut body)?;
    println!("{}", String::from_utf8_lossy(&body));

    let event = PreparedStreamChunk::from_slice(Format::OpenAi,
        br#"{"id":"chat_1","model":"example","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}"#)?;
    let mut stream =
        ResponseStreamState::new(Format::OpenAi, Format::Claude, StreamOptions::default())?;
    let mut events = stream.convert_prepared_chunk(&ctx, &event)?;
    events.extend(stream.finalize(&ctx)?);
    println!("{}", serde_json::to_string(&events)?);
    Ok(())
}
