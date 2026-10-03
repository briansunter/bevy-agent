use bevy::prelude::*;
use bevy_agent_core::{
    AgentActionQueue, AgentSet, CurrentInputFrame, DeterministicRng, EpisodeState,
    ExecutionContext, ObservationConfig, RewardState, SimClock, SnapshotId, StableEntityId,
    StableIdAllocator,
};

use crate::{
    SnapshotAppExt, SnapshotMetadata, SnapshotPolicy, SnapshotRegistry, SnapshotRole,
    SnapshotStore, create_snapshot_with_role,
};
pub struct AgentSnapshotPlugin;

impl Plugin for AgentSnapshotPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SnapshotRegistry>()
            .init_resource::<SnapshotStore>()
            .init_resource::<SnapshotPolicy>()
            .init_resource::<SnapshotMetadata>()
            .register_snapshot_component::<StableEntityId>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<SimClock>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<AgentActionQueue>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<StableIdAllocator>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<DeterministicRng>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<CurrentInputFrame>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<ObservationConfig>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<RewardState>()
            .expect("valid built-in snapshot identity")
            .register_required_snapshot_resource::<EpisodeState>()
            .expect("valid built-in snapshot identity")
            .add_systems(
                bevy_agent_core::AgentFinalize,
                (|world: &mut World| {
                    maybe_take_snapshot(world);
                })
                .in_set(AgentSet::Snapshot),
            );
    }
}
/// Periodic auto-checkpoint.
///
/// Creates a [`SnapshotRole::Periodic`] snapshot when `tick != 0` and
/// `tick % checkpoint_every_ticks == 0`. Never enforces retention: on
/// success returns the new snapshot id so the owner can index it (replay
/// log / timeline topology) and then call [`crate::enforce_retention`] with the
/// live reference set. Retention lives entirely in
/// [`crate::prune_checkpoints_with_refs`]; creation paths must not prune.
pub fn maybe_take_snapshot(world: &mut World) -> Option<SnapshotId> {
    if world
        .get_resource::<bevy_agent_core::AgentTickFailure>()
        .is_some_and(|failure| failure.error().is_some())
    {
        return None;
    }
    if world.get_resource::<ExecutionContext>() == Some(&ExecutionContext::Reconstructing) {
        return None;
    }
    let policy = world.get_resource::<SnapshotPolicy>().cloned()?;
    if policy.checkpoint_every_ticks == 0 {
        return None;
    }

    let tick = world.get_resource::<SimClock>()?.tick;
    if tick == 0 || !tick.is_multiple_of(policy.checkpoint_every_ticks) {
        return None;
    }

    match create_snapshot_with_role(
        world,
        Some(format!("checkpoint-{tick}")),
        SnapshotRole::Periodic,
    ) {
        Ok(result) => Some(result.snapshot_id),
        Err(error) => {
            world
                .resource_mut::<bevy_agent_core::AgentTickFailure>()
                .set(bevy_agent_core::AgentControlError::Message(format!(
                    "periodic checkpoint failed: {error:#}"
                )));
            None
        }
    }
}
