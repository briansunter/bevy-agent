//! Game integration builders, extractor resources and constructor validation.

use crate::*;
use bevy::ecs::schedule::Schedules;

#[derive(
    Resource, Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentMetadata {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
}

impl EnvironmentMetadata {
    pub fn validate(&self) -> ControlResult<()> {
        if self.name.trim().is_empty() || self.version.trim().is_empty() {
            return Err(AgentControlError::InvalidIntegration(
                "environment name and version must be explicitly configured".into(),
            ));
        }
        Ok(())
    }
}

type ObservationExtractorFn = dyn Fn(&mut World, ObservationMode) -> Observation + Send + Sync;
type ChecksumExtractorFn = dyn Fn(&mut World) -> EnvironmentChecksum + Send + Sync;

pub struct AgentObservationExtractor {
    extract: Box<ObservationExtractorFn>,
}

impl AgentObservationExtractor {
    pub fn new<F>(extract: F) -> Self
    where
        F: Fn(&mut World, ObservationMode) -> Observation + Send + Sync + 'static,
    {
        Self {
            extract: Box::new(extract),
        }
    }

    pub fn extract(&self, world: &mut World, mode: ObservationMode) -> Observation {
        (self.extract)(world, mode)
    }
}

impl Resource for AgentObservationExtractor {}

pub struct AgentChecksumExtractor {
    extract: Box<ChecksumExtractorFn>,
}

impl AgentChecksumExtractor {
    pub fn new<F>(extract: F) -> Self
    where
        F: Fn(&mut World) -> EnvironmentChecksum + Send + Sync + 'static,
    {
        Self {
            extract: Box::new(extract),
        }
    }

    pub fn extract(&self, world: &mut World) -> EnvironmentChecksum {
        (self.extract)(world)
    }
}

impl Resource for AgentChecksumExtractor {}

pub trait AgentControlAppExt {
    fn insert_observation_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World, ObservationMode) -> Observation + Send + Sync + 'static;

    fn insert_checksum_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World) -> EnvironmentChecksum + Send + Sync + 'static;

    fn register_custom_action_schema(
        &mut self,
        name: impl Into<String>,
        schema: serde_json::Value,
    ) -> ControlResult<&mut Self>;

    fn set_environment_metadata(
        &mut self,
        name: impl Into<String>,
        version: impl Into<String>,
        description: Option<String>,
    ) -> &mut Self;

    fn set_supported_actions(
        &mut self,
        actions: impl IntoIterator<Item = AgentActionKind>,
    ) -> &mut Self;

    fn set_supported_observation_modes(
        &mut self,
        modes: impl IntoIterator<Item = ObservationMode>,
    ) -> &mut Self;

    fn set_observation_schema(&mut self, schema: serde_json::Value) -> ControlResult<&mut Self>;
}

