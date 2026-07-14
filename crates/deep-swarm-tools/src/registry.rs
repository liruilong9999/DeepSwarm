use std::{collections::BTreeMap, sync::Arc};

use thiserror::Error;

use crate::Tool;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ToolRegistrationError {
    #[error("tool name must not be empty")]
    EmptyName,
    #[error("tool `{0}` is already registered")]
    Duplicate(String),
    #[error("tool `{0}` has an invalid parameter schema: {1}")]
    InvalidSchema(String, String),
}

#[derive(Clone, Default)]
pub struct RunToolRegistry {
    tools: Arc<BTreeMap<String, Arc<dyn Tool>>>,
}

impl RunToolRegistry {
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[derive(Default)]
pub struct RunBuilder {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl RunBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tools(
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
    ) -> Result<Self, ToolRegistrationError> {
        let mut builder = Self::new();
        for tool in tools {
            builder.register_tool(tool)?;
        }
        Ok(builder)
    }

    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistrationError> {
        let name = tool.name().trim();
        if name.is_empty() {
            return Err(ToolRegistrationError::EmptyName);
        }
        if self.tools.contains_key(name) {
            return Err(ToolRegistrationError::Duplicate(name.to_owned()));
        }
        jsonschema::validator_for(&tool.parameters_schema()).map_err(|error| {
            ToolRegistrationError::InvalidSchema(name.to_owned(), error.to_string())
        })?;
        self.tools.insert(name.to_owned(), tool);
        Ok(())
    }

    pub fn build(self) -> RunToolRegistry {
        RunToolRegistry {
            tools: Arc::new(self.tools),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{RunBuilder, ToolRegistrationError, builtin_mock_tools};

    #[test]
    fn registry_rejects_duplicate_names_and_freezes_on_build() {
        let tool = builtin_mock_tools().remove(0);
        let mut builder = RunBuilder::new();
        builder.register_tool(tool.clone()).unwrap();
        assert!(matches!(
            builder.register_tool(tool),
            Err(ToolRegistrationError::Duplicate(_))
        ));
        assert_eq!(builder.build().len(), 1);
    }
}
