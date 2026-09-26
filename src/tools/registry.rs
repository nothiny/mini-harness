use super::spec::{Tool, ToolSpec};
use std::collections::BTreeMap;
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ToolRegistryError {
    #[error("tool name cannot be empty")]
    EmptyName,
    #[error("tool `{name}` is already registered")]
    Duplicate { name: String },
    #[error("tool `{name}` specification exceeds the {max_bytes} byte limit")]
    SpecTooLarge { name: String, max_bytes: usize },
    #[error("unknown tool `{name}`")]
    Unknown { name: String },
}

#[derive(Clone)]
struct RegisteredTool {
    tool: Arc<dyn Tool>,
    spec: ToolSpec,
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}
impl ToolRegistry {
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> Result<(), ToolRegistryError> {
        let spec = tool.spec();
        let name = spec.name.0.clone();
        if name.trim().is_empty() {
            return Err(ToolRegistryError::EmptyName);
        }
        if self.tools.contains_key(&name) {
            return Err(ToolRegistryError::Duplicate { name });
        }
        if serde_json::to_vec(&spec)
            .map(|encoded| encoded.len() > super::spec::MAX_TOOL_SPEC_BYTES)
            .unwrap_or(true)
        {
            return Err(ToolRegistryError::SpecTooLarge {
                name,
                max_bytes: super::spec::MAX_TOOL_SPEC_BYTES,
            });
        }
        self.tools.insert(
            name,
            RegisteredTool {
                tool: Arc::new(tool),
                spec,
            },
        );
        Ok(())
    }
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|entry| entry.spec.clone())
            .collect()
    }
    pub fn lookup(&self, name: &str) -> Result<Arc<dyn Tool>, ToolRegistryError> {
        self.tools
            .get(name)
            .map(|entry| Arc::clone(&entry.tool))
            .ok_or_else(|| ToolRegistryError::Unknown { name: name.into() })
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.lookup(name).ok()
    }
}
