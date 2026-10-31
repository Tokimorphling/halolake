use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{Arc, Mutex},
};

/// A text protocol. Wire names match upstream RelayFormat values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Format {
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "claude")]
    Claude,
    #[serde(rename = "gemini")]
    Gemini,
}
impl Format {
    pub const ALL: [Self; 4] = [
        Self::OpenAi,
        Self::OpenAiResponses,
        Self::Claude,
        Self::Gemini,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::OpenAiResponses => "openai_responses",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }
}
impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::str::FromStr for Format {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|f| f.as_str() == s)
            .ok_or_else(|| Error::message(format!("unsupported relay format: {s}")))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Good,
    Fair,
    Discouraged,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Error,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LossPolicy {
    #[default]
    Allow,
    Safe,
    Strict,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    pub message: String,
    pub severity: Severity,
    pub from: Format,
    pub to: Format,
}
#[derive(Debug)]
pub enum Error {
    Message(String),
    Json(serde_json::Error),
    ConversionLoss(Vec<Diagnostic>),
}
impl Error {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(s) => f.write_str(s),
            Self::Json(e) => e.fmt(f),
            Self::ConversionLoss(d) => {
                f.write_str("conversion would lose protocol semantics")?;
                for (i, d) in d.iter().enumerate() {
                    f.write_str(if i == 0 { ": " } else { "; " })?;
                    if !d.path.is_empty() {
                        write!(f, "{}: ", d.path)?
                    }
                    f.write_str(if d.message.is_empty() {
                        &d.code
                    } else {
                        &d.message
                    })?
                }
                Ok(())
            }
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Json(e) = self {
            Some(e)
        } else {
            None
        }
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
pub(crate) fn reject_loss(policy: LossPolicy, diagnostics: &[Diagnostic]) -> Result<(), Error> {
    let rejected = diagnostics
        .iter()
        .filter(|d| {
            policy == LossPolicy::Strict
                || policy == LossPolicy::Safe && d.severity == Severity::Error
        })
        .cloned()
        .collect::<Vec<_>>();
    if rejected.is_empty() {
        Ok(())
    } else {
        Err(Error::ConversionLoss(rejected))
    }
}

pub type ModelPredicate = Arc<dyn Fn(&str) -> bool + Send + Sync>;
pub type ModelTokenLimit = Arc<dyn Fn(&str) -> i64 + Send + Sync>;
pub type SafetySetting = Arc<dyn Fn(&str) -> String + Send + Sync>;
#[derive(Clone, Default)]
pub struct ClaudeOptions {
    pub thinking_adapter_enabled: bool,
    pub thinking_adapter_budget_tokens_percentage: f64,
    pub default_max_tokens: Option<ModelTokenLimit>,
    pub web_search_tool_version: String,
}
#[derive(Clone, Default)]
pub struct GeminiOptions {
    pub thinking_adapter_enabled: bool,
    pub thinking_adapter_budget_tokens_percentage: f64,
    pub function_call_thought_signature_enabled: bool,
    pub supports_imagine: Option<ModelPredicate>,
    /// Whether the upstream accepts `parametersJsonSchema` and `responseJsonSchema`.
    /// Unset means unsupported; lossy typed-schema fallback emits diagnostics.
    pub supports_json_schema: Option<ModelPredicate>,
    pub safety_setting: Option<SafetySetting>,
}
#[derive(Clone, Default)]
pub struct Options {
    pub claude: ClaudeOptions,
    pub gemini: GeminiOptions,
    pub tool_loss_policy: LossPolicy,
    pub open_router_dialect: bool,
    pub preserve_thinking_suffix: Option<ModelPredicate>,
    pub preserve_effort_tail: Option<ModelPredicate>,
    pub web_search: Option<WebSearchEncoder>,
    /// Retain typed conversation items and provider reasoning metadata when
    /// translating histories instead of using the legacy Go wire shape.
    pub preserve_message_structure: bool,
    /// Compatibility-only allowance for a host facade to complete an unpaired
    /// web-search call. Low-level stream state remains strict by default.
    pub allow_unpaired_hosted_search: bool,
}
#[derive(Clone, Debug)]
pub struct WebSearchCall {
    pub source: Format,
    pub target: Format,
    pub native_type: String,
    pub index: usize,
    pub forced: bool,
    pub spec: Value,
}
#[derive(Clone, Debug, Default)]
pub struct WebSearchEncoding {
    pub tool: Option<Value>,
    /// Complete host-native tool JSON; bypasses the Chat function-tool schema.
    /// Takes precedence over `tool`, equivalent to Go ToolCallRequest.Native.
    pub native_tool: Option<Value>,
    pub omitted: bool,
    pub diagnostics: Vec<Diagnostic>,
}
pub type WebSearchEncoder = Arc<
    dyn Fn(&WebSearchCall, &mut Value) -> Result<Option<WebSearchEncoding>, Error> + Send + Sync,
>;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponsesToolState {
    pub custom_tool_names: BTreeSet<String>,
    pub namespaces: BTreeMap<String, String>,
    pub namespace_tools: BTreeMap<String, ResponsesNamespaceTool>,
    pub tool_search: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponsesNamespaceTool {
    pub namespace: String,
    pub name: String,
}
impl ResponsesToolState {
    pub fn is_custom_tool(&self, name: &str) -> bool {
        self.custom_tool_names.contains(name)
    }
    pub fn responses_tool_name(&self, name: &str) -> (String, String) {
        if let Some(tool) = self.namespace_tools.get(name) {
            return (tool.namespace.clone(), tool.name.clone());
        }
        match self.namespaces.get(name) {
            Some(ns) => (
                ns.clone(),
                name.strip_prefix(&format!("{ns}__")).unwrap_or(name).into(),
            ),
            None => (String::new(), name.into()),
        }
    }
    pub fn namespace_entry(&self, name: &str) -> Option<&ResponsesNamespaceTool> {
        self.namespace_tools.get(name)
    }
    pub fn flatten_declared(&self, namespace: &str, name: &str) -> Option<&str> {
        self.namespace_tools
            .iter()
            .find(|(_, tool)| tool.namespace == namespace && tool.name == name)
            .map(|(flattened, _)| flattened.as_str())
    }
}
pub fn namespaced_tool_name(namespace: &str, name: &str) -> String {
    if namespace.is_empty() || namespace == "functions" {
        name.into()
    } else {
        format!("{namespace}__{name}")
    }
}
/// Host-scoped media callbacks. The crate performs no network I/O.
#[derive(Clone, Default)]
pub struct MediaResolver {
    pub get_base64_data: Option<MediaCallback>,
    pub decode_base64_file_data: Option<DecodeMediaCallback>,
}
pub type MediaCallback = Arc<dyn Fn(&str, &str) -> Result<(String, String), Error> + Send + Sync>;
pub type DecodeMediaCallback = Arc<dyn Fn(&str) -> Result<(String, String), Error> + Send + Sync>;
/// Per-request host configuration and round-trip tool metadata.
/// Share a context only across the request and its corresponding response.
#[derive(Default)]
pub struct Context {
    pub origin_model_name: String,
    pub upstream_model_name: String,
    pub channel_meta_attached: bool,
    pub is_stream: bool,
    pub estimate_prompt_tokens: i64,
    pub response_id: String,
    pub created: i64,
    pub reasoning_conversion: Option<crate::reasoning::Intent>,
    pub options: Options,
    pub media: MediaResolver,
    pub responses_tools: Mutex<ResponsesToolState>,
    pub reasoning_effort: Mutex<String>,
    pub conversion_chain: Mutex<Vec<Format>>,
}
impl Context {
    pub fn resolve_media(&self, source: &str, mime: &str) -> Result<(String, String), Error> {
        self.media
            .get_base64_data
            .as_ref()
            .ok_or_else(|| Error::message("relayconvert media resolver is not configured"))?(
            source, mime,
        )
    }
    pub fn decode_media(&self, data: &str) -> Result<(String, String), Error> {
        self.media
            .decode_base64_file_data
            .as_ref()
            .ok_or_else(|| Error::message("relayconvert media resolver is not configured"))?(
            data
        )
    }
    pub fn responses_tool_state(&self) -> ResponsesToolState {
        self.responses_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn set_responses_tool_state(&self, state: ResponsesToolState) {
        *self
            .responses_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = state;
    }
    pub fn set_reasoning_effort(&self, effort: impl Into<String>) {
        *self
            .reasoning_effort
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = effort.into();
    }
    pub fn upstream_model(&self) -> &str {
        if self.channel_meta_attached {
            &self.upstream_model_name
        } else {
            ""
        }
    }
}

pub(crate) fn new_id(prefix: &str) -> String {
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static RANDOM: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
    // Read for &File permits concurrent reads. The random device supplies fresh
    // bytes per read; reusing its descriptor avoids an open/close for every ID.
    // Do not cache open failures, so a transient resource limit can recover.
    if RANDOM.get().is_none() {
        if let Ok(file) = std::fs::File::open("/dev/urandom") {
            let _ = RANDOM.set(file);
        }
    }
    let mut bytes = [0u8; 16];
    if RANDOM
        .get()
        .is_none_or(|mut file| file.read_exact(&mut bytes).is_err())
    {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes = (time ^ u128::from(NEXT.fetch_add(1, Ordering::Relaxed))).to_be_bytes();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut id = String::with_capacity(prefix.len() + 32);
    id.push_str(prefix);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        id.push(HEX[usize::from(b >> 4)] as char);
        id.push(HEX[usize::from(b & 15)] as char);
    }
    id
}

#[cfg(test)]
mod id_tests {
    #[test]
    fn generated_ids_have_v4_bits_and_are_distinct_across_threads() {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| (0..64).map(|_| super::new_id("call_")).collect::<Vec<_>>())
            })
            .collect();
        let ids: Vec<_> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        for id in &ids {
            let hex = id.strip_prefix("call_").unwrap();
            assert_eq!(hex.len(), 32);
            assert!(
                hex.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
            assert_eq!(hex.as_bytes()[12], b'4');
            assert!(b"89ab".contains(&hex.as_bytes()[16]));
        }
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
    }
}
