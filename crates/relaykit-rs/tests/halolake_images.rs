use relaykit::{image, Error};
use serde_json::json;

#[test]
fn imagen_request_preserves_size_quality_count_and_prompt() -> Result<(), Error> {
    let value = image::openai_image_to_gemini_imagen(&json!({
        "model": "imagen-4.0-generate-001",
        "prompt": "A mountain",
        "n": 2,
        "size": "1792x1024",
        "quality": "high",
    }))?;
    assert_eq!(value["instances"][0]["prompt"], "A mountain");
    assert_eq!(value["parameters"]["sampleCount"], 2);
    assert_eq!(value["parameters"]["aspectRatio"], "16:9");
    assert_eq!(value["parameters"]["imageSize"], "2K");
    assert_eq!(value["parameters"]["personGeneration"], "allow_adult");
    Ok(())
}

#[test]
fn imagen_adapter_rejects_non_image_models_and_empty_predictions() {
    assert!(image::openai_image_to_gemini_imagen(&json!({"model":"gemini-2.5-flash"})).is_err());
    assert!(image::gemini_imagen_to_openai_image(&json!({"predictions":[]}), 123).is_err());
}

#[test]
fn imagen_response_preserves_timestamp_and_filters_safety_rejections() -> Result<(), Error> {
    let value = image::gemini_imagen_to_openai_image(&json!({
        "predictions":[
            {"bytesBase64Encoded":"encoded-image","mimeType":"image/png"},
            {"raiFilteredReason":"safety","bytesBase64Encoded":"rejected"},
        ],
    }), 123)?;
    assert_eq!(value, json!({"created":123,"data":[{"b64_json":"encoded-image"}]}));
    Ok(())
}
