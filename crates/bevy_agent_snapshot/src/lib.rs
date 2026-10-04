#![doc = include_str!("../README.md")]

mod builtins;
mod capture;
mod checksum;
mod model;
mod plugin;
mod registry;
mod restore;
mod serialization;
mod store;

pub use capture::capture_snapshot;
pub use checksum::{checksum_snapshot, checksum_snapshot_with_remap_exclusions};
pub use model::*;
pub use plugin::{AgentSnapshotPlugin, maybe_take_snapshot};
pub use registry::{
    ComponentRegistration, ResourceRegistration, SNAPSHOT_SCHEMA_VERSION, SnapshotAppExt,
    SnapshotRegistrationResult, SnapshotRegistry, SnapshotType, StableIdRemapHook,
};
pub use restore::*;
pub use store::*;

#[cfg(test)]
mod tests;
