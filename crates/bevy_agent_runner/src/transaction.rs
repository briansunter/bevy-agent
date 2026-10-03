//! Failure atomicity for navigation and history ownership changes.

use super::*;

pub(super) enum TransactionKind {
    World,
    Branch,
    Replace(Option<SnapshotId>),
}
enum Owners {
    None,
    Branch(SnapshotStore, bevy_agent_replay::BranchSavepoint, Timeline),
    Replace(SnapshotStore, Box<ReplayRecorder>, Timeline),
}

struct NavigationBackup {
    world: Snapshot,
    control: AgentControlState,
    last_response: LastStepResponse,
    tick_failure: bevy_agent_core::AgentTickFailure,
    reset_once: bool,
    owners: Owners,
}

impl NavigationBackup {
    fn capture(agent: &mut AgentApp, kind: TransactionKind) -> Result<Self> {
        // Validate the world backup before moving any history owner.
        let world = capture_snapshot(agent.app.world_mut(), None)?;
        let owners = match kind {
            TransactionKind::World => Owners::None,
            TransactionKind::Branch => Owners::Branch(
                agent.app.world().resource::<SnapshotStore>().clone(),
                agent
                    .app
                    .world()
                    .resource::<ReplayRecorder>()
                    .branch_savepoint(),
                agent.app.world().resource::<Timeline>().clone(),
            ),
            TransactionKind::Replace(baseline) => {
                let replacement = agent
                    .app
                    .world()
                    .resource::<SnapshotStore>()
                    .retained_for_replacement(baseline);
                let recorder = agent
                    .app
                    .world()
                    .resource::<ReplayRecorder>()
                    .empty_replacement();
                let old_store = std::mem::replace(
                    &mut *agent.app.world_mut().resource_mut::<SnapshotStore>(),
                    replacement,
                );
                let old_recorder = std::mem::replace(
                    &mut *agent.app.world_mut().resource_mut::<ReplayRecorder>(),
                    recorder,
                );
                let old_timeline = agent
                    .app
                    .world_mut()
                    .remove_resource::<Timeline>()
                    .expect("timeline owner");
                agent.app.world_mut().insert_resource(old_timeline.clone());
                Owners::Replace(old_store, Box::new(old_recorder), old_timeline)
            }
        };
        Ok(Self {
            world,
            control: agent.app.world().resource::<AgentControlState>().clone(),
            last_response: agent.app.world().resource::<LastStepResponse>().clone(),
            tick_failure: agent
                .app
                .world()
                .resource::<bevy_agent_core::AgentTickFailure>()
                .clone(),
            reset_once: agent.reset_once,
            owners,
        })
    }

    fn rollback(self, agent: &mut AgentApp) -> Result<()> {
        match self.owners {
            Owners::None => {}
            Owners::Branch(store, recorder, timeline) => {
                agent.app.world_mut().insert_resource(store);
                agent
                    .app
                    .world_mut()
                    .resource_mut::<ReplayRecorder>()
                    .rollback_branch(recorder);
                agent.app.world_mut().insert_resource(timeline);
            }
            Owners::Replace(store, recorder, timeline) => {
                agent.app.world_mut().insert_resource(store);
                agent.app.world_mut().insert_resource(*recorder);
                agent.app.world_mut().insert_resource(timeline);
            }
        }
        let result = restore_snapshot_backup(agent.app.world_mut(), &self.world).map(|_| ());
        // Bookkeeping must be restored even if a user-provided deserializer
        // prevents restoration of the registered simulation state.
        agent.app.world_mut().insert_resource(self.control);
        agent.app.world_mut().insert_resource(self.last_response);
        agent.app.world_mut().insert_resource(self.tick_failure);
        agent.reset_once = self.reset_once;
        result
    }
}

impl AgentApp {
    /// History-only rewinds back up registered state; operations that change
    /// history ownership also back up the store, recorder, and topology.
    pub(super) fn history_transaction<T>(
        &mut self,
        operation: &str,
        kind: TransactionKind,
        apply: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let backup = NavigationBackup::capture(self, kind)?;
        match apply(self) {
            Ok(value) => Ok(value),
            Err(error) => match backup.rollback(self) {
                Ok(()) => Err(anyhow!("{operation} failed; rolled back: {error:#}")),
                Err(rollback) => {
                    let message = format!(
                        "{operation} failed ({error:#}) and rollback failed ({rollback:#})"
                    );
                    self.app.world_mut().insert_resource(FaultState {
                        message: message.clone(),
                    });
                    Err(anyhow!("{message}; world may be faulted"))
                }
            },
        }
    }
}
