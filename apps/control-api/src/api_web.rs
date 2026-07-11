//! Static web asset hosting for control-api.

use crate::{
    AppState, CLASSIC_WEB_ASSETS, DEFAULT_WEB_ASSETS, EmbeddedAsset,
    http_response::{api_error_status, api_success, json_error},
};
use axum::{
    body::Body,
    extract::State,
    http::{
        HeaderValue, Method, StatusCode, Uri,
        header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::Response,
};
use std::path::{Component, Path as FsPath, PathBuf};
use tracing::warn;

pub(crate) async fn api_empty_string() -> Response {
    api_success("")
}

pub(crate) async fn web_fallback(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
) -> Response {
    if !state.web.enabled {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    if method != Method::GET && method != Method::HEAD {
        return api_error_status(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let path = uri.path();
    if is_control_or_relay_path(path) {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    let selected = selected_web_dist(&state);
    let root = &selected.root;
    let Some(candidate) = safe_static_file_path(&root, path) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid static path");
    };
    match tokio::fs::metadata(&candidate).await {
        Ok(metadata) if metadata.is_file() => {
            return static_file_response(candidate, method == Method::HEAD, is_index_file(path))
                .await;
        }
        _ => {}
    }
    if let Some(asset_path) = static_asset_path(path) {
        if let Some(asset) = selected.embedded(asset_path.as_str()) {
            return embedded_file_response(asset, method == Method::HEAD, is_index_file(path));
        }
    }
    if is_static_asset_request(path) {
        return json_error(StatusCode::NOT_FOUND, "static asset not found");
    }
    let index_path = root.join("index.html");
    match tokio::fs::metadata(&index_path).await {
        Ok(metadata) if metadata.is_file() => {
            static_file_response(index_path, method == Method::HEAD, true).await
        }
        _ => {
            if let Some(asset) = selected.embedded("index.html") {
                embedded_file_response(asset, method == Method::HEAD, true)
            } else {
                json_error(StatusCode::NOT_FOUND, "web asset not found")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebTheme {
    Default,
    Classic,
}

pub(crate) struct SelectedWebDist {
    pub(crate) theme: WebTheme,
    pub(crate) root:  PathBuf,
}

impl SelectedWebDist {
    pub(crate) fn embedded(&self, path: &str) -> Option<&'static EmbeddedAsset> {
        let assets = match self.theme {
            WebTheme::Default => DEFAULT_WEB_ASSETS,
            WebTheme::Classic => CLASSIC_WEB_ASSETS,
        };
        assets
            .binary_search_by_key(&path, |asset| asset.path)
            .ok()
            .and_then(|index| assets.get(index))
    }
}

pub(crate) fn selected_web_dist(state: &AppState) -> SelectedWebDist {
    let options = state.options.values().unwrap_or_default();
    let theme = options
        .get("theme.frontend")
        .map(String::as_str)
        .unwrap_or(state.web.theme.as_str());
    // Classic is no longer embedded; only use it when files exist on disk.
    if theme == "classic" && state.web.classic_dist.join("index.html").is_file() {
        SelectedWebDist {
            theme: WebTheme::Classic,
            root:  state.web.classic_dist.clone(),
        }
    } else {
        SelectedWebDist {
            theme: WebTheme::Default,
            root:  state.web.default_dist.clone(),
        }
    }
}

pub(crate) fn is_control_or_relay_path(path: &str) -> bool {
    path.starts_with("/api")
        || path.starts_with("/internal")
        || path.starts_with("/v1")
        || path.starts_with("/pg")
}

pub(crate) fn is_static_asset_request(path: &str) -> bool {
    path == "/assets" || path.starts_with("/assets/") || FsPath::new(path).extension().is_some()
}

pub(crate) fn is_index_file(path: &str) -> bool {
    path == "/" || path.ends_with("/index.html")
}

pub(crate) fn static_asset_path(request_path: &str) -> Option<String> {
    let relative = request_path.trim_start_matches('/');
    let normalized = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    for component in FsPath::new(normalized).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized.to_string())
}

pub(crate) fn safe_static_file_path(root: &FsPath, request_path: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    let relative = request_path.trim_start_matches('/');
    if relative.is_empty() {
        path.push("index.html");
        return Some(path);
    }
    for component in FsPath::new(relative).components() {
        match component {
            Component::Normal(segment) => path.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

pub(crate) async fn static_file_response(path: PathBuf, head: bool, no_cache: bool) -> Response {
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let content_length = bytes.len().to_string();
            let body = if head {
                Body::empty()
            } else {
                Body::from(bytes)
            };
            let mut response = Response::new(body);
            response.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static(static_content_type(&path)),
            );
            response.headers_mut().insert(
                CACHE_CONTROL,
                HeaderValue::from_static(if no_cache {
                    "no-cache"
                } else {
                    "public, max-age=31536000, immutable"
                }),
            );
            if let Ok(value) = HeaderValue::from_str(&content_length) {
                response.headers_mut().insert(CONTENT_LENGTH, value);
            }
            response
        }
        Err(err) => {
            warn!(?err, path = %path.display(), "failed to read web static file");
            json_error(StatusCode::NOT_FOUND, "web asset not found")
        }
    }
}

pub(crate) fn embedded_file_response(
    asset: &EmbeddedAsset,
    head: bool,
    no_cache: bool,
) -> Response {
    let content_length = asset.bytes.len().to_string();
    let body = if head {
        Body::empty()
    } else {
        Body::from(asset.bytes)
    };
    let mut response = Response::new(body);
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(static_content_type(FsPath::new(asset.path))),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if no_cache {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        }),
    );
    if let Ok(value) = HeaderValue::from_str(&content_length) {
        response.headers_mut().insert(CONTENT_LENGTH, value);
    }
    response
}

pub(crate) fn static_content_type(path: &FsPath) -> &'static str {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return "application/octet-stream";
    };
    if ext.eq_ignore_ascii_case("html") {
        "text/html; charset=utf-8"
    } else if ext.eq_ignore_ascii_case("js") || ext.eq_ignore_ascii_case("mjs") {
        "text/javascript; charset=utf-8"
    } else if ext.eq_ignore_ascii_case("css") {
        "text/css; charset=utf-8"
    } else if ext.eq_ignore_ascii_case("json") {
        "application/json; charset=utf-8"
    } else if ext.eq_ignore_ascii_case("svg") {
        "image/svg+xml"
    } else if ext.eq_ignore_ascii_case("png") {
        "image/png"
    } else if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if ext.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else if ext.eq_ignore_ascii_case("ico") {
        "image/x-icon"
    } else if ext.eq_ignore_ascii_case("woff") {
        "font/woff"
    } else if ext.eq_ignore_ascii_case("woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}
