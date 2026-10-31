//! Imagen request and response adapters. Image transport remains host-owned.
use crate::Error;
use serde_json::{Value, json};

pub fn openai_image_to_gemini_imagen(input: &Value) -> Result<Value, Error> {
    if !input["model"].as_str().unwrap_or("").starts_with("imagen") {
        return Err(Error::message("upstream model does not support Imagen"));
    }
    let count = input["n"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(1);
    let mut parameters = json!({
        "sampleCount": count,
        "aspectRatio": aspect_ratio(input["size"].as_str().unwrap_or("")),
        "personGeneration": "allow_adult",
    });
    let quality = input["quality"].as_str().unwrap_or("");
    if !quality.is_empty() {
        parameters["imageSize"] = json!(match quality {
            "hd" | "high" | "2K" => "2K",
            _ => "1K",
        });
    }
    Ok(json!({
        "instances": [{"prompt": input["prompt"].as_str().unwrap_or("")}],
        "parameters": parameters,
    }))
}

pub fn gemini_imagen_to_openai_image(input: &Value, created: i64) -> Result<Value, Error> {
    let predictions = input["predictions"]
        .as_array()
        .filter(|predictions| !predictions.is_empty())
        .ok_or_else(|| Error::message("no images generated"))?;
    let data = predictions
        .iter()
        .filter(|prediction| {
            prediction["raiFilteredReason"]
                .as_str()
                .unwrap_or("")
                .is_empty()
        })
        .map(|prediction| {
            json!({
                "b64_json": prediction["bytesBase64Encoded"].as_str().unwrap_or(""),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"created": created, "data": data}))
}

fn aspect_ratio(size: &str) -> &str {
    let size = size.trim();
    if size.contains(':') {
        return size;
    }
    match size {
        "1536x1024" => "3:2",
        "1024x1536" => "2:3",
        "1024x1792" => "9:16",
        "1792x1024" => "16:9",
        _ => "1:1",
    }
}
