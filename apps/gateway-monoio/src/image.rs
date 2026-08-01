use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAiImageRouteKind {
    Generations,
    Edits,
    LegacyEdits,
}

pub(crate) struct ParsedOpenAiImageRequest {
    pub(crate) kind:    OpenAiImageRouteKind,
    pub(crate) model:   String,
    pub(crate) stream:  bool,
    pub(crate) payload: OpenAiImagePayload,
}

pub(crate) enum OpenAiImagePayload {
    Json {
        request: openai::ImageRequest,
        value:   JsonValue,
    },
    Multipart,
}

pub(crate) fn parse_openai_image_request(
    req: &GatewayRequest,
) -> Result<ParsedOpenAiImageRequest, Response<GatewayBody>> {
    let Some(kind) = OpenAiImageRouteKind::from_path(&req.path) else {
        return Err(json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "OpenAI image route not found",
        ));
    };

    if kind == OpenAiImageRouteKind::Edits && is_multipart_content_type(&req.headers) {
        let model = match multipart_string_field(&req.body, &req.headers, "model") {
            Ok(Some(model)) if !model.trim().is_empty() => model.trim().to_string(),
            Ok(_) => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "model is required",
                ));
            }
            Err(err) => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &err,
                ));
            }
        };
        if let Ok(Some(n)) = multipart_string_field(&req.body, &req.headers, "n") {
            let n = n.trim();
            if !n.is_empty() {
                let parsed = n.parse::<u32>().ok();
                if parsed.is_none_or(|n| n > openai::MAX_IMAGE_N) {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &format!("n must be an integer between 1 and {}", openai::MAX_IMAGE_N),
                    ));
                }
            }
        }
        let stream = match multipart_string_field(&req.body, &req.headers, "stream") {
            Ok(Some(value)) if !value.trim().is_empty() => match parse_form_bool(value.trim()) {
                Some(value) => value,
                None => {
                    return Err(json_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "invalid stream value",
                    ));
                }
            },
            _ => false,
        };
        return Ok(ParsedOpenAiImageRequest {
            kind,
            model,
            stream,
            payload: OpenAiImagePayload::Multipart,
        });
    }

    let mut value: JsonValue = match serde_json::from_slice(&req.body) {
        Ok(value) => value,
        Err(err) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid OpenAI image request: {err}"),
            ));
        }
    };
    let mut image_req: openai::ImageRequest = match serde_json::from_value(value.clone()) {
        Ok(req) => req,
        Err(err) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid OpenAI image request: {err}"),
            ));
        }
    };
    if let Err(message) = normalize_openai_image_request(&mut image_req, &mut value) {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &message,
        ));
    }

    Ok(ParsedOpenAiImageRequest {
        kind,
        model: image_req.model.clone(),
        stream: image_req.is_stream(),
        payload: OpenAiImagePayload::Json {
            request: image_req,
            value,
        },
    })
}

impl OpenAiImageRouteKind {
    fn from_path(path_and_query: &str) -> Option<Self> {
        match path_and_query
            .split_once('?')
            .map_or(path_and_query, |(path, _)| path)
        {
            "/v1/images/generations" => Some(Self::Generations),
            "/v1/images/edits" => Some(Self::Edits),
            "/v1/edits" => Some(Self::LegacyEdits),
            _ => None,
        }
    }
}

fn normalize_openai_image_request(
    request: &mut openai::ImageRequest,
    value: &mut JsonValue,
) -> Result<(), String> {
    if request.model.is_empty() {
        return Err("model is required".to_string());
    }
    if request.size.contains('\u{00d7}') {
        return Err("size contains multiplication sign; use 'x' instead".to_string());
    }
    if request.n.is_some_and(|n| n > openai::MAX_IMAGE_N) {
        return Err(format!(
            "n must be an integer between 1 and {}",
            openai::MAX_IMAGE_N
        ));
    }

    match request.model.as_str() {
        "dall-e-2" | "dall-e" => {
            if !request.size.is_empty()
                && request.size != "256x256"
                && request.size != "512x512"
                && request.size != "1024x1024"
            {
                return Err(
                    "size must be one of 256x256, 512x512, or 1024x1024 for dall-e-2 or dall-e"
                        .to_string(),
                );
            }
            if request.size.is_empty() {
                request.size = "1024x1024".to_string();
                set_json_string(value, "size", &request.size);
            }
        }
        "dall-e-3" => {
            if !request.size.is_empty()
                && request.size != "1024x1024"
                && request.size != "1024x1792"
                && request.size != "1792x1024"
            {
                return Err(
                    "size must be one of 1024x1024, 1024x1792 or 1792x1024 for dall-e-3"
                        .to_string(),
                );
            }
            if request.quality.is_empty() {
                request.quality = "standard".to_string();
                set_json_string(value, "quality", &request.quality);
            }
            if request.size.is_empty() {
                request.size = "1024x1024".to_string();
                set_json_string(value, "size", &request.size);
            }
        }
        "gpt-image-1" => {
            if request.quality.is_empty() {
                request.quality = "auto".to_string();
                set_json_string(value, "quality", &request.quality);
            }
        }
        _ => {}
    }

    if request.n.unwrap_or(0) == 0 {
        request.n = Some(1);
        value["n"] = JsonValue::Number(serde_json::Number::from(1));
    }

    Ok(())
}

fn set_json_string(value: &mut JsonValue, key: &str, data: &str) {
    value[key] = JsonValue::String(data.to_string());
}

pub(crate) fn content_type_header(headers: &HeaderMap) -> Option<&HeaderValue> {
    headers.get(header::CONTENT_TYPE)
}

fn is_multipart_content_type(headers: &HeaderMap) -> bool {
    content_type_header(headers)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .to_ascii_lowercase()
                .contains("multipart/form-data")
        })
}

