use super::*;
use crate::{Diagnostic, Severity};
pub(super) fn inspect(from: Format, to: Format, v: &Value) -> Vec<Diagnostic> {
    if from == Format::Claude {
        return crate::response::hosted::inspect(from, to, v);
    }
    let mut ds = Vec::new();
    let mut add = |path: String, code: &str, message: String, severity: Severity| {
        ds.push(Diagnostic {
            path,
            code: code.into(),
            message,
            severity,
            from,
            to,
        })
    };
    if from == Format::OpenAiResponses && from != to {
        let item = st(&v["item"]["type"]).trim();
        if !matches!(
            item,
            "" | "message" | "reasoning" | "function_call" | "custom_tool_call"
        ) {
            add(
                "item".into(),
                "hosted_tool_event_unrepresentable",
                format!(
                    "OpenAI Responses hosted-tool output {item:?} has no semantic target-protocol stream mapping"
                ),
                Severity::Error,
            );
        } else {
            let typ = st(&v["type"]).trim();
            if [
                ".web_search_call.",
                ".file_search_call.",
                ".code_interpreter_call.",
                ".computer_tool_call.",
                ".image_generation_call.",
                ".mcp_call.",
            ]
            .iter()
            .any(|p| typ.contains(p))
            {
                add(
                    "type".into(),
                    "hosted_tool_event_unrepresentable",
                    format!(
                        "OpenAI Responses hosted-tool stream event {typ:?} has no semantic target-protocol mapping"
                    ),
                    Severity::Error,
                );
            }
        }
    } else if from == Format::Gemini {
        for (index, c) in arr(&v["candidates"]).iter().enumerate() {
            let m = &c["groundingMetadata"];
            if m.is_null() {
                continue;
            }
            let path = format!("candidates[{index}].groundingMetadata");
            if !arr(&m["webSearchQueries"]).is_empty() && to != Format::OpenAiResponses {
                add(format!("{path}.webSearchQueries"),"web_search_call_unrepresentable","Gemini grounding confirms a hosted web search, but the target stream converter cannot produce an OpenAI Responses web_search_call lifecycle".into(),Severity::Error);
            }
            if [
                "webSearchQueries",
                "retrievalQueries",
                "searchEntryPoint",
                "retrievalMetadata",
                "sourceFlaggingUris",
                "googleMapsWidgetContextToken",
            ]
            .iter()
            .any(|k| {
                m.get(k)
                    .is_some_and(|v| !v.is_null() && v != &json!([]) && v != &json!(""))
            }) {
                add(path,"hosted_tool_metadata_reduced","Gemini grounding citations are preserved across stream chunks, but provider-specific search metadata has no target-protocol equivalent".into(),Severity::Warning);
            }
        }
    }
    ds
}
