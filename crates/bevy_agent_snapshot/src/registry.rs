use std::any::TypeId;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hasher;

use anyhow::{Context, Result, anyhow};
use bevy::ecs::world::{EntityRef, EntityWorldMut};
use bevy::prelude::*;
use bevy_agent_core::{EnvironmentMetadata, StableEntityId, StableHasher};
use serde::{Deserialize, Serialize};

use crate::{ComponentSnapshot, ResourceSnapshot, SnapshotMetadata};

/// Current snapshot contract: explicit stable type identities and versions.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 3;

/// Persisted identity of a registered component or resource.
///
/// IDs belong to the integrating game's wire contract, independently of Rust
/// module names. Change the version whenever serialized fields or their
/// meaning change. IDs must be nonempty qualified ASCII identifiers; versions
/// start at one. A registry rejects identities shared by different Rust types.
pub trait SnapshotType {
    const TYPE_ID: &'static str;
    const SCHEMA_VERSION: u32;
}

/// Registration result exposed so exported macros require no caller dependency.
pub type SnapshotRegistrationResult<T> = Result<T>;

pub(crate) type ComponentCaptureFn = fn(&EntityRef<'_>) -> Result<Option<ComponentSnapshot>>;
pub(crate) type ComponentRestoreFn = fn(&mut EntityWorldMut<'_>, &serde_json::Value) -> Result<()>;
pub(crate) type ComponentValidateFn = fn(&serde_json::Value) -> Result<()>;
/// Fix-up hook run after all restored entities and components are installed.
pub type StableIdRemapHook = dyn Fn(&mut World, &HashMap<StableEntityId, Entity>);
pub(crate) type ResourceCaptureFn = fn(&World) -> Result<Option<ResourceSnapshot>>;
pub(crate) type ResourceRestoreFn = fn(&mut World, &serde_json::Value) -> Result<()>;
pub(crate) type ResourceRemoveFn = fn(&mut World);
pub(crate) type ResourceValidateFn = fn(&serde_json::Value) -> Result<()>;

#[derive(Clone)]
pub struct ComponentRegistration {
    pub(crate) type_id: &'static str,
    pub(crate) schema_version: u32,
    rust_type_id: TypeId,
    rust_type_name: &'static str,
    pub(crate) capture: ComponentCaptureFn,
    pub(crate) restore: ComponentRestoreFn,
    pub(crate) validate: ComponentValidateFn,
}

impl ComponentRegistration {
    pub fn type_id(&self) -> &'static str {
        self.type_id
    }
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

#[derive(Clone)]
pub struct ResourceRegistration {
    pub(crate) type_id: &'static str,
    pub(crate) schema_version: u32,
    rust_type_id: TypeId,
    rust_type_name: &'static str,
    pub(crate) capture: ResourceCaptureFn,
    pub(crate) restore: ResourceRestoreFn,
    pub(crate) remove: ResourceRemoveFn,
    pub(crate) required: bool,
    pub(crate) validate: ResourceValidateFn,
}

impl ResourceRegistration {
    pub fn type_id(&self) -> &'static str {
        self.type_id
    }
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
    pub fn is_required(&self) -> bool {
        self.required
    }
}

/// Live registration owner. Serializer tables cannot be modified externally.
#[derive(Resource, Clone, Default)]
pub struct SnapshotRegistry {
    pub(crate) component_serializers: BTreeMap<&'static str, ComponentRegistration>,
    pub(crate) resource_serializers: BTreeMap<&'static str, ResourceRegistration>,
}

pub trait SnapshotAppExt {
    fn register_snapshot_component<T>(&mut self) -> Result<&mut Self>
    where
        T: Component + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static;

    fn register_snapshot_resource<T>(&mut self) -> Result<&mut Self>
    where
        T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static;

    /// Require this resource at capture and restore boundaries.
    fn register_required_snapshot_resource<T>(&mut self) -> Result<&mut Self>
    where
        T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static;

    fn set_snapshot_metadata(
        &mut self,
        game_id: impl Into<String>,
        game_version: impl Into<String>,
    ) -> &mut Self;
}

#[macro_export]
macro_rules! register_snapshot_components {
    ($app:expr $(, $component:ty)+ $(,)?) => {{
        (|| -> $crate::SnapshotRegistrationResult<()> {
            let world = ($app).world_mut();
            let mut registry = world.get_resource::<$crate::SnapshotRegistry>().cloned().unwrap_or_default();
            $(registry.register_component::<$component>()?;)+
            world.insert_resource(registry);
            Ok(())
        })()
    }};
}

#[macro_export]
macro_rules! register_snapshot_resources {
    ($app:expr $(, $resource:ty)+ $(,)?) => {{
        (|| -> $crate::SnapshotRegistrationResult<()> {
            let world = ($app).world_mut();
            let mut registry = world.get_resource::<$crate::SnapshotRegistry>().cloned().unwrap_or_default();
            $(registry.register_resource::<$resource>()?;)+
            world.insert_resource(registry);
            Ok(())
        })()
    }};
}

impl SnapshotAppExt for App {
    fn register_snapshot_component<T>(&mut self) -> Result<&mut Self>
    where
        T: Component + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        self.init_resource::<SnapshotRegistry>();
        self.world_mut()
            .resource_mut::<SnapshotRegistry>()
            .register_component::<T>()?;
        Ok(self)
    }

    fn register_snapshot_resource<T>(&mut self) -> Result<&mut Self>
    where
        T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        self.init_resource::<SnapshotRegistry>();
        self.world_mut()
            .resource_mut::<SnapshotRegistry>()
            .register_resource::<T>()?;
        Ok(self)
    }

    fn register_required_snapshot_resource<T>(&mut self) -> Result<&mut Self>
    where
        T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        self.init_resource::<SnapshotRegistry>();
        self.world_mut()
            .resource_mut::<SnapshotRegistry>()
            .register_required_resource::<T>()?;
        Ok(self)
    }

    fn set_snapshot_metadata(
        &mut self,
        game_id: impl Into<String>,
        game_version: impl Into<String>,
    ) -> &mut Self {
        let mut metadata = self
            .world()
            .get_resource::<SnapshotMetadata>()
            .cloned()
            .unwrap_or_default();
        metadata.game_id = game_id.into();
        metadata.game_version = game_version.into();
        self.insert_resource(EnvironmentMetadata {
            name: metadata.game_id.clone(),
            version: metadata.game_version.clone(),
            description: self
                .world()
                .get_resource::<EnvironmentMetadata>()
                .and_then(|environment| environment.description.clone()),
        });
        self.insert_resource(metadata)
    }
}

