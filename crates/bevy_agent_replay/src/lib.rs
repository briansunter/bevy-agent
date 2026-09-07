//! Replay logs and timeline branches for deterministic agent-controlled games.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentAction, AgentControlState, AgentSet, BranchId, CurrentInputFrame,
    EnvironmentMetadata, SnapshotChecksum, SnapshotId, TimelineId,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Maximum ticks allowed in a single reconstruction interval (work-budget).
pub const MAX_RECONSTRUCTION_TICKS: u64 = 100_000;

/// Legacy branch id for records deserialized without a branch tag.
/// `Uuid::nil()` is used so legacy detection is stable (unlike
/// `BranchId::default()` which generates a fresh random id per call).
#[must_use]
pub fn legacy_branch_default() -> BranchId {
    BranchId(Uuid::nil())
}

/// Single legacy root id that all default/None legacy records migrate to.
#[must_use]
pub fn legacy_root_id() -> BranchId {
    BranchId(Uuid::nil())
}

#[must_use]
pub fn is_legacy_branch(branch: BranchId) -> bool {
    branch.0 == Uuid::nil()
}

/// Canonical string key for [`ReplayLog::branch_checksums`].
#[must_use]
pub fn branch_checksum_key(branch: BranchId) -> String {
    branch.0.to_string()
}

/// Maximum ancestors walked when validating timeline lineage.
///
/// Traversal helpers loop over parent links; bounded validation rejects true
/// cycles on import so a corrupt bundle can never hang lineage traversal.
pub const MAX_LINEAGE_DEPTH: usize = 1024;

/// Every snapshot referenced by a replay log.
///
/// Collects `initial_snapshot`, all legacy `checkpoints` values, all
/// `branch_checkpoints` snapshot ids, and every `timeline_topology`
/// `fork_snapshot`. Snapshot retention (`prune_checkpoints_with_refs` /
/// `delete_snapshot_checked` in `bevy_agent_snapshot`) must be called with
/// this set so coordinated deletes never drop a snapshot another subsystem
/// still needs.
#[must_use]
pub fn collect_replay_references(log: &ReplayLog) -> BTreeSet<SnapshotId> {
    let mut referenced = BTreeSet::new();
    if let Some(initial) = log.initial_snapshot {
        referenced.insert(initial);
    }
    referenced.extend(log.checkpoints.values().copied());
    referenced.extend(
        log.branch_checkpoints
            .iter()
            .map(|checkpoint| checkpoint.snapshot_id),
    );
    referenced.extend(
        log.timeline_topology
            .iter()
            .filter_map(|branch| branch.fork_snapshot),
    );
    referenced
}

