//! Stop/finish reason mappings, including upstream's pass-through behavior.
pub fn claude_to_openai(reason: &str) -> String {
    match reason.to_ascii_lowercase().as_str() {
        "stop_sequence" | "end_turn" => "stop",
        "max_tokens" | "pause_turn" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => reason,
    }
    .into()
}
pub fn openai_to_claude(reason: &str) -> String {
    match reason.to_ascii_lowercase().as_str() {
        "stop" => "end_turn",
        "stop_sequence" => "stop_sequence",
        "length" | "max_tokens" => "max_tokens",
        "content_filter" => "refusal",
        "tool_calls" => "tool_use",
        _ => reason,
    }
    .into()
}