impl SnapshotRegistry {
    pub fn components(&self) -> impl ExactSizeIterator<Item = &ComponentRegistration> {
        self.component_serializers.values()
    }
    pub fn resources(&self) -> impl ExactSizeIterator<Item = &ResourceRegistration> {
        self.resource_serializers.values()
    }
    pub fn component(&self, type_id: &str) -> Option<&ComponentRegistration> {
        self.component_serializers.get(type_id)
    }
    pub fn resource(&self, type_id: &str) -> Option<&ResourceRegistration> {
        self.resource_serializers.get(type_id)
    }

    /// Register one identity atomically; duplicate registration of the same
    /// Rust type is idempotent, while conflicting identities leave it unchanged.
    pub fn register_component<T>(&mut self) -> Result<()>
    where
        T: Component + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        self.validate_identity::<T>()?;
        self.component_serializers
            .entry(T::TYPE_ID)
            .or_insert(ComponentRegistration {
                type_id: T::TYPE_ID,
                schema_version: T::SCHEMA_VERSION,
                rust_type_id: TypeId::of::<T>(),
                rust_type_name: std::any::type_name::<T>(),
                capture: capture_component::<T>,
                restore: restore_component::<T>,
                validate: validate_component::<T>,
            });
        Ok(())
    }

    pub fn register_resource<T>(&mut self) -> Result<()>
    where
        T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        self.validate_identity::<T>()?;
        // Idempotence also preserves a plugin's required-resource policy.
        self.resource_serializers
            .entry(T::TYPE_ID)
            .or_insert(ResourceRegistration {
                type_id: T::TYPE_ID,
                schema_version: T::SCHEMA_VERSION,
                rust_type_id: TypeId::of::<T>(),
                rust_type_name: std::any::type_name::<T>(),
                capture: capture_resource::<T>,
                restore: restore_resource::<T>,
                remove: remove_resource::<T>,
                required: false,
                validate: validate_resource::<T>,
            });
        Ok(())
    }

    pub fn register_required_resource<T>(&mut self) -> Result<()>
    where
        T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
    {
        self.register_resource::<T>()?;
        self.resource_serializers
            .get_mut(T::TYPE_ID)
            .expect("just registered resource")
            .required = true;
        Ok(())
    }

    fn validate_identity<T: SnapshotType + 'static>(&self) -> Result<()> {
        let id = T::TYPE_ID;
        if id.is_empty()
            || id.len() > 256
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
        {
            return Err(anyhow!(
                "invalid snapshot type ID {id:?}: expected a nonempty qualified ASCII identifier"
            ));
        }
        if T::SCHEMA_VERSION == 0 {
            return Err(anyhow!(
                "snapshot type {id} schema version must be greater than zero"
            ));
        }
        let expected = TypeId::of::<T>();
        let conflict = self
            .component_serializers
            .get(id)
            .map(|registration| (registration.rust_type_id, registration.rust_type_name))
            .or_else(|| {
                self.resource_serializers
                    .get(id)
                    .map(|registration| (registration.rust_type_id, registration.rust_type_name))
            });
        if let Some((existing, name)) = conflict
            && existing != expected
        {
            return Err(anyhow!(
                "snapshot type ID {id} is already registered for {name}; cannot register {}",
                std::any::type_name::<T>()
            ));
        }
        Ok(())
    }

    /// Hash the declared wire contract, never Rust names or crate versions.
    #[must_use]
    pub fn schema_hash(&self) -> String {
        let mut hasher = StableHasher::new();
        hasher.write_u32(SNAPSHOT_SCHEMA_VERSION);
        let mut components = self.components().collect::<Vec<_>>();
        components.sort_unstable_by_key(|registration| registration.type_id);
        hasher.write_string("components");
        hasher.write_u64(components.len() as u64);
        for registration in components {
            hasher.write_string(registration.type_id);
            hasher.write_u32(registration.schema_version);
        }
        let mut resources = self.resources().collect::<Vec<_>>();
        resources.sort_unstable_by_key(|registration| registration.type_id);
        hasher.write_string("resources");
        hasher.write_u64(resources.len() as u64);
        for registration in resources {
            hasher.write_string(registration.type_id);
            hasher.write_u32(registration.schema_version);
            hasher.write_u8(u8::from(registration.required));
        }
        format!("{:016x}", hasher.finish_hash())
    }
}

