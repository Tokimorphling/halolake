//! Gateway-facing adapters for the workspace's single RelayKit conversion engine.
//! Transport, routing, and channel cache policy remain the host's responsibility.

mod compat;
mod relay;

pub use relaykit;
pub use relay::{
    PromptCacheMode, ProtocolError, RelaySseTranslator, ResponsesNamespaceTool,
    ResponsesToolMapping, apply_claude_prompt_cache, convert_request_json,
    convert_response_json, convert_response_json_with_options, filter_encrypted_reasoning,
    responses_tool_mapping,
};
pub use compat::*;
