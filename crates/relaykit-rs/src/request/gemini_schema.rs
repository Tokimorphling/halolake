//! Select Gemini's typed Schema or JSON Schema representation without silently
//! losing constraints that the typed form cannot carry.
use super::*;

pub(super) const ALLOWED_FIELDS: &[&str] = &[
    "anyOf",
    "default",
    "description",
    "enum",
    "example",
    "format",
    "items",
    "maxItems",
    "maxLength",
    "maxProperties",
    "maximum",
    "minItems",
    "minLength",
    "minProperties",
    "minimum",
    "nullable",
    "pattern",
    "properties",
    "propertyOrdering",
    "required",
    "title",
    "type",
];

pub(super) fn misfits(schema: &Value) -> Vec<String> {
    let mut found = BTreeSet::new();
    collect_misfits(schema, 0, &mut found);
    found.into_iter().collect()
}

fn collect_misfits(schema: &Value, depth: usize, found: &mut BTreeSet<String>) {
    if depth > 64 {
        found.insert("nesting deeper than 64 levels".into());
        return;
    }
    let Some(schema) = schema.as_object() else {
        if schema.is_boolean() {
            found.insert("boolean schemas".into());
        }
        return;
    };
    for (key, value) in schema {
        if !ALLOWED_FIELDS.contains(&key.as_str()) {
            if key != "additionalProperties" && key != "$schema" {
                found.insert(key.clone());
            }
            continue;
        }
        match key.as_str() {
            "type" if value.is_array() => {
                found.insert("type arrays".into());
            }
            "enum" if arr(value).iter().any(|value| !value.is_string()) => {
                found.insert("non-string enum values".into());
            }
            "properties" => {
                if let Some(properties) = value.as_object() {
                    for property in properties.values() {
                        collect_misfits(property, depth + 1, found);
                    }
                }
            }
            "items" => {
                if let Some(entries) = value.as_array() {
                    found.insert("tuple items".into());
                    for entry in entries {
                        collect_misfits(entry, depth + 1, found);
                    }
                } else {
                    collect_misfits(value, depth + 1, found);
                }
            }
            "anyOf" => {
                for variant in arr(value) {
                    collect_misfits(variant, depth + 1, found);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn function_parameters(
    schema: &Value,
    supports_json_schema: bool,
) -> (&'static str, Value, Vec<String>) {
    let mut misfits = misfits(schema);
    let empty_object = schema["properties"].as_object().is_some_and(Map::is_empty);
    if empty_object {
        for (key, value) in schema.as_object().unwrap() {
            if key == "type" || key == "properties" || key == "required" && arr(value).is_empty() {
                continue;
            }
            if ALLOWED_FIELDS.contains(&key.as_str()) {
                misfits.push("constraints on an empty object".into());
                break;
            }
        }
    }
    if !misfits.is_empty() && supports_json_schema {
        return ("parametersJsonSchema", schema.clone(), vec![]);
    }
    let parameters = if empty_object {
        Value::Null
    } else {
        chat::clean_schema(schema, 0)
    };
    ("parameters", parameters, misfits)
}
