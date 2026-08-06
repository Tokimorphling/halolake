use crate::config::WorkflowConfig;
use anyhow::{Context, Result, bail};
use halolake_tokiame_protocol::{DispatchJob, WorkflowCapability};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct WorkflowCatalog {
    entries: BTreeMap<(String, String), WorkflowDefinition>,
}

#[derive(Debug, Clone)]
struct WorkflowDefinition {
    config:   WorkflowConfig,
    template: JsonValue,
}

impl WorkflowCatalog {
    pub fn load(configs: &[WorkflowConfig]) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for config in configs {
            if config.id.trim().is_empty()
                || config.version.trim().is_empty()
                || config.model.trim().is_empty()
            {
                bail!("workflow id, version and model are required");
            }
            let raw = std::fs::read_to_string(&config.file)
                .with_context(|| format!("read workflow {}", config.file.display()))?;
            let template: JsonValue = serde_json::from_str(&raw)
                .with_context(|| format!("parse workflow {}", config.file.display()))?;
            if !template.is_object() {
                bail!("workflow {} must be a ComfyUI API JSON object", config.id);
            }
            for pointer in config
                .parameters
                .values()
                .chain(config.inputs.iter().map(|binding| &binding.pointer))
            {
                if !pointer.starts_with('/') || template.pointer(pointer).is_none() {
                    bail!(
                        "workflow {} binding pointer {pointer:?} does not exist in template",
                        config.id
                    );
                }
            }
            let key = (config.id.clone(), config.version.clone());
            if entries
                .insert(key, WorkflowDefinition {
                    config: config.clone(),
                    template,
                })
                .is_some()
            {
                bail!(
                    "duplicate workflow {} version {}",
                    config.id,
                    config.version
                );
            }
        }
        Ok(Self { entries })
    }

    pub fn capabilities(&self) -> Vec<WorkflowCapability> {
        self.entries
            .values()
            .map(|entry| WorkflowCapability {
                id:           entry.config.id.clone(),
                version:      entry.config.version.clone(),
                model:        entry.config.model.clone(),
                output_types: entry.config.output_types.clone(),
            })
            .collect()
    }

    pub fn validate(&self, dispatch: &DispatchJob) -> Result<()> {
        let entry = self.entry(dispatch)?;
        let parameters = dispatch
            .params
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("job parameters must be a JSON object"))?;
        for name in parameters.keys() {
            if !entry.config.parameters.contains_key(name) {
                bail!("parameter {name:?} is not allowlisted by workflow");
            }
        }
        let bound = entry
            .config
            .inputs
            .iter()
            .map(|binding| binding.index)
            .collect::<BTreeSet<_>>();
        if dispatch.inputs.len() != bound.len()
            || bound.iter().copied().ne(0..dispatch.inputs.len())
        {
            bail!(
                "workflow expects {} ordered input artifacts, job supplied {}",
                bound.len(),
                dispatch.inputs.len()
            );
        }
        Ok(())
    }

    pub fn render(&self, dispatch: &DispatchJob, input_names: &[String]) -> Result<JsonValue> {
        self.validate(dispatch)?;
        let entry = self.entry(dispatch)?;
        let mut workflow = entry.template.clone();
        for (name, value) in dispatch.params.as_object().expect("validated object") {
            let pointer = &entry.config.parameters[name];
            *workflow
                .pointer_mut(pointer)
                .expect("binding validated at startup") = value.clone();
        }
        for binding in &entry.config.inputs {
            *workflow
                .pointer_mut(&binding.pointer)
                .expect("binding validated at startup") =
                JsonValue::String(input_names[binding.index].clone());
        }
        Ok(workflow)
    }

    fn entry(&self, dispatch: &DispatchJob) -> Result<&WorkflowDefinition> {
        let entry = self
            .entries
            .get(&(
                dispatch.workflow_id.clone(),
                dispatch.workflow_version.clone(),
            ))
            .ok_or_else(|| anyhow::anyhow!("workflow/version is not installed"))?;
        if dispatch.model != entry.config.model {
            bail!("workflow model does not match the installed definition");
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InputBinding, WorkflowConfig};
    use halolake_tokiame_protocol::PresignedRequest;
    use std::{collections::BTreeMap, path::PathBuf};

    fn catalog() -> WorkflowCatalog {
        let config = WorkflowConfig {
            id:           "image-edit".into(),
            version:      "v1".into(),
            model:        "comfy-edit".into(),
            file:         PathBuf::new(),
            output_types: vec!["image/png".into()],
            parameters:   BTreeMap::from([("prompt".into(), "/6/inputs/text".into())]),
            inputs:       vec![InputBinding {
                index:   0,
                pointer: "/10/inputs/image".into(),
            }],
        };
        WorkflowCatalog {
            entries: BTreeMap::from([(("image-edit".into(), "v1".into()), WorkflowDefinition {
                config,
                template: serde_json::json!({
                    "6":{"inputs":{"text":"default"}},
                    "10":{"inputs":{"image":"default.png"}}
                }),
            })]),
        }
    }

    fn dispatch(params: JsonValue) -> DispatchJob {
        DispatchJob {
            command_id: "command".into(),
            job_id: "job".into(),
            attempt: 1,
            model: "comfy-edit".into(),
            workflow_id: "image-edit".into(),
            workflow_version: "v1".into(),
            params,
            inputs: vec![halolake_tokiame_protocol::JobInput {
                artifact_id:  "input".into(),
                name:         "source.png".into(),
                content_type: "image/png".into(),
                size_bytes:   1,
                sha256:       "0".repeat(64),
                download:     PresignedRequest {
                    method:             "GET".into(),
                    url:                "https://example.invalid/input".into(),
                    headers:            BTreeMap::new(),
                    expires_at_unix_ms: 1,
                },
            }],
        }
    }

    #[test]
    fn renders_only_allowlisted_json_pointer_bindings() {
        let rendered = catalog()
            .render(&dispatch(serde_json::json!({"prompt":"safe prompt"})), &[
                "uploaded.png".into(),
            ])
            .unwrap();
        assert_eq!(rendered.pointer("/6/inputs/text").unwrap(), "safe prompt");
        assert_eq!(
            rendered.pointer("/10/inputs/image").unwrap(),
            "uploaded.png"
        );
    }

    #[test]
    fn rejects_unknown_user_parameters() {
        let error = catalog()
            .render(
                &dispatch(serde_json::json!({"arbitrary_node":"attack"})),
                &["uploaded.png".into()],
            )
            .unwrap_err();
        assert!(error.to_string().contains("not allowlisted"));
    }
}