impl AgentControlAppExt for App {
    fn insert_observation_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World, ObservationMode) -> Observation + Send + Sync + 'static,
    {
        self.insert_resource(AgentObservationExtractor::new(extract))
    }

    fn insert_checksum_extractor<F>(&mut self, extract: F) -> &mut Self
    where
        F: Fn(&mut World) -> EnvironmentChecksum + Send + Sync + 'static,
    {
        self.insert_resource(AgentChecksumExtractor::new(extract))
    }

    fn register_custom_action_schema(
        &mut self,
        name: impl Into<String>,
        schema: serde_json::Value,
    ) -> ControlResult<&mut Self> {
        if !self.world().contains_resource::<AgentActionCatalog>() {
            self.init_resource::<AgentActionCatalog>();
        }
        self.world_mut()
            .resource_mut::<AgentActionCatalog>()
            .register_custom_action_schema(name, schema)?;
        Ok(self)
    }

    fn set_environment_metadata(
        &mut self,
        name: impl Into<String>,
        version: impl Into<String>,
        description: Option<String>,
    ) -> &mut Self {
        self.insert_resource(EnvironmentMetadata {
            name: name.into(),
            version: version.into(),
            description,
        })
    }

    fn set_supported_actions(
        &mut self,
        actions: impl IntoIterator<Item = AgentActionKind>,
    ) -> &mut Self {
        if !self.world().contains_resource::<AgentActionCatalog>() {
            self.init_resource::<AgentActionCatalog>();
        }
        self.world_mut()
            .resource_mut::<AgentActionCatalog>()
            .set_supported_actions(actions);
        self
    }

    fn set_supported_observation_modes(
        &mut self,
        modes: impl IntoIterator<Item = ObservationMode>,
    ) -> &mut Self {
        self.init_resource::<AgentObservationCatalog>();
        self.world_mut()
            .resource_mut::<AgentObservationCatalog>()
            .set_supported_modes(modes);
        self
    }

    fn set_observation_schema(&mut self, schema: serde_json::Value) -> ControlResult<&mut Self> {
        self.init_resource::<AgentObservationCatalog>();
        self.world_mut()
            .resource_mut::<AgentObservationCatalog>()
            .set_schema(schema)?;
        Ok(self)
    }
}
fn require<'a, T: Resource>(world: &'a World, name: &'static str) -> ControlResult<&'a T> {
    world
        .get_resource::<T>()
        .ok_or(AgentControlError::MissingResource(name))
}

pub(crate) fn validate_resources(world: &World) -> ControlResult<()> {
    require::<SimClock>(world, "SimClock")?.validate()?;
    require::<StableIdAllocator>(world, "StableIdAllocator")?;
    require::<DeterministicRng>(world, "DeterministicRng")?;
    require::<AgentControlState>(world, "AgentControlState")?;
    require::<AgentActionQueue>(world, "AgentActionQueue")?;
    require::<CurrentInputFrame>(world, "CurrentInputFrame")?;
    require::<RewardState>(world, "RewardState")?;
    require::<EpisodeState>(world, "EpisodeState")?;
    require::<LastStepResponse>(world, "LastStepResponse")?;
    require::<ExecutionContext>(world, "ExecutionContext")?;
    require::<AgentTickFailure>(world, "AgentTickFailure")?;
    require::<AgentObservationExtractor>(world, "AgentObservationExtractor")?;
    require::<AgentChecksumExtractor>(world, "AgentChecksumExtractor")?;
    require::<EnvironmentMetadata>(world, "EnvironmentMetadata")?.validate()?;
    require::<AgentActionCatalog>(world, "AgentActionCatalog")?.validate()?;
    let observations = require::<AgentObservationCatalog>(world, "AgentObservationCatalog")?;
    observations.validate_mode(&require::<ObservationConfig>(world, "ObservationConfig")?.mode)?;
    Ok(())
}

pub(crate) fn validate_runtime_config(world: &World) -> ControlResult<()> {
    validate_resources(world)?;
    let schedules = require::<Schedules>(world, "Schedules")?;
    for (installed, name) in [
        (schedules.contains(AgentReset), "AgentReset"),
        (schedules.contains(AgentDecision), "AgentDecision"),
        (schedules.contains(AgentPreTick), "AgentPreTick"),
        (schedules.contains(AgentTick), "AgentTick"),
        (schedules.contains(AgentPostTick), "AgentPostTick"),
        (schedules.contains(AgentFinalize), "AgentFinalize"),
    ] {
        if !installed {
            return Err(AgentControlError::MissingSchedule(name));
        }
    }
    Ok(())
}

/// Checks resources, schedules, extractors, catalogs, metadata and imported
/// future actions before constructing a controllable environment.
pub fn validate_integration(world: &World) -> ControlResult<()> {
    validate_runtime_config(world)?;
    let catalog = world.resource::<AgentActionCatalog>();
    let tick = world.resource::<SimClock>().tick;
    for scheduled in world.resource::<AgentActionQueue>().iter() {
        validate_future_action(catalog, tick, scheduled)?;
    }
    Ok(())
}
