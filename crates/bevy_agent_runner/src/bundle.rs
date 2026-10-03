//! Strict portable replay validation and transactional activation.

use super::*;

/// A portable replay artifact containing every snapshot referenced by its log.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayBundle {
    pub format_version: u32,
    pub log: ReplayLog,
    pub snapshots: Vec<Snapshot>,
}

impl ReplayBundle {
    pub const FORMAT_VERSION: u32 = 3;
}

/// Maximum number of branches accepted in a portable replay bundle.
pub const MAX_IMPORT_BRANCHES: usize = 10_000;

pub(super) fn validate_bundle_topology(bundle: &ReplayBundle) -> Result<()> {
    if bundle.log.timeline_topology.len() > MAX_IMPORT_BRANCHES {
        return Err(anyhow!(
            "replay bundle branch graph exceeds limit of {MAX_IMPORT_BRANCHES}"
        ));
    }
    bundle.log.validate_history().map_err(anyhow::Error::msg)
}

fn validate_import_temporal(bundle: &ReplayBundle) -> Result<()> {
    use std::collections::HashMap;
    let snapshots: HashMap<SnapshotId, &Snapshot> = bundle
        .snapshots
        .iter()
        .map(|snapshot| (snapshot.manifest.snapshot_id, snapshot))
        .collect();
    let check_at = |id: &SnapshotId, tick: u64, context: &str| -> Result<()> {
        let snapshot = snapshots
            .get(id)
            .ok_or_else(|| anyhow!("replay bundle {context} references missing snapshot {id:?}"))?;
        if snapshot.clock.tick != tick || snapshot.manifest.tick != tick {
            return Err(anyhow!(
                "replay bundle {context} tick mismatch at tick {tick}: snapshot {:?} has clock.tick={} manifest.tick={}",
                snapshot.manifest.snapshot_id,
                snapshot.clock.tick,
                snapshot.manifest.tick,
            ));
        }
        Ok(())
    };
    for checkpoint in &bundle.log.branch_checkpoints {
        check_at(
            &checkpoint.snapshot_id,
            checkpoint.tick,
            &format!("branch checkpoint (branch {:?})", checkpoint.branch_id),
        )?;
    }
    if let Some(initial) = bundle.log.initial_snapshot {
        check_at(
            &initial,
            bundle.log.initial_tick,
            "baseline initial snapshot",
        )?;
    }
    for branch in &bundle.log.timeline_topology {
        if let Some(fork_snapshot) = branch.fork_snapshot {
            check_at(
                &fork_snapshot,
                branch.fork_tick,
                &format!("fork snapshot (branch {:?})", branch.branch_id),
            )?;
        }
    }
    Ok(())
}

impl AgentApp {
    pub fn export_replay_bundle(&self) -> Result<ReplayBundle> {
        self.ensure_no_fault()?;
        let mut log = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.log().clone())
            .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
        // Export writes the timeline topology verbatim: branches, active
        // branch, cursor tick, and recording bounds.
        if let (Some(timeline), Some(clock)) = (
            self.app.world().get_resource::<Timeline>(),
            self.app.world().get_resource::<SimClock>(),
        ) {
            let recording = self.app.world().resource::<ReplayRecorder>().is_recording();
            let cursor = if recording {
                clock.tick
            } else {
                log.cursor_tick
            };
            log.sync_topology(timeline, cursor);
        }
        let store = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .ok_or_else(|| anyhow!("AgentSnapshotPlugin is not installed"))?;

        // Retention integrity: every snapshot referenced by the log
        // (initial, branch checkpoints, and topology fork
        // snapshots) must be present. Built from `collect_replay_references`
        // so fork snapshots are included.
        let provided = store.iter().map(|(id, _)| *id).collect::<BTreeSet<_>>();
        if let Some(missing) = log.missing_snapshot_reference(&provided) {
            return Err(anyhow!("replay references missing snapshot {missing:?}"));
        }

        let ids = collect_replay_references(&log)
            .into_iter()
            .collect::<Vec<_>>();

        let snapshots = ids
            .into_iter()
            .map(|id| {
                store
                    .get(id)
                    .cloned()
                    .ok_or_else(|| anyhow!("replay references missing snapshot {id:?}"))
            })
            .collect::<Result<Vec<_>>>()?;

        // Bundle self-containment: every referenced id must be in the payload.
        let payload = snapshots
            .iter()
            .map(|snapshot| snapshot.manifest.snapshot_id)
            .collect::<BTreeSet<_>>();
        let referenced = collect_replay_references(&log);
        if let Some(missing) = referenced.difference(&payload).next() {
            return Err(anyhow!(
                "replay bundle is missing referenced snapshot {missing:?}"
            ));
        }