fn capture_component<T>(entity: &EntityRef<'_>) -> Result<Option<ComponentSnapshot>>
where
    T: Component + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    entity
        .get::<T>()
        .map(|component| {
            crate::serialization::to_value(component)
                .map(|value| ComponentSnapshot {
                    type_id: T::TYPE_ID.to_string(),
                    schema_version: T::SCHEMA_VERSION,
                    value,
                })
                .context("serializing component")
        })
        .transpose()
}

fn restore_component<T>(entity: &mut EntityWorldMut<'_>, value: &serde_json::Value) -> Result<()>
where
    T: Component + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    let component = serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing component {}", T::TYPE_ID))?;
    entity.insert(component);
    Ok(())
}

fn capture_resource<T>(world: &World) -> Result<Option<ResourceSnapshot>>
where
    T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    world
        .get_resource::<T>()
        .map(|resource| {
            crate::serialization::to_value(resource)
                .map(|value| ResourceSnapshot {
                    type_id: T::TYPE_ID.to_string(),
                    schema_version: T::SCHEMA_VERSION,
                    value,
                })
                .context("serializing resource")
        })
        .transpose()
}

fn restore_resource<T>(world: &mut World, value: &serde_json::Value) -> Result<()>
where
    T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    let resource = serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing resource {}", T::TYPE_ID))?;
    world.insert_resource(resource);
    Ok(())
}

fn validate_component<T>(value: &serde_json::Value) -> Result<()>
where
    T: Component + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    let decoded = serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing component {}", T::TYPE_ID))?;
    if crate::serialization::to_value(&decoded)? != *value {
        return Err(anyhow!(
            "component {} does not round trip its snapshot payload",
            T::TYPE_ID
        ));
    }
    Ok(())
}

fn validate_resource<T>(value: &serde_json::Value) -> Result<()>
where
    T: Resource + SnapshotType + Clone + Serialize + for<'de> Deserialize<'de> + 'static,
{
    let decoded = serde_json::from_value::<T>(value.clone())
        .with_context(|| format!("deserializing resource {}", T::TYPE_ID))?;
    if crate::serialization::to_value(&decoded)? != *value {
        return Err(anyhow!(
            "resource {} does not round trip its snapshot payload",
            T::TYPE_ID
        ));
    }
    Ok(())
}

fn remove_resource<T: Resource + 'static>(world: &mut World) {
    world.remove_resource::<T>();
}
