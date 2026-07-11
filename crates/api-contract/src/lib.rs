pub mod claude;
pub mod gemini;
pub mod openai;

pub type JsonValue = serde_json::Value;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:    Option<T>,
}

impl<T> ApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self {
            success: true,
            message: String::new(),
            data:    Some(data),
        }
    }

    pub fn ok() -> ApiResponse<()> {
        ApiResponse {
            success: true,
            message: String::new(),
            data:    None,
        }
    }

    pub fn error(message: impl Into<String>) -> ApiResponse<()> {
        ApiResponse {
            success: false,
            message: message.into(),
            data:    None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Page<T> {
    pub items:     Vec<T>,
    pub total:     usize,
    pub page:      usize,
    pub page_size: usize,
}