        let bundle = ReplayBundle {
            format_version: ReplayBundle::FORMAT_VERSION,
            log,
            snapshots,
        };
        self.validate_replay_bundle(&bundle)?;
        Ok(bundle)
    }

    /// Validates a portable replay against the destination without changing its world.
    ///
    /// Both direct imports and remote transports use this preflight. Every
    /// snapshot is checked, including payloads not referenced by the log.
    pub fn validate_replay_bundle(&self, bundle: &ReplayBundle) -> Result<()> {
        if bundle.log.manifest.schema_version != bevy_agent_replay::REPLAY_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported replay schema {}; expected {}",
                bundle.log.manifest.schema_version,
                bevy_agent_replay::REPLAY_SCHEMA_VERSION
            ));
        }
        if bundle.format_version != ReplayBundle::FORMAT_VERSION {
            return Err(anyhow!(
                "unsupported replay bundle format {}; expected {}",
                bundle.format_version,
                ReplayBundle::FORMAT_VERSION
            ));
        }
        if !self.app.world().contains_resource::<SnapshotStore>() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }
        if !self.app.world().contains_resource::<ReplayRecorder>() {
            return Err(anyhow!("AgentReplayPlugin is not installed"));
        }
        let environment = self
            .app
            .world()
            .get_resource::<EnvironmentMetadata>()
            .cloned()
            .unwrap_or_default();
        if environment.name != "unknown-game" {
            if bundle.log.manifest.game_id != environment.name {
                return Err(anyhow!(
                    "replay game mismatch: expected {}, got {}",
                    environment.name,
                    bundle.log.manifest.game_id
                ));
            }
            if bundle.log.manifest.game_version != environment.version {
                return Err(anyhow!(
                    "replay version mismatch: expected {}, got {}",
                    environment.version,
                    bundle.log.manifest.game_version
                ));
            }
        }
        validate_bundle_topology(bundle)?;
        let referenced = collect_replay_references(&bundle.log);
        let mut provided = BTreeSet::new();
        for snapshot in &bundle.snapshots {
            if !provided.insert(snapshot.manifest.snapshot_id) {
                return Err(anyhow!(
                    "replay bundle has duplicate snapshot id {:?}",
                    snapshot.manifest.snapshot_id
                ));
            }
            validate_snapshot_full(self.app.world(), snapshot).map_err(|error| {
                anyhow!(
                    "replay bundle snapshot {:?} is invalid: {error}",
                    snapshot.manifest.snapshot_id
                )
            })?;
        }
        if let Some(missing) = referenced.difference(&provided).next() {
            return Err(anyhow!(
                "replay bundle is missing referenced snapshot {missing:?}"
            ));
        }
        if bundle.log.initial_snapshot.is_none() {
            return Err(anyhow!(
                "replay bundle requires a restorable baseline snapshot"
            ));
        }
        validate_import_temporal(bundle)?;
        for tick in [
            bundle.log.initial_tick,
            bundle.log.cursor_tick,
            bundle.log.end_tick,
        ] {
            validate_clock_tick(tick)?;
        }
        if bundle.log.initial_tick > bundle.log.cursor_tick
            || bundle.log.cursor_tick > bundle.log.end_tick
        {
            return Err(anyhow!("replay cursor must be within its recording bounds"));
        }
        let catalog = self
            .app
            .world()
            .get_resource::<AgentActionCatalog>()
            .ok_or_else(|| anyhow!("missing AgentActionCatalog"))?;
        for record in &bundle.log.records {
            validate_clock_tick(record.tick)?;
            validate_action_against_catalog(catalog, &record.action)?;
        }
        for snapshot in &bundle.snapshots {
            if referenced.contains(&snapshot.manifest.snapshot_id)
                && snapshot.manifest.episode_id != bundle.log.manifest.episode_id
            {
                return Err(anyhow!(
                    "replay bundle snapshot {:?} belongs to another episode",
                    snapshot.manifest.snapshot_id
                ));
            }
        }
        for checkpoint in &bundle.log.branch_checkpoints {
            if checkpoint.episode != bundle.log.manifest.episode_id {
                return Err(anyhow!(
                    "replay bundle checkpoint belongs to another episode"
                ));
            }
        }
        let timeline = bundle
            .log
            .validated_timeline(self.app.world().resource::<Timeline>().timeline_id())
            .map_err(anyhow::Error::msg)?;
        history::validate_history_target(
            &bundle.log,
            &timeline,
            timeline.current_branch(),
            bundle.log.cursor_tick,
        )?;
        let (checkpoint_tick, _) = bundle
            .log
            .nearest_checkpoint_for_branch(
                &timeline,
                timeline.current_branch(),
                bundle.log.cursor_tick,
            )
            .ok_or_else(|| anyhow!("replay cursor has no usable checkpoint"))?;
        if bundle.log.cursor_tick - checkpoint_tick > MAX_RECONSTRUCTION_TICKS {
            return Err(anyhow!(
                "replay cursor reconstruction exceeds budget {MAX_RECONSTRUCTION_TICKS}"
            ));
        }
        Ok(())
    }

    pub fn load_replay_bundle(&mut self, bundle: ReplayBundle) -> Result<()> {
        self.validate_replay_bundle(&bundle)?;
        self.ensure_no_fault()?;
        self.ensure_started();
        self.history_transaction(
            "replay import activation",
            transaction::TransactionKind::Replace(None),
            move |agent| {
                let cursor = bundle.log.cursor_tick;
                let episode = bundle.log.manifest.episode_id;
                let timeline = bundle
                    .log
                    .validated_timeline(agent.app.world().resource::<Timeline>().timeline_id())
                    .map_err(anyhow::Error::msg)?;
                install_snapshots(agent.app.world_mut(), bundle.snapshots)?;
                set_snapshot_episode(agent.app.world_mut(), episode);
                agent
                    .app
                    .world_mut()
                    .resource_mut::<ReplayRecorder>()
                    .replace_log(bundle.log)
                    .map_err(anyhow::Error::msg)?;
                let mut control = agent.app.world_mut().resource_mut::<AgentControlState>();
                control.timeline_id = timeline.timeline_id();
                control.branch_id = timeline.current_branch();
                agent.app.world_mut().insert_resource(timeline);
                agent
                    .app
                    .world_mut()
                    .resource_mut::<AgentActionQueue>()
                    .clear();
                agent.reset_once = true;
                agent.activate_history_tick(cursor)?;
                if agent.current_tick() != cursor {
                    return Err(anyhow!(
                        "replay import cursor mismatch (world tick {} != cursor {cursor})",
                        agent.current_tick()
                    ));
                }
                agent.prune_with_live_refs();
                Ok(())
            },
        )
    }
}