/// Payload set for a portable replay bundle.
///
/// Covers the same set as [`collect_replay_references`]: `initial_snapshot`,
/// legacy `checkpoints`, branch-tagged `branch_checkpoints`, and topology
/// `fork_snapshot`s. Bundle export/import must thread this set through so
/// every referenced snapshot is present in the payload.
#[must_use]
pub fn bundle_snapshot_ids(log: &ReplayLog) -> BTreeSet<SnapshotId> {
    collect_replay_references(log)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayManifest {
    pub replay_id: Uuid,
    pub game_id: String,
    pub game_version: String,
    /// Episode id incremented on every reset. Checkpoints are tagged with the
    /// episode that created them; sync only installs matching episodes.
    #[serde(default)]
    pub episode_id: u64,
}

impl Default for ReplayManifest {
    fn default() -> Self {
        Self {
            replay_id: Uuid::new_v4(),
            game_id: "unknown-game".to_string(),
            game_version: env!("CARGO_PKG_VERSION").to_string(),
            episode_id: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ActionRecord {
    pub tick: u64,
    #[serde(default = "legacy_branch_default")]
    pub branch_id: BranchId,
    pub source: ActionSource,
    pub action: AgentAction,
}

/// Branch-tagged checkpoint entry. The legacy `ReplayLog::checkpoints`
/// (`BTreeMap<u64, SnapshotId>`) cannot hold two checkpoints at the same tick
/// on different branches; this vector preserves parent/child same-tick
/// isolation while the legacy map is kept for back-compat serialization.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BranchCheckpoint {
    pub tick: u64,
    #[serde(default = "legacy_branch_default")]
    pub branch_id: BranchId,
    pub snapshot_id: SnapshotId,
    /// Episode that created this checkpoint. Legacy entries default to 0.
    #[serde(default)]
    pub episode: u64,
}

/// Execution context for the simulation. `Reconstructing` is installed for the
/// duration of history replay (`restore_tick`/`branch` interval replay) so
/// policy systems and snapshot bookkeeping can stay quiet while recorded
/// ticks are rebuilt. See `ReplayLog::truncate_future` for the post-restore
/// stepping policy.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionContext {
    #[default]
    Live,
    Reconstructing,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReplayLog {
    pub manifest: ReplayManifest,
    pub initial_snapshot: Option<SnapshotId>,
    /// True for newly recorded live histories. Serde default `false` keeps
    /// legacy bundles (which predate this flag) classified as legacy.
    /// See [`ReplayLog::is_modern`] and [`ReplayLog::new_live`].
    #[serde(default)]
    pub modern: bool,
    /// Every executed tick per branch (including empty frames with no
    /// actions). Keyed by [`branch_checksum_key`]. Serde default empty for
    /// back-compat; legacy migration backfills from `records`.
    #[serde(default)]
    pub completed_ticks: BTreeMap<String, BTreeSet<u64>>,
    pub records: Vec<ActionRecord>,
    pub checkpoints: BTreeMap<u64, SnapshotId>,
    /// Legacy per-tick checksums (no branch tag).
    ///
    /// Kept for serde back-compat: old bundles only populate this map.
    /// New code must write via [`ReplayLog::insert_branch_checksum`] (which
    /// keeps this map consistent) and read via
    /// [`ReplayLog::expected_checksum`] (branch map only for modern
    /// histories, legacy fallback for legacy histories). On load, call
    /// [`ReplayLog::migrate_legacy_checksums`] to copy legacy entries into
    /// the root branch explicitly.
    #[serde(default, alias = "checksums")]
    pub snapshot_checksums: BTreeMap<u64, SnapshotChecksum>,
    /// Branch-aware expected checksums, keyed by `BranchId` string
    /// (`branch.0.to_string()`) -> tick -> checksum.
    ///
    /// The legacy [`ReplayLog::snapshot_checksums`] map cannot hold two
    /// checksums at the same tick on different branches; this map preserves
    /// per-branch isolation. Use [`ReplayLog::expected_checksum`] for reads
    /// (no legacy fallback when [`ReplayLog::is_modern`] is true) and
    /// [`ReplayLog::insert_branch_checksum`] for writes.
    #[serde(default)]
    pub branch_checksums: BTreeMap<String, BTreeMap<u64, SnapshotChecksum>>,
    /// Branch-tagged checkpoints. Allows the same tick to hold distinct
    /// checkpoints per branch (parent/child isolation).
    #[serde(default)]
    pub branch_checkpoints: Vec<BranchCheckpoint>,
    /// Exported timeline topology. Modern bundles populate this verbatim;
    /// legacy logs leave it empty and fall back to single-root migration.
    #[serde(default)]
    pub timeline_topology: Vec<TimelineBranch>,
    /// Active branch at export time.
    #[serde(default)]
    pub active_branch: Option<BranchId>,
    /// Cursor tick at export time.
    #[serde(default)]
    pub cursor_tick: u64,
    /// Recording bounds.
    #[serde(default)]
    pub initial_tick: u64,
    #[serde(default)]
    pub end_tick: u64,
}

impl ReplayLog {
    /// Constructor for new live histories. Sets [`ReplayLog::modern`] so
    /// live recordings never fall back to legacy global checksum visibility.
    #[must_use]
    pub fn new_live() -> Self {
        Self {
            modern: true,
            ..Default::default()
        }
    }

    /// Canonical live history marker. Sets [`ReplayLog::modern`] and, when
    /// `timeline_topology` is empty, populates it with a single `root`
    /// entry so live logs always carry topology. Called on reset
    /// ([`start_recording`]) and on first record
    /// ([`record_replay_step`], [`ReplayLog::record_tick`],
    /// [`ReplayLog::push_checkpoint_with_episode`],
    /// [`ReplayLog::insert_branch_checksum`]).
    pub fn ensure_modern(&mut self, root: BranchId) {
        self.modern = true;
        if self.timeline_topology.is_empty() {
            self.timeline_topology.push(TimelineBranch {
                branch_id: root,
                parent_branch: None,
                fork_tick: 0,
                fork_snapshot: None,
                label: Some("root".to_string()),
                actions: Vec::new(),
            });
        }
    }

    /// Record that `tick` executed on `branch`, including empty frames with
    /// no actions. Exposed for the runner so per-branch bounds
    /// ([`ReplayLog::branch_end_tick`] / [`ReplayLog::recorded_range`])
    /// advance even when a tick carries no actions/checkpoints/checksums.
    pub fn record_tick(&mut self, branch: BranchId, tick: u64) {
        self.ensure_modern(branch);
        self.completed_ticks
            .entry(branch_checksum_key(branch))
            .or_default()
            .insert(tick);
    }

    /// Backfill [`ReplayLog::completed_ticks`] from `records` for legacy
    /// logs that predate per-tick completion tracking.
    pub fn migrate_legacy_completed_ticks(&mut self) {
        for record in &self.records {
            self.completed_ticks
                .entry(branch_checksum_key(record.branch_id))
                .or_default()
                .insert(record.tick);
        }
    }

    #[must_use]
    pub fn actions_between(&self, start_exclusive: u64, end_inclusive: u64) -> Vec<ActionRecord> {
        self.records
            .iter()
            .filter(|record| record.tick > start_exclusive && record.tick <= end_inclusive)
            .cloned()
            .collect()
    }

    /// Modern logs carry `timeline_topology` (or were recorded live via
    /// [`ReplayLog::new_live`] / [`ReplayLog::ensure_modern`]); unknown
    /// branch ids are rejected. Legacy logs (empty topology, no live flag,
    /// no branch-tagged checkpoints/checksums) treat unknown ids as
    /// universally visible for back-compat, with nil (`legacy_root_id`)
    /// migrated to a single root.
    #[must_use]
    pub fn is_modern(&self) -> bool {
        self.modern
            || !self.timeline_topology.is_empty()
            || !self.branch_checkpoints.is_empty()
            || self
                .branch_checksums
                .values()
                .any(|per_branch| !per_branch.is_empty())
    }

    /// End of recorded history: max over records, checkpoints, bounds,
    /// and branch-aware checksums.
    ///
    /// Deprecated in favor of [`ReplayLog::branch_end_tick`] for per-branch
    /// bounds; this returns the global max across all branches. Kept for
    /// back-compat callers that need a single upper bound.
    #[deprecated(
        note = "Prefer `branch_end_tick` / `recorded_range` for per-branch bounds; this returns the global max across branches."
    )]
    #[must_use]
    pub fn log_end_tick(&self) -> u64 {
        let mut end = self.end_tick.max(self.cursor_tick).max(self.initial_tick);
        for record in &self.records {
            end = end.max(record.tick);
        }
        for tick in self.checkpoints.keys() {
            end = end.max(*tick);
        }
        for checkpoint in &self.branch_checkpoints {
            end = end.max(checkpoint.tick);
        }
        for tick in self.snapshot_checksums.keys() {
            end = end.max(*tick);
        }
        for per_branch in self.branch_checksums.values() {
            for tick in per_branch.keys() {
                end = end.max(*tick);
            }
        }
        for ticks in self.completed_ticks.values() {
            for tick in ticks {
                end = end.max(*tick);
            }
        }
        end
    }

    /// Per-branch end of recorded history using fork-bounded lineage intervals.
    ///
    /// Returns the max tick visible on `branch` (own records plus ancestor
    /// contributions bounded by fork ticks, visible checkpoints/checksums),
    /// floored at `initial_tick`. Use with [`ReplayLog::recorded_range`].
    #[must_use]
    pub fn branch_end_tick(&self, timeline: &Timeline, branch: BranchId) -> u64 {
        let modern = self.is_modern();
        let mut end = self.initial_tick;
        for record in &self.records {
            if branch_record_visible(timeline, record.branch_id, record.tick, branch, modern) {
                end = end.max(record.tick);
            }
        }
        for checkpoint in &self.branch_checkpoints {
            if branch_record_visible(
                timeline,
                checkpoint.branch_id,
                checkpoint.tick,
                branch,
                modern,
            ) {
                end = end.max(checkpoint.tick);
            }
        }
        // Legacy tick map carries no branch tag. For legacy logs (empty
        // topology) every entry is visible to any branch; for modern logs the
        // legacy map is only an old-reader alias and must not inflate
        // per-branch bounds, so consult it only when no branch-tagged data
        // exists.
        if self.branch_checkpoints.is_empty() {
            for tick in self.checkpoints.keys() {
                end = end.max(*tick);
            }
        }
        if modern {
            for (key, per_branch) in &self.branch_checksums {
                let checksum_branch = key.parse::<Uuid>().map(BranchId).unwrap_or(branch);
                for tick in per_branch.keys() {
                    if branch_record_visible(timeline, checksum_branch, *tick, branch, modern) {
                        end = end.max(*tick);
                    }
                }
            }
            for (key, ticks) in &self.completed_ticks {
                let tick_branch = key.parse::<Uuid>().map(BranchId).unwrap_or(branch);
                for tick in ticks {
                    if branch_record_visible(timeline, tick_branch, *tick, branch, modern) {
                        end = end.max(*tick);
                    }
                }
            }
        } else {
            for per_branch in self.branch_checksums.values() {
                for tick in per_branch.keys() {
                    end = end.max(*tick);
                }
            }
            for tick in self.snapshot_checksums.keys() {
                end = end.max(*tick);
            }
            for ticks in self.completed_ticks.values() {
                for tick in ticks {
                    end = end.max(*tick);
                }
            }
        }
        // Timeline actions mirror recorded frames; include visible ones so a
        // log with topology-only progress still reports a bound.
        if let Some(_info) = timeline.branches.get(&branch) {
            for ancestor in timeline.lineage(branch) {
                if let Some(info) = timeline.branches.get(&ancestor) {
                    for record in &info.actions {
                        if branch_record_visible(
                            timeline,
                            record.branch_id,
                            record.tick,
                            branch,
                            modern,
                        ) {
                            end = end.max(record.tick);
                        }
                    }
                }
            }
        }
        end
    }

    /// Per-branch recorded range `(initial_tick, branch_end)`.
    ///
    /// Lower bound is the global recording baseline; upper bound is
    /// [`ReplayLog::branch_end_tick`] (fork-bounded visibility).
    #[must_use]
    pub fn recorded_range(&self, timeline: &Timeline, branch: BranchId) -> (u64, u64) {
        (self.initial_tick, self.branch_end_tick(timeline, branch))
    }

    /// Branch-aware checksum lookup.
    ///
    /// Modern histories (`timeline_topology` non-empty, see
    /// [`ReplayLog::is_modern`]) return the per-branch entry only — the
    /// legacy per-tick map is an old-reader alias and must not leak across
    /// branches (same-tick parent/child isolation). Legacy histories (empty
    /// topology) fall back to the per-tick map, which predates branch tags
    /// and is visible to every branch.
    #[must_use]
    pub fn expected_checksum(&self, branch: BranchId, tick: u64) -> Option<&SnapshotChecksum> {
        if self.is_modern() {
            return self
                .branch_checksums
                .get(&branch_checksum_key(branch))
                .and_then(|per_branch| per_branch.get(&tick));
        }
        self.branch_checksums
            .get(&branch_checksum_key(branch))
            .and_then(|per_branch| per_branch.get(&tick))
            .or_else(|| self.snapshot_checksums.get(&tick))
    }

    /// Explicit legacy checksum migration run on bundle load.
    ///
    /// Copies every legacy `snapshot_checksums` entry into
    /// `branch_checksums[root_branch]` where that tick has no branch entry
    /// yet. Modern bundles already carry per-branch data and are untouched
    /// beyond filling gaps; legacy bundles gain a branch-tagged view under
    /// the single root without mutating the legacy map (old readers still
    /// see it).
    pub fn migrate_legacy_checksums(&mut self, root_branch: BranchId) {
        let key = branch_checksum_key(root_branch);
        let per_branch = self.branch_checksums.entry(key).or_default();
        for (tick, checksum) in &self.snapshot_checksums {
            per_branch.entry(*tick).or_insert_with(|| checksum.clone());
        }
        self.migrate_legacy_completed_ticks();
    }

    /// Branch-aware checksum insert. Writes the per-branch map and keeps the
    /// legacy per-tick map consistent (legacy aliases the latest write at
    /// that tick for old readers).
    pub fn insert_branch_checksum(
        &mut self,
        branch: BranchId,
        tick: u64,
        checksum: SnapshotChecksum,
    ) {
        self.ensure_modern(branch);
        self.branch_checksums
            .entry(branch_checksum_key(branch))
            .or_default()
            .insert(tick, checksum.clone());
        self.snapshot_checksums.insert(tick, checksum);
    }

    /// Populate topology fields from a live timeline (called on export).
    pub fn sync_topology(&mut self, timeline: &Timeline, cursor_tick: u64) {
        self.modern = true;
        self.timeline_topology = timeline.branches.values().cloned().collect();
        // Deterministic export order.
        self.timeline_topology
            .sort_by_key(|branch| (branch.fork_tick, branch.branch_id.0));
        self.active_branch = Some(timeline.current_branch);
        self.cursor_tick = cursor_tick;
        let mut min_tick = u64::MAX;
        let mut max_tick = 0u64;
        for record in &self.records {
            min_tick = min_tick.min(record.tick);
            max_tick = max_tick.max(record.tick);
        }
        for checkpoint in &self.branch_checkpoints {
            min_tick = min_tick.min(checkpoint.tick);
            max_tick = max_tick.max(checkpoint.tick);
        }
        for tick in self.checkpoints.keys() {
            min_tick = min_tick.min(*tick);
            max_tick = max_tick.max(*tick);
        }
        for ticks in self.completed_ticks.values() {
            for tick in ticks {
                min_tick = min_tick.min(*tick);
                max_tick = max_tick.max(*tick);
            }
        }
        if min_tick == u64::MAX {
            min_tick = cursor_tick;
        }
        self.initial_tick = min_tick.min(cursor_tick);
        self.end_tick = max_tick.max(cursor_tick);
    }

    /// Actions visible on `branch` using fork-bounded intervals: for lineage
    /// root->...->target, each ancestor contributes
    /// `(own_fork, child_fork]` where `child_fork` is the fork tick of the
    /// child towards the target, and the target contributes all its own
    /// records. Parent-future records beyond the fork are excluded.
    #[must_use]
    pub fn actions_for_branch(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        start_exclusive: u64,
        end_inclusive: u64,
    ) -> Vec<ActionRecord> {
        let modern = self.is_modern();
        self.records
            .iter()
            .filter(|record| {
                record.tick > start_exclusive
                    && record.tick <= end_inclusive
                    && branch_record_visible(
                        timeline,
                        record.branch_id,
                        record.tick,
                        branch,
                        modern,
                    )
            })
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn nearest_checkpoint_at_or_before(&self, tick: u64) -> Option<(u64, SnapshotId)> {
        self.checkpoints
            .range(..=tick)
            .next_back()
            .map(|(tick, snapshot_id)| (*tick, *snapshot_id))
    }

    /// Branch-aware checkpoint selection using the SAME fork-bounded
    /// visibility helper as [`ReplayLog::actions_for_branch`]: the nearest
    /// checkpoint at-or-before `tick` visible in the branch's intervals.
    /// Falls back to the legacy tick map for logs without branch-tagged data.
    #[must_use]
    pub fn nearest_checkpoint_for_branch(
        &self,
        timeline: &Timeline,
        branch: BranchId,
        tick: u64,
    ) -> Option<(u64, SnapshotId)> {
        let modern = self.is_modern();
        let mut best: Option<(u64, BranchId, SnapshotId)> = None;
        let mut consider = |candidate_tick: u64, candidate_branch: BranchId, id: SnapshotId| {
            if candidate_tick > tick {
                return;
            }
            if !branch_record_visible(timeline, candidate_branch, candidate_tick, branch, modern) {
                return;
            }
            // Nearest tick wins; ties prefer the deepest branch (self over
            // ancestor) so same-tick parent/child checkpoints stay isolated.
            let replace = match best {
                None => true,
                Some((best_tick, best_branch, _)) => {
                    candidate_tick > best_tick
                        || (candidate_tick == best_tick
                            && best_branch != candidate_branch
                            && lineage_contains(timeline, best_branch, candidate_branch))
                }
            };
            if replace {
                best = Some((candidate_tick, candidate_branch, id));
            }
        };
        for checkpoint in &self.branch_checkpoints {
            consider(
                checkpoint.tick,
                checkpoint.branch_id,
                checkpoint.snapshot_id,
            );
        }
        // Legacy map entries carry no branch tag. Only consult them when no
        // branch-tagged data exists (old logs); treat them as visible to any
        // branch at-or-before the tick to preserve back-compat behavior.
        if self.branch_checkpoints.is_empty() {
            for (candidate_tick, id) in &self.checkpoints {
                if *candidate_tick <= tick {
                    let replace = match best {
                        None => true,
                        Some((best_tick, _, _)) => *candidate_tick > best_tick,
                    };
                    if replace {
                        best = Some((*candidate_tick, branch, *id));
                    }
                }
            }
        }
        // Initial snapshot covers tick 0 when nothing else matches.
        if best.is_none()
            && let Some(initial) = self.initial_snapshot
        {
            best = Some((0, branch, initial));
        }
        best.map(|(tick, _, id)| (tick, id))
    }

    /// Post-restore stepping policy: diverging from a restored tick truncates
    /// the recorded future on the same branch. Call this before appending new
    /// steps after `restore_tick` so stale future records can never be
    /// replayed or confused with the diverged history. Records on other
    /// branches are preserved.
    ///
    /// Prunes records, branch-tagged checkpoints, this branch's
    /// `branch_checksums` entries beyond `tick` (dropping the branch key when
    /// empty), and legacy `checkpoints` / `snapshot_checksums` entries beyond
    /// `tick` that have no remaining branch checkpoint at that tick (so a
    /// same-tick checkpoint on another branch keeps its alias). Cursor and
    /// `end_tick` bounds are clamped down to `tick` when they point beyond
    /// the new end.
    pub fn truncate_future(&mut self, branch: BranchId, tick: u64) {
        self.records
            .retain(|record| !(record.branch_id == branch && record.tick > tick));
        self.branch_checkpoints
            .retain(|checkpoint| !(checkpoint.branch_id == branch && checkpoint.tick > tick));
        // Branch-aware checksums: drop this branch's future.
        let key = branch_checksum_key(branch);
        if let Some(per_branch) = self.branch_checksums.get_mut(&key) {
            per_branch.retain(|checkpoint_tick, _| *checkpoint_tick <= tick);
        }
        if self
            .branch_checksums
            .get(&key)
            .is_some_and(|per_branch| per_branch.is_empty())
        {
            self.branch_checksums.remove(&key);
        }
        // Completed ticks: drop this branch's future (including empty
        // frames); drop the branch key when empty.
        if let Some(ticks) = self.completed_ticks.get_mut(&key) {
            ticks.retain(|completed_tick| *completed_tick <= tick);
        }
        if self
            .completed_ticks
            .get(&key)
            .is_some_and(|ticks| ticks.is_empty())
        {
            self.completed_ticks.remove(&key);
        }
        // Legacy maps: always drop entries beyond `tick` that have no
        // remaining branch checkpoint at that tick (same-tick isolation
        // across branches). No empty-vector special case: legacy-only logs
        // (no branch checkpoints) truncate unconditionally, matching the
        // rule because no checkpoint remains at any future tick.
        self.checkpoints.retain(|checkpoint_tick, _| {
            if *checkpoint_tick <= tick {
                return true;
            }
            self.branch_checkpoints
                .iter()
                .any(|checkpoint| checkpoint.tick == *checkpoint_tick)
        });
        self.snapshot_checksums.retain(|checksum_tick, _| {
            if *checksum_tick <= tick {
                return true;
            }
            self.branch_checkpoints
                .iter()
                .any(|checkpoint| checkpoint.tick == *checksum_tick)
        });
        if self.cursor_tick > tick {
            self.cursor_tick = tick;
        }
        if self.end_tick > tick {
            self.end_tick = tick;
        }
    }

    /// Records a checkpoint on a branch, keeping both the legacy tick map and
    /// the branch-tagged vector consistent.
    pub fn push_checkpoint(&mut self, branch: BranchId, tick: u64, snapshot_id: SnapshotId) {
        let episode = self.manifest.episode_id;
        self.push_checkpoint_with_episode(branch, tick, snapshot_id, episode);
    }

    /// Episode-tagged checkpoint insert. Only checkpoints matching the current
    /// episode are synced; `reset` bumps `manifest.episode_id` so stale
    /// cross-episode checkpoints never contaminate a fresh log.
    pub fn push_checkpoint_with_episode(
        &mut self,
        branch: BranchId,
        tick: u64,
        snapshot_id: SnapshotId,
        episode: u64,
    ) {
        self.ensure_modern(branch);
        self.checkpoints.insert(tick, snapshot_id);
        if let Some(existing) = self
            .branch_checkpoints
            .iter_mut()
            .find(|checkpoint| checkpoint.tick == tick && checkpoint.branch_id == branch)
        {
            existing.snapshot_id = snapshot_id;
            existing.episode = episode;
        } else {
            self.branch_checkpoints.push(BranchCheckpoint {
                tick,
                branch_id: branch,
                snapshot_id,
                episode,
            });
        }
    }

    /// Validates that every snapshot referenced by the log is present in
    /// `provided`. Used by bundle export/import to guarantee retention
    /// integrity after `prune_checkpoints` runs in the snapshot crate.
    /// Covers [`bundle_snapshot_ids`] / [`collect_replay_references`]:
    /// initial, legacy + branch-tagged checkpoints, and topology
    /// `fork_snapshot`s.
    #[must_use]
    pub fn missing_snapshot_reference(
        &self,
        provided: &std::collections::BTreeSet<SnapshotId>,
    ) -> Option<SnapshotId> {
        let referenced = bundle_snapshot_ids(self);
        referenced.difference(provided).next().copied()
    }
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRecorder {
    pub recording: bool,
    pub log: ReplayLog,
}

impl Default for ReplayRecorder {
    fn default() -> Self {
        Self {
            recording: true,
            log: ReplayLog::new_live(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineBranch {
    pub branch_id: BranchId,
    pub parent_branch: Option<BranchId>,
    pub fork_tick: u64,
    pub fork_snapshot: Option<SnapshotId>,
    pub label: Option<String>,
    pub actions: Vec<ActionRecord>,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize)]
pub struct Timeline {
    pub timeline_id: TimelineId,
    pub current_branch: BranchId,
    pub branches: HashMap<BranchId, TimelineBranch>,
}

impl Default for Timeline {
    fn default() -> Self {
        let timeline_id = TimelineId::new();
        let branch_id = BranchId::new();
        let mut branches = HashMap::new();
        branches.insert(
            branch_id,
            TimelineBranch {
                branch_id,
                parent_branch: None,
                fork_tick: 0,
                fork_snapshot: None,
                label: Some("root".to_string()),
                actions: Vec::new(),
            },
        );
        Self {
            timeline_id,
            current_branch: branch_id,
            branches,
        }
    }
}

impl Timeline {
    pub fn create_branch(
        &mut self,
        fork_tick: u64,
        fork_snapshot: Option<SnapshotId>,
        label: Option<String>,
    ) -> BranchId {
        let parent = self.current_branch;
        let branch_id = BranchId::new();
        self.branches.insert(
            branch_id,
            TimelineBranch {
                branch_id,
                parent_branch: Some(parent),
                fork_tick,
                fork_snapshot,
                label,
                actions: Vec::new(),
            },
        );
        self.current_branch = branch_id;
        branch_id
    }

    /// Ancestry chain from `branch` up to the root (branch first, root last).
    #[must_use]
    pub fn lineage(&self, branch: BranchId) -> Vec<BranchId> {
        let mut chain = Vec::new();
        let mut current = Some(branch);
        while let Some(id) = current {
            chain.push(id);
            current = self
                .branches
                .get(&id)
                .and_then(|branch| branch.parent_branch);
        }
        chain
    }

    /// Bounded ancestry chain from `branch` up to the root.
    ///
    /// Walks at most [`MAX_LINEAGE_DEPTH`] (1024) parent links and rejects
    /// cycles (repeated ids) and over-deep chains with an `Err`, so corrupt
    /// topologies can never hang traversal. Returns the chain branch-first,
    /// root-last on success.
    pub fn ancestors_bounded(&self, branch: BranchId) -> Result<Vec<BranchId>, String> {
        ancestors_bounded(self, branch)
    }

    /// Truncates future actions on `branch` beyond `tick` (post-restore
    /// diverge policy, timeline side; see `ReplayLog::truncate_future`).
    pub fn truncate_future(&mut self, branch: BranchId, tick: u64) {
        if let Some(branch_state) = self.branches.get_mut(&branch) {
            branch_state.actions.retain(|record| record.tick <= tick);
        }
    }
}

/// Bounded ancestry chain from `branch` up to the root (branch first,
/// root last).
///
/// Walks at most [`MAX_LINEAGE_DEPTH`] (1024) parent links; returns `Err` on
/// a cycle (repeated id, including self-parents) or when the chain exceeds
/// the depth bound (corrupt topology). Unknown ids terminate the walk with
/// the chain collected so far.
pub fn ancestors_bounded(timeline: &Timeline, branch: BranchId) -> Result<Vec<BranchId>, String> {
    use std::collections::HashSet;
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current = Some(branch);
    for _ in 0..MAX_LINEAGE_DEPTH {
        let Some(id) = current else {
            return Ok(chain);
        };
        if !visited.insert(id) {
            return Err(format!(
                "timeline lineage for {branch:?} is cyclic at {id:?}"
            ));
        }
        chain.push(id);
        match timeline.branches.get(&id) {
            None => return Ok(chain),
            Some(info) => current = info.parent_branch,
        }
    }
    Err(format!(
        "timeline lineage for {branch:?} exceeds maximum depth {MAX_LINEAGE_DEPTH}"
    ))
}

/// Returns true when `ancestor` equals `descendant` or appears in the
/// descendant's parent chain.
#[must_use]
pub fn lineage_contains(timeline: &Timeline, ancestor: BranchId, descendant: BranchId) -> bool {
    let mut current = Some(descendant);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = timeline
            .branches
            .get(&id)
            .and_then(|branch| branch.parent_branch);
    }
    false
}

/// Fork tick at which the path from `descendant` leaves `ancestor`
/// (the `fork_tick` of the child directly under `ancestor`).
/// Returns `None` when both are equal or `ancestor` is not an ancestor.
#[must_use]
pub fn branch_fork_from_ancestor(
    timeline: &Timeline,
    ancestor: BranchId,
    descendant: BranchId,
) -> Option<u64> {
    if ancestor == descendant {
        return None;
    }
    let mut current = descendant;
    loop {
        let branch = timeline.branches.get(&current)?;
        match branch.parent_branch {
            Some(parent) if parent == ancestor => return Some(branch.fork_tick),
            Some(parent) => current = parent,
            None => return None,
        }
    }
}

/// Fork-bounded visibility shared by [`ReplayLog::actions_for_branch`] and
/// [`ReplayLog::nearest_checkpoint_for_branch`].
///
/// For lineage root->...->target, each ancestor `A` contributes
/// `(own_fork, child_fork]` where `own_fork` is `A`'s own fork tick and
/// `child_fork` is the fork tick of the child towards `target`; the target
/// itself contributes all its own records (including its fork-tick
/// checkpoint for same-tick isolation). Unknown branch ids are rejected in
/// modern bundles (`modern == true`, i.e. `timeline_topology` present) and
/// treated as universally visible only for legacy logs; nil ids migrate to
/// the single legacy root.
#[must_use]
pub fn branch_record_visible(
    timeline: &Timeline,
    record_branch: BranchId,
    record_tick: u64,
    target: BranchId,
    modern: bool,
) -> bool {
    if record_branch == target {
        return true;
    }
    if lineage_contains(timeline, record_branch, target) {
        let own_fork = timeline
            .branches
            .get(&record_branch)
            .map(|branch| branch.fork_tick)
            .unwrap_or(0);
        if let Some(child_fork) = branch_fork_from_ancestor(timeline, record_branch, target) {
            return record_tick > own_fork && record_tick <= child_fork;
        }
        return false;
    }
    // Not an ancestor: sibling/subtree records are invisible.
    if timeline.branches.contains_key(&record_branch) {
        return false;
    }
    // Unknown branch id.
    if modern {
        return false;
    }
    // Legacy: nil (missing tag) migrates to single root -> visible; other
    // unknown ids preserved as universal for back-compat.
    let _ = is_legacy_branch(record_branch);
    true
}

pub struct AgentReplayPlugin;

impl Plugin for AgentReplayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReplayRecorder>()
            .init_resource::<Timeline>()
            .init_resource::<ExecutionContext>()
            .add_systems(
                bevy_agent_core::AgentTick,
                record_replay_step.in_set(AgentSet::ReplayRecord),
            );
    }
}

pub fn record_replay_step(
    input: Res<CurrentInputFrame>,
    mut recorder: ResMut<ReplayRecorder>,
    mut timeline: ResMut<Timeline>,
    control: Res<AgentControlState>,
    context: Res<ExecutionContext>,
) {
    if !recorder.recording {
        return;
    }
    // No recording append while reconstructing history.
    if *context == ExecutionContext::Reconstructing {
        return;
    }

    let branch_id = if timeline.branches.contains_key(&control.branch_id) {
        control.branch_id
    } else {
        timeline.current_branch
    };
    let mut records = Vec::new();
    for (index, action) in input.actions.iter().cloned().enumerate() {
        records.push(ActionRecord {
            tick: input.tick,
            branch_id,
            source: input
                .sources
                .get(index)
                .cloned()
                .unwrap_or(ActionSource::Agent),
            action,
        });
    }

    recorder.log.ensure_modern(branch_id);
    recorder.log.record_tick(branch_id, input.tick);
    recorder.log.records.extend(records.iter().cloned());
    if let Some(branch) = timeline.branches.get_mut(&branch_id) {
        branch.actions.extend(records);
    }
}

pub fn start_recording(world: &mut World, initial_snapshot: Option<SnapshotId>) {
    let metadata = world
        .get_resource::<EnvironmentMetadata>()
        .cloned()
        .unwrap_or_default();
    // Reset the timeline to a fresh root so a new recording never inherits
    // stale branches; callers that need a baseline snapshot pass it in (the
    // runner captures the current tick when `None` is given and snapshot
    // support is installed).
    let (timeline_id, branch_id) = {
        let timeline = world.get_resource_mut::<Timeline>();
        if let Some(mut timeline) = timeline {
            *timeline = Timeline::default();
            (timeline.timeline_id, timeline.current_branch)
        } else {
            let timeline = Timeline::default();
            let ids = (timeline.timeline_id, timeline.current_branch);
            world.insert_resource(timeline);
            ids
        }
    };
    if let Some(mut control) = world.get_resource_mut::<AgentControlState>() {
        control.timeline_id = timeline_id;
        control.branch_id = branch_id;
    }
    if world.get_resource::<ReplayRecorder>().is_none() {
        world.insert_resource(ReplayRecorder::default());
    }
    let mut recorder = world.resource_mut::<ReplayRecorder>();
    recorder.recording = true;
    recorder.log = ReplayLog::new_live();
    recorder.log.ensure_modern(branch_id);
    recorder.log.manifest.game_id = metadata.name;
    recorder.log.manifest.game_version = metadata.version;
    recorder.log.initial_snapshot = initial_snapshot;
}

pub fn stop_recording(world: &mut World) -> ReplayLog {
    let mut recorder = world.resource_mut::<ReplayRecorder>();
    recorder.recording = false;
    recorder.log.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tick: u64, action: AgentAction) -> ActionRecord {
        ActionRecord {
            tick,
            branch_id: BranchId::default(),
            source: ActionSource::Agent,
            action,
        }
    }

    fn record_on(branch_id: BranchId, tick: u64, action: AgentAction) -> ActionRecord {
        ActionRecord {
            tick,
            branch_id,
            source: ActionSource::Agent,
            action,
        }
    }

    #[test]
    fn replay_log_filters_actions_between_ticks() {
        let log = ReplayLog {
            records: vec![
                record(1, AgentAction::Noop),
                record(2, AgentAction::Jump),
                record(3, AgentAction::Interact),
            ],
            ..Default::default()
        };

        let actions = log.actions_between(1, 3);

        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].tick, 2);
        assert_eq!(actions[1].tick, 3);
    }

    #[test]
    fn replay_log_finds_nearest_checkpoint_at_or_before_tick() {
        let first = SnapshotId::new();
        let second = SnapshotId::new();
        let log = ReplayLog {
            checkpoints: [(10, first), (20, second)].into_iter().collect(),
            ..Default::default()
        };

        assert_eq!(log.nearest_checkpoint_at_or_before(9), None);
        assert_eq!(log.nearest_checkpoint_at_or_before(10), Some((10, first)));
        assert_eq!(log.nearest_checkpoint_at_or_before(25), Some((20, second)));
    }

    #[test]
    fn timeline_create_branch_tracks_parent_and_switches_current_branch() {
        let mut timeline = Timeline::default();
        let parent = timeline.current_branch;
        let snapshot = SnapshotId::new();

        let child = timeline.create_branch(42, Some(snapshot), Some("try-alt".to_string()));

        assert_ne!(child, parent);
        assert_eq!(timeline.current_branch, child);
        let branch = timeline.branches.get(&child).unwrap();
        assert_eq!(branch.parent_branch, Some(parent));
        assert_eq!(branch.fork_tick, 42);
        assert_eq!(branch.fork_snapshot, Some(snapshot));
        assert_eq!(branch.label.as_deref(), Some("try-alt"));
    }

    #[test]
    fn start_and_stop_recording_resets_and_returns_log() {
        let mut world = World::new();
        world.insert_resource(ReplayRecorder::default());
        let snapshot = SnapshotId::new();

        start_recording(&mut world, Some(snapshot));
        {
            let mut recorder = world.resource_mut::<ReplayRecorder>();
            recorder.log.records.push(record(1, AgentAction::Jump));
        }
        let log = stop_recording(&mut world);

        assert!(!world.resource::<ReplayRecorder>().recording);
        assert_eq!(log.initial_snapshot, Some(snapshot));
        assert_eq!(log.records.len(), 1);
    }

    #[test]
    fn actions_for_branch_includes_ancestors_but_not_siblings() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);
        // Sibling forked off root while keeping `child` addressable.
        timeline.current_branch = root;
        let sibling = timeline.create_branch(5, None, None);
        timeline.current_branch = child;

        let log = ReplayLog {
            records: vec![
                record_on(root, 3, AgentAction::Noop),
                record_on(child, 7, AgentAction::Jump),
                record_on(sibling, 7, AgentAction::Interact),
            ],
            ..Default::default()
        };

        let child_actions = log.actions_for_branch(&timeline, child, 0, 10);
        assert_eq!(child_actions.len(), 2);
        assert!(child_actions.iter().any(|r| r.tick == 3));
        assert!(child_actions.iter().any(|r| r.action == AgentAction::Jump));

        let sibling_actions = log.actions_for_branch(&timeline, sibling, 0, 10);
        assert_eq!(sibling_actions.len(), 2);
        assert!(
            sibling_actions
                .iter()
                .any(|r| r.action == AgentAction::Interact)
        );
    }

    #[test]
    fn checkpoint_selection_excludes_parent_future_beyond_fork() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);

        let parent_early = SnapshotId::new();
        let parent_late = SnapshotId::new();
        let mut log = ReplayLog::default();
        log.push_checkpoint(root, 3, parent_early);
        log.push_checkpoint(root, 8, parent_late);

        // Child restoring at tick 6 must see the tick-3 ancestor checkpoint,
        // not the tick-8 parent-future checkpoint recorded after the fork.
        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, child, 6),
            Some((3, parent_early))
        );
        // Root itself still sees its own tick-8 checkpoint.
        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, root, 9),
            Some((8, parent_late))
        );
    }

    #[test]
    fn same_tick_checkpoints_are_isolated_per_branch() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);

        let parent_snap = SnapshotId::new();
        let child_snap = SnapshotId::new();
        let mut log = ReplayLog::default();
        log.push_checkpoint(root, 5, parent_snap);
        log.push_checkpoint(child, 5, child_snap);

        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, child, 5),
            Some((5, child_snap))
        );
        assert_eq!(
            log.nearest_checkpoint_for_branch(&timeline, root, 5),
            Some((5, parent_snap))
        );
    }

    #[test]
    fn parent_future_excluded_via_fork_bounded_intervals() {
        // Parent tick2 MoveRight must not leak into a child forked at tick1.
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(1, None, None);
        let move_right = AgentAction::Move { x: 1.0, y: 0.0 };
        let log = ReplayLog {
            records: vec![
                record_on(root, 1, AgentAction::Noop),
                record_on(root, 2, move_right.clone()),
                record_on(child, 2, AgentAction::Noop),
            ],
            ..Default::default()
        };
        let child_actions = log.actions_for_branch(&timeline, child, 0, 2);
        // Shared tick1 + child-only tick2; parent tick2 excluded.
        assert_eq!(child_actions.len(), 2);
        assert!(child_actions.iter().any(|r| r.tick == 1));
        let tick2: Vec<_> = child_actions.iter().filter(|r| r.tick == 2).collect();
        assert_eq!(tick2.len(), 1);
        assert_eq!(tick2[0].action, AgentAction::Noop);
        assert_eq!(tick2[0].branch_id, child);
    }

    #[test]
    fn modern_unknown_branch_ids_are_rejected() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(1, None, None);
        let unknown = BranchId::new();
        let mut log = ReplayLog {
            records: vec![
                record_on(root, 1, AgentAction::Noop),
                record_on(unknown, 2, AgentAction::Jump),
            ],
            ..Default::default()
        };
        // Legacy (empty topology): unknown treated universal for back-compat.
        assert_eq!(log.actions_for_branch(&timeline, child, 0, 2).len(), 2);
        // Modern (topology present): unknown rejected.
        log.sync_topology(&timeline, 2);
        assert_eq!(log.actions_for_branch(&timeline, child, 0, 2).len(), 1);
    }

    #[test]
    fn truncate_future_diverges_same_branch_only() {
        let mut timeline = Timeline::default();
        let root = timeline.current_branch;
        let child = timeline.create_branch(5, None, None);

        let mut log = ReplayLog {
            records: vec![
                record_on(root, 6, AgentAction::Noop),
                record_on(root, 9, AgentAction::Jump),
                record_on(child, 9, AgentAction::Interact),
            ],
            ..Default::default()
        };
        let doomed = SnapshotId::new();
        let kept = SnapshotId::new();
        log.push_checkpoint(root, 9, doomed);
        log.push_checkpoint(child, 9, kept);

        log.truncate_future(root, 6);
        timeline.truncate_future(root, 6);

        assert!(
            log.records
                .iter()
                .all(|r| r.branch_id != root || r.tick <= 6)
        );
        assert!(log.records.iter().any(|r| r.branch_id == child));
        assert!(
            log.branch_checkpoints
                .iter()
                .all(|c| c.branch_id != root || c.tick <= 6)
        );
        assert!(log.branch_checkpoints.iter().any(|c| c.snapshot_id == kept));
        assert!(
            !log.branch_checkpoints
                .iter()
                .any(|c| c.snapshot_id == doomed)
        );
    }
}