fn multipart_string_field(
    body: &[u8],
    headers: &HeaderMap,
    field: &str,
) -> Result<Option<String>, String> {
    let Some((start, end)) = multipart_field_range(body, headers, field)? else {
        return Ok(None);
    };
    let value = std::str::from_utf8(&body[start..end])
        .map_err(|err| format!("multipart field {field} is not utf-8: {err}"))?;
    Ok(Some(value.trim().to_string()))
}

pub(crate) fn rewrite_multipart_model_field(
    body: Bytes,
    headers: &HeaderMap,
    upstream_model: &str,
) -> Result<Bytes, String> {
    let Some((start, end)) = multipart_field_range(&body, headers, "model")? else {
        return Err("multipart image edit form must contain model field".to_string());
    };
    if &body[start..end] == upstream_model.as_bytes() {
        return Ok(body);
    }

    let mut rewritten =
        Vec::with_capacity(body.len() + upstream_model.len().saturating_sub(end - start));
    rewritten.extend_from_slice(&body[..start]);
    rewritten.extend_from_slice(upstream_model.as_bytes());
    rewritten.extend_from_slice(&body[end..]);
    Ok(Bytes::from(rewritten))
}

fn multipart_field_range(
    body: &[u8],
    headers: &HeaderMap,
    field: &str,
) -> Result<Option<(usize, usize)>, String> {
    let content_type = content_type_header(headers)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "multipart request missing content-type".to_string())?;
    let boundary = multipart_boundary(content_type)
        .ok_or_else(|| "multipart request missing boundary".to_string())?;
    let marker = {
        let mut marker = Vec::with_capacity(boundary.len() + 2);
        marker.extend_from_slice(b"--");
        marker.extend_from_slice(boundary.as_bytes());
        marker
    };

    let Some(mut pos) = find_bytes(body, &marker) else {
        return Ok(None);
    };

    loop {
        let mut part_start = pos + marker.len();
        if body.get(part_start..part_start + 2) == Some(b"--") {
            return Ok(None);
        }
        if body.get(part_start..part_start + 2) == Some(b"\r\n") {
            part_start += 2;
        } else if body.get(part_start) == Some(&b'\n') {
            part_start += 1;
        }

        let Some(next_rel) = find_bytes(&body[part_start..], &marker) else {
            return Ok(None);
        };
        let part_end = part_start + next_rel;
        let part = &body[part_start..part_end];
        let Some((header_len, separator_len)) = multipart_header_separator(part) else {
            pos = part_end;
            continue;
        };
        let header_bytes = &part[..header_len];
        if multipart_headers_match_field(header_bytes, field) {
            let start = part_start + header_len + separator_len;
            let mut end = part_end;
            if end >= 2 && &body[end - 2..end] == b"\r\n" {
                end -= 2;
            } else if end >= 1 && body[end - 1] == b'\n' {
                end -= 1;
            }
            return Ok(Some((start, end)));
        }
        pos = part_end;
    }
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let value = value.trim();
        return Some(
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value)
                .to_string(),
        );
    }
    None
}

fn multipart_header_separator(part: &[u8]) -> Option<(usize, usize)> {
    find_bytes(part, b"\r\n\r\n")
        .map(|idx| (idx, 4))
        .or_else(|| find_bytes(part, b"\n\n").map(|idx| (idx, 2)))
}

fn multipart_headers_match_field(headers: &[u8], field: &str) -> bool {
    let Ok(headers) = std::str::from_utf8(headers) else {
        return false;
    };
    let quoted = format!("name=\"{field}\"");
    let raw = format!("name={field}");
    headers.lines().any(|line| {
        let Some((name, value)) = line.trim().split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case("content-disposition")
            && (value.contains(&quoted) || value.contains(&raw))
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_form_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_image_json_request_with_new_api_defaults() {
        let req = GatewayRequest {
            headers: HeaderMap::new(),
            path:    "/v1/images/generations".to_string(),
            body:    Bytes::from_static(br#"{"model":"gpt-image-1","prompt":"draw"}"#),
        };

        let parsed = parse_openai_image_request(&req).expect("image request should parse");

        assert_eq!(parsed.kind, OpenAiImageRouteKind::Generations);
        assert_eq!(parsed.model, "gpt-image-1");
        let OpenAiImagePayload::Json { request, value } = parsed.payload else {
            panic!("expected json image payload");
        };
        assert_eq!(request.n, Some(1));
        assert_eq!(request.quality, "auto");
        assert_eq!(value["n"], JsonValue::Number(serde_json::Number::from(1)));
        assert_eq!(value["quality"], JsonValue::String("auto".to_string()));
    }

    #[test]
    fn parses_and_rewrites_multipart_image_edit_model() {
        let boundary = "halolake-boundary";
        let body = Bytes::from(format!(
            "--{boundary}\r\nContent-Disposition: form-data; \
             name=\"model\"\r\n\r\ngpt-image-1\r\n--{boundary}\r\nContent-Disposition: form-data; \
             name=\"prompt\"\r\n\r\nedit it\r\n--{boundary}--\r\n"
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
        );

        let model = multipart_string_field(&body, &headers, "model")
            .expect("multipart parse should succeed")
            .expect("model field should exist");
        assert_eq!(model, "gpt-image-1");

        let rewritten = rewrite_multipart_model_field(body, &headers, "imagen-4")
            .expect("multipart model rewrite should succeed");
        let rewritten_model = multipart_string_field(&rewritten, &headers, "model")
            .expect("rewritten multipart parse should succeed")
            .expect("rewritten model field should exist");
        assert_eq!(rewritten_model, "imagen-4");
    }
}
