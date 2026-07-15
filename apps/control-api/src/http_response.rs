//! HTTP response helpers shared by control-api handlers.

use crate::{security::SecurityError, system_task::SystemTaskRecord};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use halolake_api_contract::ApiResponse;
use halolake_control_plane::{ChannelFeedbackError, ManagementError, UsageError};
use halolake_domain::{PageRequest, PageResult};
use serde::Serialize;
use serde_json::{Value as JsonValue, json};

#[derive(Debug, Serialize)]
pub(crate) struct HealthResponse {
    pub(crate) status:           &'static str,
    pub(crate) snapshot_version: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub(crate) struct ErrorBody {
    pub(crate) message: String,
}

pub(crate) fn api_success_with_extra(mut value: JsonValue) -> Response {
    if let Some(object) = value.as_object_mut() {
        object.insert("success".to_string(), json!(true));
        object.entry("message").or_insert_with(|| json!(""));
    }
    Json(value).into_response()
}

pub(crate) fn api_success<T: Serialize>(data: T) -> Response {
    Json(ApiResponse::success(data)).into_response()
}

pub(crate) fn api_success_with_message<T: Serialize>(message: &str, data: T) -> Response {
    Json(ApiResponse {
        success: true,
        message: message.to_string(),
        data:    Some(data),
    })
    .into_response()
}

pub(crate) fn api_ok() -> Response {
    Json(ApiResponse::<()>::ok()).into_response()
}

pub(crate) fn api_ok_message(message: &str) -> Response {
    Json(ApiResponse::<()> {
        success: true,
        message: message.to_string(),
        data:    None,
    })
    .into_response()
}

pub(crate) fn api_error_status(status: StatusCode, message: &str) -> Response {
    (status, Json(ApiResponse::<()>::error(message))).into_response()
}

pub(crate) fn api_error_status_with_data<T: Serialize>(
    status: StatusCode,
    message: &str,
    data: T,
) -> Response {
    (
        status,
        Json(ApiResponse {
            success: false,
            message: message.to_string(),
            data:    Some(data),
        }),
    )
        .into_response()
}

pub(crate) fn system_task_conflict(task: &SystemTaskRecord, message: &str) -> Response {
    api_error_status_with_data(
        StatusCode::CONFLICT,
        message,
        json!({
            "task_id": task.task_id,
            "status": task.status,
            "type": task.task_type,
        }),
    )
}

pub(crate) fn management_error(err: ManagementError) -> Response {
    let status = match err {
        ManagementError::NotFound => StatusCode::NOT_FOUND,
        ManagementError::Duplicate
        | ManagementError::InvalidCredentials
        | ManagementError::InvalidRequest(_)
        | ManagementError::PasswordHash(_)
        | ManagementError::InvalidModelMapping { .. } => StatusCode::BAD_REQUEST,
        ManagementError::StaleChannelUpdate(_) | ManagementError::StaleManagementVersion { .. } => {
            StatusCode::CONFLICT
        }
        ManagementError::PermissionDenied => StatusCode::FORBIDDEN,
        ManagementError::UnsupportedChannelType(_) => StatusCode::BAD_REQUEST,
        ManagementError::Poisoned(_)
        | ManagementError::Snapshot(_)
        | ManagementError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error_status(status, &err.to_string())
}

pub(crate) fn security_error(err: SecurityError) -> Response {
    let message = err.message();
    match err {
        SecurityError::Management(err) => management_error(err),
        SecurityError::Business(_) => api_error_status(StatusCode::OK, &message),
    }
}

pub(crate) fn usage_error(err: UsageError) -> Response {
    let status = match err {
        UsageError::Unavailable | UsageError::Poisoned(_) | UsageError::Storage(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        UsageError::Transport(_) | UsageError::InvalidResponse(_) => StatusCode::BAD_GATEWAY,
    };
    api_error_status(status, &err.to_string())
}

pub(crate) fn channel_feedback_error(err: ChannelFeedbackError) -> Response {
    let status = match err {
        ChannelFeedbackError::Unavailable | ChannelFeedbackError::Storage(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        ChannelFeedbackError::Transport(_) | ChannelFeedbackError::InvalidResponse(_) => {
            StatusCode::BAD_GATEWAY
        }
    };
    api_error_status(status, &err.to_string())
}

pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: ErrorBody {
                message: message.to_string(),
            },
        }),
    )
        .into_response()
}

pub(crate) fn page_items<T>(items: Vec<T>, page: PageRequest) -> PageResult<T> {
    let total = items.len();
    let start = page.offset();
    let limit = page.limit();
    let items = items.into_iter().skip(start).take(limit).collect();
    PageResult::new(items, total, page)
}
