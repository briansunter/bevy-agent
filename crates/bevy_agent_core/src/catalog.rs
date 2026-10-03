//! Explicit game capabilities and cached JSON Schema validation.

use crate::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CustomActionSchema {
    pub name: String,
    pub schema: serde_json::Value,
}

#[derive(Resource, Clone, Default, Serialize)]
pub struct AgentActionCatalog {
    supported_actions: BTreeSet<AgentActionKind>,
    custom_actions: BTreeMap<String, CustomActionSchema>,
    #[serde(skip)]
    validators: BTreeMap<String, std::sync::Arc<jsonschema::Validator>>,
}

impl std::fmt::Debug for AgentActionCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentActionCatalog")
            .field("supported_actions", &self.supported_actions)
            .field("custom_actions", &self.custom_actions)
            .finish()
    }
}

impl<'de> Deserialize<'de> for AgentActionCatalog {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CatalogData {
            supported_actions: BTreeSet<AgentActionKind>,
            custom_actions: BTreeMap<String, CustomActionSchema>,
        }
        let data = CatalogData::deserialize(deserializer)?;
        let mut catalog = Self {
            supported_actions: data.supported_actions,
            ..Self::default()
        };
        for (name, entry) in data.custom_actions {
            if name != entry.name {
                return Err(serde::de::Error::custom("custom schema key/name mismatch"));
            }
            catalog
                .register_custom_action_schema(name, entry.schema)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(catalog)
    }
}

impl AgentActionCatalog {
    pub fn register_custom_action_schema(
        &mut self,
        name: impl Into<String>,
        schema: serde_json::Value,
    ) -> ControlResult<()> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(AgentControlError::InvalidSchema(
                "custom action name is empty".into(),
            ));
        }
        let validator = crate::schema::compile_schema(&schema)?;
        self.validators.insert(name.clone(), validator);
        self.custom_actions
            .insert(name.clone(), CustomActionSchema { name, schema });
        Ok(())
    }

    pub fn set_supported_actions(&mut self, actions: impl IntoIterator<Item = AgentActionKind>) {
        self.supported_actions = actions.into_iter().collect();
    }

    #[must_use]
    pub fn supported_actions(&self) -> &BTreeSet<AgentActionKind> {
        &self.supported_actions
    }

    #[must_use]
    pub fn custom_actions(&self) -> &BTreeMap<String, CustomActionSchema> {
        &self.custom_actions
    }

    #[must_use]
    pub fn supports(&self, action: AgentActionKind) -> bool {
        self.supported_actions.contains(&action)
    }

    pub fn validate(&self) -> ControlResult<()> {
        if self.supported_actions.is_empty() {
            return Err(AgentControlError::InvalidIntegration(
                "supported actions must be explicitly configured".into(),
            ));
        }
        if self.supports(AgentActionKind::Custom) == self.custom_actions.is_empty() {
            return Err(AgentControlError::InvalidIntegration(
                "Custom support requires registered schemas, and schemas require Custom support"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn validate_action(&self, action: &AgentAction) -> ControlResult<()> {
        self.validate()?;
        if !self.supports(action.kind()) {
            return Err(AgentControlError::InvalidAction(format!(
                "unsupported action kind {:?}",
                action.kind()
            )));
        }
        match action {
            AgentAction::Move { x, y } => {
                if !x.is_finite() || !y.is_finite() || x.abs() > 1.0 || y.abs() > 1.0 {
                    return Err(AgentControlError::InvalidAction(
                        "Move x/y must be finite in [-1, 1]".into(),
                    ));
                }
            }
            AgentAction::Look {
                yaw_delta,
                pitch_delta,
            } => {
                if !yaw_delta.is_finite()
                    || !pitch_delta.is_finite()
                    || yaw_delta.abs() > LOOK_YAW_DELTA_LIMIT_RADIANS
                    || pitch_delta.abs() > LOOK_PITCH_DELTA_LIMIT_RADIANS
                {
                    return Err(AgentControlError::InvalidAction(
                        "Look deltas exceed the finite yaw/pitch limits".into(),
                    ));
                }
            }
            AgentAction::Custom { value } => {
                if !self
                    .validators
                    .values()
                    .any(|validator| validator.is_valid(value))
                {
                    return Err(AgentControlError::InvalidAction(
                        "custom payload matches no registered JSON schema".into(),
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Pure validation shared by direct, scheduled, remote, and replay actions.
pub fn validate_action_against_catalog(
    catalog: &AgentActionCatalog,
    action: &AgentAction,
) -> ControlResult<()> {
    catalog.validate_action(action)
}

#[derive(Resource, Clone, Default, Serialize)]
pub struct AgentObservationCatalog {
    supported_modes: BTreeSet<ObservationMode>,
    schema: Option<serde_json::Value>,
    #[serde(skip)]
    validator: Option<std::sync::Arc<jsonschema::Validator>>,
}

impl std::fmt::Debug for AgentObservationCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentObservationCatalog")
            .field("supported_modes", &self.supported_modes)
            .field("schema", &self.schema)
            .finish()
    }
}

impl<'de> Deserialize<'de> for AgentObservationCatalog {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CatalogData {
            supported_modes: BTreeSet<ObservationMode>,
            schema: Option<serde_json::Value>,
        }
        let data = CatalogData::deserialize(deserializer)?;
        let mut catalog = Self {
            supported_modes: data.supported_modes,
            ..Self::default()
        };
        if let Some(schema) = data.schema {
            catalog
                .set_schema(schema)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(catalog)
    }
}

impl AgentObservationCatalog {
    pub fn set_supported_modes(&mut self, modes: impl IntoIterator<Item = ObservationMode>) {
        self.supported_modes = modes.into_iter().collect();
    }
    #[must_use]
    pub fn supported_modes(&self) -> &BTreeSet<ObservationMode> {
        &self.supported_modes
    }
    #[must_use]
    pub fn supports(&self, mode: &ObservationMode) -> bool {
        self.supported_modes.contains(mode)
    }
    #[must_use]
    pub fn schema(&self) -> Option<&serde_json::Value> {
        self.schema.as_ref()
    }

    /// A game schema describes the complete serialized observation envelope.
    /// Compilation succeeds before the current schema/validator are replaced.
    pub fn set_schema(&mut self, schema: serde_json::Value) -> ControlResult<()> {
        let validator = crate::schema::compile_schema(&schema)?;
        self.schema = Some(schema);
        self.validator = Some(validator);
        Ok(())
    }

    pub fn validate(&self) -> ControlResult<()> {
        if self.supported_modes.is_empty() {
            return Err(AgentControlError::InvalidIntegration(
                "supported observation modes must be explicitly configured".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_mode(&self, mode: &ObservationMode) -> ControlResult<()> {
        self.validate()?;
        if !self.supports(mode) {
            return Err(AgentControlError::UnsupportedObservationMode(format!(
                "{mode:?} is not supported by this game"
            )));
        }
        Ok(())
    }

    pub fn validate_observation(
        &self,
        mode: &ObservationMode,
        observation: &Observation,
    ) -> ControlResult<()> {
        self.validate_mode(mode)?;
        if let Some(validator) = &self.validator {
            let value = serde_json::to_value(observation).map_err(|error| {
                AgentControlError::InvalidIntegration(format!(
                    "invalid observation serialization: {error}"
                ))
            })?;
            if let Err(error) = validator.validate(&value) {
                return Err(AgentControlError::InvalidIntegration(format!(
                    "observation violates declared schema: {error}"
                )));
            }
        }
        Ok(())
    }
}
