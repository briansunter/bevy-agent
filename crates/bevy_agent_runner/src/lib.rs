//! Runner API for manually stepping Bevy apps through agent simulation ticks.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use bevy::app::{PluginGroup, PluginGroupBuilder};
use bevy::prelude::*;
use bevy_agent_core::{
    ActionSource, AgentAction, AgentActionCatalog, AgentActionQueue, AgentControlPlugin,
    AgentControlState, AgentReset, AgentTick, BranchId, ControlMode, CurrentInputFrame,
    DeterministicRng, EnvironmentMetadata, EpisodeState, ExecutionContext as CoreExecutionContext,
    LastStepResponse, Observation, ObservationConfig, ObservationMode, PendingInputPolicy,
    ResetResponse, RewardState, SimClock, SnapshotChecksum, SnapshotId, StepManyResponse,
    StepResponse, apply_pending_policy, collect_observation, run_agent_tick,
    validate_action_against_catalog,
};
use bevy_agent_core::{AgentPostTick, AgentPreTick};
use bevy_agent_replay::{
    ActionRecord, AgentReplayPlugin, ExecutionContext as ReplayExecutionContext,
    MAX_RECONSTRUCTION_TICKS, ReplayLog, ReplayRecorder, Timeline, TimelineBranch,
    branch_fork_from_ancestor, collect_replay_references, is_legacy_branch, legacy_root_id,
    lineage_contains, start_recording,
};
use bevy_agent_snapshot::{
    AgentSnapshotPlugin, FaultState, Snapshot, SnapshotCreateResult, SnapshotPolicy, SnapshotRole,
    SnapshotStore, capture_snapshot, checksum_snapshot, create_snapshot_with_role,
    delete_snapshot_checked, enforce_retention, pin_snapshot, prune_checkpoints_with_refs,
    restore_snapshot, restore_snapshot_value, unpin_snapshot,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct AgentControlPlugins {
    control: AgentControlPlugin,
    snapshots: bool,
    replay: bool,
    snapshot_policy: Option<SnapshotPolicy>,
}

impl AgentControlPlugins {
    #[must_use]
    pub fn deterministic() -> Self {
        Self {
            control: AgentControlPlugin::deterministic(),
            snapshots: true,
            replay: true,
            snapshot_policy: None,
        }
    }

    #[must_use]
    pub fn visual_debug() -> Self {
        Self {
            control: AgentControlPlugin::visual_debug(),
            snapshots: true,
            replay: true,
            snapshot_policy: None,
        }
    }

    #[must_use]
    pub fn remote() -> Self {
        Self {
            control: AgentControlPlugin::remote(),
            snapshots: true,
            replay: true,
            snapshot_policy: None,
        }
    }

    #[must_use]
    pub fn without_snapshots(mut self) -> Self {
        self.snapshots = false;
        self
    }

    #[must_use]
    pub fn without_replay(mut self) -> Self {
        self.replay = false;
        self
    }

    #[must_use]
    pub fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = Some(policy);
        self
    }
}

impl Default for AgentControlPlugins {
    fn default() -> Self {
        Self::deterministic()
    }
}

impl PluginGroup for AgentControlPlugins {
    fn build(self) -> PluginGroupBuilder {
        let mut builder = PluginGroupBuilder::start::<Self>().add(self.control);
        if self.snapshots {
            builder = builder.add(AgentSnapshotPlugin);
            if let Some(policy) = self.snapshot_policy {
                builder = builder.add(AgentSnapshotPolicyPlugin(policy));
            }
        }
        if self.replay {
            builder = builder.add(AgentReplayPlugin);
        }
        builder
    }
}

struct AgentSnapshotPolicyPlugin(SnapshotPolicy);

impl Plugin for AgentSnapshotPolicyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.0.clone());
    }
}

/// Maximum ancestors walked when validating timeline lineage.
///
/// Traversal helpers in the replay crate loop over parent links; replay
/// bundle validation rejects true cycles on import, but the runner defends
/// anyway so a corrupt in-memory topology can never hang history navigation.
pub const MAX_LINEAGE_DEPTH: usize = 1024;

/// Collects every snapshot id referenced by a replay log (initial, legacy
/// tick map, and branch-tagged checkpoints). Retention enforcement threads
/// this set through pruning so coordinated deletes never drop a snapshot
/// another subsystem still needs. Never prune a creation path with an empty
/// set; always derive live refs from the current log.
#[must_use]
pub fn live_snapshot_refs(log: &ReplayLog) -> BTreeSet<SnapshotId> {
    collect_replay_references(log)
}

/// Branch-aware expected checksum: per-branch map only, no cross-branch
/// fallback. Legacy tick-map entries are visible through
/// `log.expected_checksum` only when they predate branch tags (back-compat);
/// a checksum recorded on another branch must never satisfy this branch.
#[must_use]
pub fn expected_checksum_for_tick(
    log: &ReplayLog,
    branch: BranchId,
    tick: u64,
) -> Option<SnapshotChecksum> {
    log.expected_checksum(branch, tick).cloned()
}

/// Computes a snapshot checksum that excludes the pending action queue.
///
/// Pending futures beyond a restore target must NOT affect tick-target state
/// verification: the queue at verify time (cleared + current frame + maybe
/// re-applied futures) differs from the queue at record time. Both the stored
/// expectation and the replay-time actual are therefore computed with the
/// queue stripped (the `action_queue` field and the serialized
/// `AgentActionQueue` resource entry), so verification compares pure
/// simulation state.
pub fn queue_excluded_checksum(snapshot: &Snapshot) -> Result<SnapshotChecksum> {
    let mut stripped = snapshot.clone();
    stripped.action_queue.clear();
    stripped
        .resources
        .retain(|resource| !resource.type_name.contains("AgentActionQueue"));
    checksum_snapshot(&stripped)
}

/// Captures the current world and checksums it excluding the queue (see
/// [`queue_excluded_checksum`]).
fn capture_queue_excluded_checksum(world: &mut World) -> Result<SnapshotChecksum> {
    let snapshot = capture_snapshot(world, None)?;
    queue_excluded_checksum(&snapshot)
}

/// Stored expectation for a checkpoint snapshot, computed queue-excluded so
/// later replay verification (also queue-excluded) compares like with like.
#[allow(dead_code)]
fn stored_queue_excluded_checksum(
    store: &SnapshotStore,
    id: SnapshotId,
    fallback: SnapshotChecksum,
) -> SnapshotChecksum {
    store
        .snapshots
        .get(&id)
        .and_then(|snapshot| queue_excluded_checksum(snapshot).ok())
        .unwrap_or(fallback)
}

/// Rejects cyclic parent links in an imported timeline topology.
///
/// Walks each branch's parent chain with a hard bound
/// ([`MAX_LINEAGE_DEPTH`]): self-parents, cycles, and over-deep chains are
/// rejected before the topology is installed. Complements the replay crate's
/// own validation; the runner defends anyway so a corrupt bundle can never
/// hang lineage traversal.
fn validate_import_topology_acyclic(topology: &[TimelineBranch]) -> Result<()> {
    use std::collections::HashMap;
    let parents: HashMap<BranchId, Option<BranchId>> = topology
        .iter()
        .map(|branch| (branch.branch_id, branch.parent_branch))
        .collect();
    for branch in topology {
        let mut visited = HashSet::new();
        let mut current = branch.parent_branch;
        for _ in 0..MAX_LINEAGE_DEPTH {
            let Some(id) = current else {
                break;
            };
            if id == branch.branch_id {
                return Err(anyhow!(
                    "replay bundle branch {:?} has a cyclic parent link",
                    branch.branch_id
                ));
            }
            if !visited.insert(id) {
                return Err(anyhow!(
                    "replay bundle topology contains a parent cycle at {id:?}"
                ));
            }
            match parents.get(&id) {
                None => break,
                Some(parent) => current = *parent,
            }
        }
        if current.is_some() {
            return Err(anyhow!(
                "replay bundle branch {:?} parent chain exceeds maximum depth {MAX_LINEAGE_DEPTH}",
                branch.branch_id
            ));
        }
    }
    Ok(())
}

/// Temporal validation for an imported replay bundle (checked BEFORE install).
///
/// For every branch-tagged and legacy checkpoint the referenced snapshot must
/// exist and satisfy `snapshot.clock.tick == checkpoint.tick` and
/// `snapshot.manifest.tick == checkpoint.tick`. The baseline initial snapshot
/// tick must equal `log.initial_tick`, and every topology `fork_snapshot`
/// tick must equal its branch `fork_tick`. Any mismatch rejects the import
/// before world state is touched.
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
    for (tick, id) in &bundle.log.checkpoints {
        check_at(id, *tick, "legacy checkpoint")?;
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

/// Resolves the fork parent for `branch(from_tick)` to the ancestor owning
/// the `from_tick` interval.
///
/// Walks the lineage from the current branch (deepest first) and returns the
/// deepest ancestor whose fork interval contains `from_tick`: the target
/// itself when `from_tick >= own_fork`, otherwise an ancestor with
/// `from_tick >= own_fork && from_tick <= child_fork` (inclusive bounds so
/// the tick-0 baseline resolves to the root). Rejects when no ancestor
/// contains the tick so the import validator invariant (child fork >= parent
/// fork) always holds for the new branch.
fn resolve_fork_parent_branch(
    timeline: &Timeline,
    current: BranchId,
    from_tick: u64,
) -> Result<BranchId> {
    let chain = timeline.lineage(current);
    if !chain.contains(&current) && !timeline.branches.contains_key(&current) {
        return Err(anyhow!(
            "branch target {from_tick} references unknown branch {current:?}"
        ));
    }
    // Deepest first: `lineage` returns branch-first, root-last.
    for candidate in &chain {
        let Some(info) = timeline.branches.get(candidate) else {
            continue;
        };
        if *candidate == current {
            if from_tick >= info.fork_tick {
                return Ok(*candidate);
            }
            continue;
        }
        let Some(child_fork) = branch_fork_from_ancestor(timeline, *candidate, current) else {
            continue;
        };
        if from_tick >= info.fork_tick && from_tick <= child_fork {
            return Ok(*candidate);
        }
    }
    // Root fallback: tick 0 baseline belongs to the root even though the
    // strict interval `(own_fork, child_fork]` excludes it.
    if from_tick == 0
        && let Some(root) = chain.last().copied()
    {
        return Ok(root);
    }
    Err(anyhow!(
        "branch target {from_tick} is not owned by any ancestor of {current:?}"
    ))
}

/// Marks the current tick completed in the replay log, including empty
/// frames.
///
/// The `record_replay_step` system only appends records for drained inputs;
/// ticks with no input would otherwise leave `end_tick`/`cursor_tick` stale.
/// This is the runner-side equivalent of `ReplayLog::record_tick`: it
/// advances the recording bounds to the current tick unconditionally.
/// Newer replay crates may expose `modern`/`completed_ticks` fields or an
/// explicit `record_tick` API; this helper uses only the stable
/// `end_tick`/`cursor_tick` bounds so it compiles against both shapes
/// (callers should prefer the native API when present and fall back here).
fn mark_tick_completed(world: &mut World) {
    let tick = world
        .get_resource::<SimClock>()
        .map(|clock| clock.tick)
        .unwrap_or(0);
    let branch = world
        .get_resource::<bevy_agent_core::AgentControlState>()
        .map(|c| c.branch_id);
    if let Some(mut recorder) = world.get_resource_mut::<ReplayRecorder>() {
        recorder.log.end_tick = recorder.log.end_tick.max(tick);
        recorder.log.cursor_tick = recorder.log.cursor_tick.max(tick);
        // Record per-branch completed tick (including empty frames).
        let branch = branch.unwrap_or_else(|| recorder.log.active_branch.unwrap_or_default());
        recorder
            .log
            .completed_ticks
            .entry(bevy_agent_replay::branch_checksum_key(branch))
            .or_default()
            .insert(tick);
    }
}

/// Ensures the log carries the modern topology shape (the runner-side
/// equivalent of `ensure_modern` / `new_live`): populates
/// `timeline_topology` from the live timeline and installs
/// `active_branch`/`cursor_tick` when missing. Called on reset so a fresh
/// episode is live by definition even before the first checkpoint.
fn ensure_modern_log(log: &mut ReplayLog, timeline: &Timeline, cursor_tick: u64) {
    if log.timeline_topology.is_empty() {
        log.timeline_topology = timeline.branches.values().cloned().collect();
        log.timeline_topology
            .sort_by_key(|branch| (branch.fork_tick, branch.branch_id.0));
    }
    if log.active_branch.is_none() {
        log.active_branch = Some(timeline.current_branch);
    }
    log.cursor_tick = log.cursor_tick.max(cursor_tick);
    log.end_tick = log.end_tick.max(cursor_tick);
    log.end_tick = log.end_tick.max(log.initial_tick);
}

/// Global end of recorded history (max over bounds, records, checkpoints,
/// and checksums) computed directly from [`ReplayLog`] fields. Used for
/// legacy / validation fallbacks where no per-branch bound applies.
fn global_end_tick(log: &ReplayLog) -> u64 {
    let mut end = log.end_tick.max(log.cursor_tick).max(log.initial_tick);
    for record in &log.records {
        end = end.max(record.tick);
    }
    for tick in log.checkpoints.keys() {
        end = end.max(*tick);
    }
    for checkpoint in &log.branch_checkpoints {
        end = end.max(checkpoint.tick);
    }
    for tick in log.snapshot_checksums.keys() {
        end = end.max(*tick);
    }
    for per_branch in log.branch_checksums.values() {
        for tick in per_branch.keys() {
            end = end.max(*tick);
        }
    }
    end
}

/// Per-branch recorded range `(start, end)` inclusive.
///
/// `start` is `log.initial_tick` (ancestors are visible from the baseline).
/// `end` is the maximum tick visible on `branch` via fork-bounded intervals:
/// visible records, visible branch checkpoints, and this branch's own
/// `branch_checksums` entries. Legacy tick maps (`checkpoints` /
/// `snapshot_checksums`) extend the range only for legacy logs without
/// branch-tagged data; otherwise stale legacy ticks from other branches must
/// not extend this branch's range.
fn recorded_range(log: &ReplayLog, timeline: &Timeline, branch: BranchId) -> (u64, u64) {
    use bevy_agent_replay::branch_record_visible;
    let start = log.initial_tick;
    // Legacy logs (no topology, no branch-tagged data): global end.
    if !log.is_modern() && log.branch_checkpoints.is_empty() && log.branch_checksums.is_empty() {
        return (start, global_end_tick(log));
    }
    let modern = log.is_modern();
    let mut end = start;
    for record in &log.records {
        if record.tick <= end {
            continue;
        }
        if branch_record_visible(timeline, record.branch_id, record.tick, branch, modern) {
            end = end.max(record.tick);
        }
    }
    for checkpoint in &log.branch_checkpoints {
        if checkpoint.tick <= end {
            continue;
        }
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
    // Own branch checksums only; never other branches' entries.
    if let Some(per_branch) = log
        .branch_checksums
        .get(&bevy_agent_replay::branch_checksum_key(branch))
    {
        for tick in per_branch.keys() {
            end = end.max(*tick);
        }
    }
    // Completed ticks (including empty resolved frames) extend this branch.
    if let Some(ticks) = log
        .completed_ticks
        .get(&bevy_agent_replay::branch_checksum_key(branch))
    {
        for tick in ticks {
            // Only count ticks within this branch's visible lineage interval.
            if *tick >= start && branch_record_visible(timeline, branch, *tick, branch, modern) {
                end = end.max(*tick);
            }
        }
    }
    // Recording bounds advanced every executed tick (including empty frames)
    // also extend the range, clamped to this branch's visibility: the global
    // cursor/end belongs to the active branch history; other branches use
    // their own completed ticks above.
    if Some(branch) == log.active_branch {
        end = end.max(log.end_tick.max(log.cursor_tick));
    }
    // Legacy maps count only when no branch-tagged data exists at all
    // (pure legacy log); otherwise they are stale aliases from other branches.
    if log.branch_checkpoints.is_empty() && log.branch_checksums.is_empty() {
        for tick in log.checkpoints.keys().chain(log.snapshot_checksums.keys()) {
            end = end.max(*tick);
        }
    }
    // Initial snapshot covers the baseline even with no other data.
    if log.initial_snapshot.is_some() {
        end = end.max(start);
    }
    (start, end)
}

/// Bounds + lineage visibility check for history navigation targets.
///
/// Rejects `tick < start` (lower bound), `tick > end` (per-branch upper
/// bound from [`recorded_range`]), unknown branches, and over-deep/cyclic
/// lineages — all BEFORE any world mutation so failed navigation leaves
/// state untouched. Stale legacy ticks beyond this branch's recorded range
/// are rejected even when the global log extends further on other branches.
fn validate_history_target(
    log: &ReplayLog,
    timeline: &Timeline,
    branch: BranchId,
    tick: u64,
) -> Result<()> {
    check_lineage_bounded(timeline, branch)?;
    if !timeline.branches.contains_key(&branch) {
        return Err(anyhow!(
            "history target {tick} references unknown branch {branch:?}"
        ));
    }
    let (start, end) = recorded_range(log, timeline, branch);
    if tick < start {
        return Err(anyhow!(
            "history target {tick} precedes recording baseline {start}"
        ));
    }
    if tick > end {
        return Err(anyhow!(
            "restore target {tick} is beyond recorded end {end}"
        ));
    }
    Ok(())
}

/// Defends lineage traversal against cycles/corruption: walks parent links
/// with a hard bound ([`MAX_LINEAGE_DEPTH`]).
fn check_lineage_bounded(timeline: &Timeline, branch: BranchId) -> Result<()> {
    let mut visited = HashSet::new();
    let mut current = Some(branch);
    for _ in 0..MAX_LINEAGE_DEPTH {
        let Some(id) = current else {
            return Ok(());
        };
        if !visited.insert(id) {
            return Err(anyhow!("timeline lineage for {branch:?} is cyclic"));
        }
        match timeline.branches.get(&id) {
            None => return Ok(()),
            Some(info) => current = info.parent_branch,
        }
    }
    // Exhausted the bound without reaching a root: fail closed. Touch the
    // replay traversal helpers so coverage keeps them exercised.
    let _ = lineage_contains(timeline, branch, branch);
    let _ = branch_fork_from_ancestor(timeline, branch, branch);
    Err(anyhow!(
        "timeline lineage for {branch:?} exceeds maximum depth {MAX_LINEAGE_DEPTH}"
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResetOptions {
    pub seed: Option<u64>,
    pub observation_mode: ObservationMode,
    pub create_initial_snapshot: bool,
}

impl Default for ResetOptions {
    fn default() -> Self {
        Self {
            seed: Some(0),
            observation_mode: ObservationMode::Hybrid,
            create_initial_snapshot: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VisualCaptureOptions {
    pub output_dir: PathBuf,
    pub label: Option<String>,
    pub timeout_frames: u32,
    #[serde(default)]
    pub source: CaptureSource,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    #[default]
    Auto,
    Software,
    PrimaryWindow,
}

impl Default for VisualCaptureOptions {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("screenshots"),
            label: None,
            timeout_frames: 8,
            source: CaptureSource::Auto,
        }
    }
}

/// A portable replay artifact containing every snapshot referenced by its log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayBundle {
    pub format_version: u32,
    pub log: ReplayLog,
    pub snapshots: Vec<Snapshot>,
}

impl ReplayBundle {
    pub const FORMAT_VERSION: u32 = 1;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisualCaptureResult {
    pub tick: u64,
    pub frame: u64,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub format: String,
}

type VisualCaptureFn =
    dyn Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult> + Send + Sync;

#[derive(Resource)]
pub struct AgentVisualCaptureRenderer {
    capture: Box<VisualCaptureFn>,
}

impl AgentVisualCaptureRenderer {
    pub fn new<F>(capture: F) -> Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static,
    {
        Self {
            capture: Box::new(capture),
        }
    }

    pub fn capture(
        &self,
        world: &mut World,
        options: &VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        (self.capture)(world, options)
    }
}

pub trait VisualCaptureAppExt {
    fn insert_visual_capture_renderer<F>(&mut self, capture: F) -> &mut Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static;
}

impl VisualCaptureAppExt for App {
    fn insert_visual_capture_renderer<F>(&mut self, capture: F) -> &mut Self
    where
        F: Fn(&mut World, &VisualCaptureOptions) -> Result<VisualCaptureResult>
            + Send
            + Sync
            + 'static,
    {
        self.insert_resource(AgentVisualCaptureRenderer::new(capture))
    }
}

#[must_use]
pub fn sanitized_capture_label(label: Option<&str>) -> String {
    let label = label.unwrap_or("capture");
    let mut sanitized = String::new();
    let mut previous_dash = false;
    for character in label.chars() {
        let next = if character.is_ascii_alphanumeric() {
            previous_dash = false;
            Some(character.to_ascii_lowercase())
        } else if character == '-' || character == '_' || character.is_ascii_whitespace() {
            if previous_dash {
                None
            } else {
                previous_dash = true;
                Some('-')
            }
        } else {
            None
        };
        if let Some(next) = next {
            sanitized.push(next);
        }
    }
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "capture".to_string()
    } else {
        sanitized.to_string()
    }
}

pub fn visual_capture_path(
    options: &VisualCaptureOptions,
    tick: u64,
    frame: u64,
) -> Result<PathBuf> {
    std::fs::create_dir_all(&options.output_dir)?;
    let label = sanitized_capture_label(options.label.as_deref());
    let stem = format!("tick-{tick:06}-frame-{frame:06}-{label}");
    unique_path(&options.output_dir, &stem, "png")
}

fn unique_path(directory: &Path, stem: &str, extension: &str) -> Result<PathBuf> {
    let first = directory.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return Ok(first);
    }

    for index in 1..10_000 {
        let candidate = directory.join(format!("{stem}-{index}.{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "could not find an unused capture path for {}",
        directory.display()
    ))
}

pub trait AgentEnvironment {
    type Action;
    type Observation;

    fn reset(&mut self, options: ResetOptions) -> Result<Self::Observation>;

    fn step(&mut self, action: Self::Action) -> Result<StepResponse<Self::Observation>>;

    fn step_many(
        &mut self,
        actions: Vec<Self::Action>,
    ) -> Result<Vec<StepResponse<Self::Observation>>>;

    fn observe(&mut self, mode: ObservationMode) -> Result<Self::Observation>;

    fn snapshot(&mut self) -> Result<SnapshotCreateResult>;

    fn restore(&mut self, snapshot: SnapshotId) -> Result<()>;

    fn restore_tick(&mut self, tick: u64) -> Result<()>;

    fn branch(
        &mut self,
        from_tick: u64,
        label: Option<String>,
    ) -> Result<bevy_agent_core::BranchId>;
}

pub struct AgentApp {
    app: App,
    started: bool,
    reset_once: bool,
}

impl AgentApp {
    pub fn new(build_app: impl FnOnce() -> App) -> Self {
        Self::from_app(build_app())
    }

    pub fn from_app(mut app: App) -> Self {
        app.finish();
        app.cleanup();
        Self {
            app,
            started: false,
            reset_once: false,
        }
    }

    /// Wraps an app whose plugins have already been finished by an external
    /// runner. This is primarily used by main-thread integrations that lend
    /// their world to the synchronous control API for one request.
    pub fn from_running_app(app: App, reset_once: bool) -> Self {
        Self {
            app,
            started: true,
            reset_once,
        }
    }

    pub fn into_app(self) -> App {
        self.app
    }

    #[must_use]
    pub const fn has_reset(&self) -> bool {
        self.reset_once
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn world(&self) -> &World {
        self.app.world()
    }

    pub fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }

    pub fn step_frame(&mut self) {
        self.ensure_started();
        self.app.update();
    }

    pub fn current_tick(&self) -> u64 {
        self.app.world().resource::<SimClock>().tick
    }

    pub fn enqueue_action_at(&mut self, tick: u64, source: ActionSource, action: AgentAction) {
        self.app
            .world_mut()
            .resource_mut::<AgentActionQueue>()
            .schedule(tick, source, action);
    }

    pub fn fast_forward(&mut self, ticks: u64) -> Result<StepResponse<Observation>> {
        let mut last = None;
        for _ in 0..ticks {
            let response = self.step(AgentAction::Noop)?;
            let done = response.done || response.truncated;
            last = Some(response);
            if done {
                break;
            }
        }
        last.ok_or_else(|| anyhow!("fast_forward called with zero ticks"))
    }

    pub fn capture_visual(&mut self, options: VisualCaptureOptions) -> Result<VisualCaptureResult> {
        self.ensure_started();
        self.ensure_reset()?;

        match options.source {
            CaptureSource::PrimaryWindow => self.capture_primary_window(options),
            CaptureSource::Software => {
                if !self
                    .app
                    .world()
                    .contains_resource::<AgentVisualCaptureRenderer>()
                {
                    return Err(anyhow!(
                        "software visual capture requested, but no AgentVisualCaptureRenderer is registered"
                    ));
                }
                self.app.world_mut().resource_scope(
                    |world, renderer: Mut<AgentVisualCaptureRenderer>| {
                        renderer.capture(world, &options)
                    },
                )
            }
            CaptureSource::Auto => {
                if self
                    .app
                    .world()
                    .contains_resource::<AgentVisualCaptureRenderer>()
                {
                    return self.app.world_mut().resource_scope(
                        |world, renderer: Mut<AgentVisualCaptureRenderer>| {
                            renderer.capture(world, &options)
                        },
                    );
                }
                self.capture_primary_window(options)
            }
        }
    }

    pub fn capture_primary_window(
        &mut self,
        options: VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        #[cfg(feature = "visual")]
        {
            self.capture_primary_window_impl(options)
        }

        #[cfg(not(feature = "visual"))]
        {
            let _ = options;
            Err(anyhow!(
                "visual capture requires a registered AgentVisualCaptureRenderer or the bevy_agent_runner visual feature"
            ))
        }
    }

    pub fn replay_log(&self) -> Option<&ReplayLog> {
        self.app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| &recorder.log)
    }

    /// Fault gate: history corruption that failed rollback leaves a
    /// [`FaultState`] resource; all navigation/stepping is rejected until an
    /// explicit `reset` clears it (reset removes the resource).
    fn ensure_no_fault(&self) -> Result<()> {
        if let Some(fault) = self.app.world().get_resource::<FaultState>() {
            return Err(anyhow!(
                "world is faulted ({}); reset before stepping",
                fault.message
            ));
        }
        Ok(())
    }

    /// Single checkpoint owner: every snapshot creation in the runner routes
    /// through the role-aware API, then retention is enforced with the live
    /// reference set derived from the current replay log. Never prunes with
    /// an empty set on a creation path.
    ///
    /// The snapshot crate's creation path enforces retention internally with
    /// no replay visibility, so live log references are temporarily pinned
    /// across the creation call: referenced snapshots can never be evicted by
    /// the creation itself. Temporary pins are released afterwards (pre-
    /// existing pins are left untouched); post-creation retention is then
    /// enforced with the live reference set.
    fn create_owned_checkpoint(
        &mut self,
        label: Option<String>,
        role: SnapshotRole,
    ) -> Result<SnapshotCreateResult> {
        // Temporarily pin live log references so the creation's internal
        // retention pass cannot evict a snapshot the log still needs.
        let already_pinned: BTreeSet<SnapshotId> = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .map(|store| store.pinned.clone())
            .unwrap_or_default();
        let live_refs: BTreeSet<SnapshotId> = self
            .replay_log()
            .map(live_snapshot_refs)
            .unwrap_or_default();
        let temp_pinned: Vec<SnapshotId> = live_refs
            .iter()
            .filter(|id| !already_pinned.contains(id))
            .copied()
            .collect();
        for id in &temp_pinned {
            pin_snapshot(self.app.world_mut(), *id);
        }
        let result = create_snapshot_with_role(self.app.world_mut(), label, role);
        for id in &temp_pinned {
            unpin_snapshot(self.app.world_mut(), *id);
        }
        let result = result?;
        self.prune_with_live_refs();
        Ok(result)
    }

    /// Enforces retention with live refs from the replay log (no-op without
    /// snapshot support or without a recorder).
    fn prune_with_live_refs(&mut self) {
        let (keep, refs) = match (
            self.app.world().get_resource::<SnapshotPolicy>(),
            self.app.world().get_resource::<ReplayRecorder>(),
        ) {
            (Some(policy), Some(recorder)) => (
                policy.keep_last_n_checkpoints,
                live_snapshot_refs(&recorder.log),
            ),
            (Some(policy), None) => (policy.keep_last_n_checkpoints, BTreeSet::new()),
            _ => return,
        };
        prune_checkpoints_with_refs(self.app.world_mut(), keep, &refs);
    }

    /// Coordinated delete: rejects snapshots that are pinned or referenced
    /// by the live replay log (checked-delete semantics).
    pub fn delete_snapshot(&mut self, id: SnapshotId) -> Result<()> {
        let refs = self
            .replay_log()
            .map(live_snapshot_refs)
            .unwrap_or_default();
        delete_snapshot_checked(self.app.world_mut(), id, &refs)
    }

    pub fn reset_with_response(
        &mut self,
        options: ResetOptions,
    ) -> Result<ResetResponse<Observation>> {
        let observation = <Self as AgentEnvironment>::reset(self, options)?;
        let response = self.last_response()?;
        Ok(ResetResponse {
            tick: response.tick,
            observation,
            checksum: response.checksum,
            snapshot_id: response.info.snapshot_created,
            timeline_id: response.info.timeline_id,
            branch_id: response.info.branch_id,
        })
    }

    pub fn step_many_with_response(
        &mut self,
        actions: Vec<AgentAction>,
        stop_on_done: bool,
        include_responses: bool,
    ) -> Result<StepManyResponse<Observation>> {
        // Controller boundary: validate the whole batch BEFORE any
        // truncate/enqueue/advance so an invalid action rejects the batch
        // without partial stepping.
        self.validate_batch(&actions)?;
        let start_tick = self.current_tick();
        // Incremental aggregation: retain only requested observations
        // (last always, all only when `include_responses`), summing rewards
        // as we go instead of retaining then summing.
        let mut retained: Vec<StepResponse<Observation>> = Vec::new();
        if include_responses {
            retained.reserve(actions.len());
        }
        let mut steps = 0usize;
        let mut reward = 0.0f32;
        let mut observation: Option<Observation> = None;
        let mut done = false;
        let mut truncated = false;
        let mut info: Option<bevy_agent_core::StepInfo> = None;
        let mut checksum: Option<bevy_agent_core::EnvironmentChecksum> = None;
        let mut end_tick = start_tick;
        for action in actions {
            let response = self.step(action)?;
            let terminal = response.done || response.truncated;
            end_tick = response.tick;
            steps += 1;
            reward += response.reward;
            done = response.done;
            truncated = response.truncated;
            observation = Some(response.observation.clone());
            info = Some(response.info.clone());
            checksum = response.checksum.clone();
            if include_responses {
                retained.push(response);
            }
            if stop_on_done && terminal {
                break;
            }
        }
        Ok(StepManyResponse {
            start_tick,
            end_tick,
            steps,
            observation,
            reward,
            done,
            truncated,
            info,
            checksum,
            responses: retained,
        })
    }

    /// Validates a whole batch against the action catalog, rejecting the batch
    /// on the first invalid action.
    fn validate_batch(&self, actions: &[AgentAction]) -> Result<()> {
        let catalog = self
            .app
            .world()
            .get_resource::<AgentActionCatalog>()
            .cloned()
            .unwrap_or_default();
        for action in actions {
            validate_action_against_catalog(&catalog, action)
                .map_err(|error| anyhow!("InvalidAction: {error}"))?;
        }
        Ok(())
    }

    pub fn export_replay_bundle(&self) -> Result<ReplayBundle> {
        let mut log = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.log.clone())
            .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
        // Export writes the timeline topology verbatim: branches, active
        // branch, cursor tick, and recording bounds.
        if let (Some(timeline), Some(clock)) = (
            self.app.world().get_resource::<Timeline>(),
            self.app.world().get_resource::<SimClock>(),
        ) {
            let cursor = clock.tick;
            log.sync_topology(timeline, cursor);
        }
        let store = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .ok_or_else(|| anyhow!("AgentSnapshotPlugin is not installed"))?;

        // Retention integrity: every snapshot referenced by the log
        // (initial, legacy + branch-tagged checkpoints, and topology fork
        // snapshots) must be present. Built from `collect_replay_references`
        // so fork snapshots are included.
        let provided = store.snapshots.keys().copied().collect::<BTreeSet<_>>();
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
                    .snapshots
                    .get(&id)
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

        Ok(ReplayBundle {
            format_version: ReplayBundle::FORMAT_VERSION,
            log,
            snapshots,
        })
    }

    pub fn load_replay_bundle(&mut self, bundle: ReplayBundle) -> Result<()> {
        if bundle.format_version != ReplayBundle::FORMAT_VERSION {
            return Err(anyhow!(
                "unsupported replay bundle format {}; expected {}",
                bundle.format_version,
                ReplayBundle::FORMAT_VERSION
            ));
        }
        let environment = self
            .app
            .world()
            .get_resource::<EnvironmentMetadata>()
            .cloned()
            .unwrap_or_default();
        if environment.name != "unknown-game" && bundle.log.manifest.game_id != environment.name {
            return Err(anyhow!(
                "replay game mismatch: expected {}, got {}",
                environment.name,
                bundle.log.manifest.game_id
            ));
        }
        if environment.name != "unknown-game"
            && bundle.log.manifest.game_version != environment.version
        {
            return Err(anyhow!(
                "replay version mismatch: expected {}, got {}",
                environment.version,
                bundle.log.manifest.game_version
            ));
        }

        // Validate exported topology when present: active branch must exist,
        // parent links must resolve, fork ticks must be ordered.
        if !bundle.log.timeline_topology.is_empty() {
            use std::collections::HashSet;
            let ids: HashSet<_> = bundle
                .log
                .timeline_topology
                .iter()
                .map(|branch| branch.branch_id)
                .collect();
            if bundle.log.timeline_topology.len() != ids.len() {
                return Err(anyhow!("replay bundle has duplicate branch ids"));
            }
            // Cyclic safety: reject self-parent links and parent cycles
            // (bounded walk) before installing topology.
            validate_import_topology_acyclic(&bundle.log.timeline_topology)?;
            for branch in &bundle.log.timeline_topology {
                if let Some(parent) = branch.parent_branch {
                    if !ids.contains(&parent) {
                        return Err(anyhow!(
                            "replay bundle branch {:?} has unknown parent {:?}",
                            branch.branch_id,
                            parent
                        ));
                    }
                    let parent_tick = bundle
                        .log
                        .timeline_topology
                        .iter()
                        .find(|candidate| candidate.branch_id == parent)
                        .map(|candidate| candidate.fork_tick)
                        .unwrap_or(0);
                    if branch.fork_tick < parent_tick {
                        return Err(anyhow!(
                            "replay bundle branch {:?} fork {} precedes parent fork {}",
                            branch.branch_id,
                            branch.fork_tick,
                            parent_tick
                        ));
                    }
                }
                // Every branch-tagged record/checkpoint must reference a known
                // branch in modern bundles; unknown ids are rejected.
                for record in bundle
                    .log
                    .records
                    .iter()
                    .filter(|record| record.branch_id == branch.branch_id)
                {
                    let _ = record.tick;
                }
            }
            for record in &bundle.log.records {
                if !ids.contains(&record.branch_id) {
                    return Err(anyhow!(
                        "replay bundle record references unknown branch {:?}",
                        record.branch_id
                    ));
                }
            }
            for checkpoint in &bundle.log.branch_checkpoints {
                if !ids.contains(&checkpoint.branch_id) {
                    return Err(anyhow!(
                        "replay bundle checkpoint references unknown branch {:?}",
                        checkpoint.branch_id
                    ));
                }
            }
            if let Some(active) = bundle.log.active_branch
                && !ids.contains(&active)
            {
                return Err(anyhow!("replay bundle active branch {active:?} is unknown"));
            }
        }

        let referenced = collect_replay_references(&bundle.log);
        let provided = bundle
            .snapshots
            .iter()
            .map(|snapshot| snapshot.manifest.snapshot_id)
            .collect::<BTreeSet<_>>();
        if let Some(missing) = referenced.difference(&provided).next() {
            return Err(anyhow!(
                "replay bundle is missing referenced snapshot {missing:?}"
            ));
        }

        // Checkpoint temporal validation BEFORE installing: every
        // branch/legacy checkpoint must reference a snapshot whose clock and
        // manifest ticks equal the checkpoint tick; the baseline initial
        // snapshot tick must equal `initial_tick`; fork snapshots must equal
        // their branch `fork_tick`. Rejects before any mutation.
        validate_import_temporal(&bundle)?;

        // Typed validation for EVERY referenced snapshot (not only the cursor
        // snapshot). `bevy_agent_snapshot` exposes no public prepare/validate
        // entry point (`prepare_restore_plan` is private), so the fallback is
        // a serde decode round-trip plus `clock.validate` plus checksum
        // recompute. Failures return Err without touching world state
        // (transactional import).
        for snapshot in &bundle.snapshots {
            if !referenced.contains(&snapshot.manifest.snapshot_id) {
                continue;
            }
            // Serde decode check: typed round-trip through JSON.
            let encoded = serde_json::to_value(snapshot).map_err(|error| {
                anyhow!(
                    "replay bundle snapshot {:?} failed typed encode check: {error}",
                    snapshot.manifest.snapshot_id
                )
            })?;
            let _decoded: Snapshot = serde_json::from_value(encoded).map_err(|error| {
                anyhow!(
                    "replay bundle snapshot {:?} failed typed decode check: {error}",
                    snapshot.manifest.snapshot_id
                )
            })?;
            if snapshot.manifest.tick != snapshot.clock.tick {
                return Err(anyhow!(
                    "replay bundle snapshot {:?} manifest/clock tick mismatch: manifest.tick={} clock.tick={}",
                    snapshot.manifest.snapshot_id,
                    snapshot.manifest.tick,
                    snapshot.clock.tick,
                ));
            }
            snapshot.clock.validate().map_err(|error| {
                anyhow!(
                    "replay bundle snapshot {:?} has invalid clock: {error}",
                    snapshot.manifest.snapshot_id
                )
            })?;
            let recomputed = checksum_snapshot(snapshot).map_err(|error| {
                anyhow!(
                    "replay bundle snapshot {:?} failed decode check: {error:?}",
                    snapshot.manifest.snapshot_id
                )
            })?;
            if recomputed.hash != snapshot.checksum.hash
                || recomputed.tick != snapshot.checksum.tick
            {
                return Err(anyhow!(
                    "replay bundle snapshot {:?} checksum precondition failed: stored {:?}, recomputed {:?}",
                    snapshot.manifest.snapshot_id,
                    snapshot.checksum,
                    recomputed
                ));
            }
        }
        // Registry decode check (no mutation): referenced payloads must only
        // name registered resources/components.
        if let Some(registry) = self
            .app
            .world()
            .get_resource::<bevy_agent_snapshot::SnapshotRegistry>()
        {
            for snapshot in &bundle.snapshots {
                if !referenced.contains(&snapshot.manifest.snapshot_id) {
                    continue;
                }
                for resource in &snapshot.resources {
                    if !registry
                        .resource_serializers
                        .contains_key(resource.type_name.as_str())
                    {
                        return Err(anyhow!(
                            "replay bundle snapshot {:?} references unregistered resource {}",
                            snapshot.manifest.snapshot_id,
                            resource.type_name
                        ));
                    }
                }
                for absent in &snapshot.absent_resources {
                    if !registry.resource_serializers.contains_key(absent.as_str()) {
                        return Err(anyhow!(
                            "replay bundle snapshot {:?} references unregistered absent resource {absent}",
                            snapshot.manifest.snapshot_id,
                        ));
                    }
                }
                for entity in &snapshot.entities {
                    for component in &entity.components {
                        if !registry
                            .component_serializers
                            .contains_key(component.type_name.as_str())
                        {
                            return Err(anyhow!(
                                "replay bundle snapshot {:?} references unregistered component {}",
                                snapshot.manifest.snapshot_id,
                                component.type_name
                            ));
                        }
                    }
                }
            }
        }

        // Preconditions requiring plugins: checked BEFORE any mutation so a
        // missing plugin returns Err without touching fault/reset state.
        if !self.app.world().contains_resource::<SnapshotStore>() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }
        if !self.app.world().contains_resource::<ReplayRecorder>() {
            return Err(anyhow!("AgentReplayPlugin is not installed"));
        }
        // World backup for transactional rollback (entities/resources/clock).
        // Captured before mutation; a capture failure aborts without install.
        let world_backup = if self.has_snapshot_support() {
            match capture_snapshot(self.app.world_mut(), None) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    return Err(anyhow!("replay import backup failed: {error:?}"));
                }
            }
        } else {
            None
        };
        let store_backup = self.app.world().get_resource::<SnapshotStore>().cloned();
        let recorder_backup = self.app.world().get_resource::<ReplayRecorder>().cloned();
        let timeline_backup = self.app.world().get_resource::<Timeline>().cloned();
        let control_backup = self
            .app
            .world()
            .get_resource::<AgentControlState>()
            .cloned();
        let last_backup = self.app.world().get_resource::<LastStepResponse>().cloned();
        let reset_once_backup = self.reset_once;

        // Pre-move bundle facts (`bundle.snapshots` / `bundle.log` are moved
        // into the store/recorder during install).
        let bundle_has_snapshots = !bundle.snapshots.is_empty();
        let bundle_has_history = !bundle.log.records.is_empty()
            || !bundle.log.branch_checkpoints.is_empty()
            || !bundle.log.checkpoints.is_empty()
            || bundle.log.initial_snapshot.is_some();
        let bundle_cursor = bundle.log.cursor_tick;
        // Imported pending queues by snapshot id (cloned pre-move) for the
        // `ReplaceFromImport` policy applied after activation.
        let imported_queues: BTreeMap<
            SnapshotId,
            Vec<bevy_agent_core::ScheduledAction<AgentAction>>,
        > = bundle
            .snapshots
            .iter()
            .map(|snapshot| (snapshot.manifest.snapshot_id, snapshot.action_queue.clone()))
            .collect();

        // Install helper with rollback on activation failure. Returns `Result`:
        // a backup-restore failure is reported so the caller can install
        // `FaultState` carrying both the original and the rollback error.
        let rollback = |agent: &mut AgentApp| -> Result<()> {
            if let Some(store) = store_backup.clone()
                && let Some(mut live) = agent.app.world_mut().get_resource_mut::<SnapshotStore>()
            {
                *live = store;
            }
            if let Some(recorder) = recorder_backup.clone()
                && let Some(mut live) = agent.app.world_mut().get_resource_mut::<ReplayRecorder>()
            {
                *live = recorder;
            }
            if let Some(timeline) = timeline_backup.clone()
                && let Some(mut live) = agent.app.world_mut().get_resource_mut::<Timeline>()
            {
                *live = timeline;
            }
            if let Some(control) = control_backup.clone()
                && let Some(mut live) = agent
                    .app
                    .world_mut()
                    .get_resource_mut::<AgentControlState>()
            {
                *live = control;
            }
            if let Some(last) = last_backup.clone()
                && let Some(mut live) = agent.app.world_mut().get_resource_mut::<LastStepResponse>()
            {
                *live = last;
            }
            agent.reset_once = reset_once_backup;
            if let Some(backup) = world_backup.clone() {
                restore_snapshot_value(agent.app.world_mut(), &backup)?;
            }
            Ok(())
        };

        // ---- Install log + store + timeline ----
        {
            let mut store = self
                .app
                .world_mut()
                .get_resource_mut::<SnapshotStore>()
                .ok_or_else(|| anyhow!("AgentSnapshotPlugin is not installed"))?;
            for snapshot in bundle.snapshots {
                let id = snapshot.manifest.snapshot_id;
                if let Some(label) = snapshot.manifest.label.clone() {
                    store.labels.insert(label, id);
                }
                store.snapshots.insert(id, snapshot);
            }
            // Install the checkpoint list so retention bookkeeping matches the
            // imported log (deduplicated, ordered by log tick). Covers all
            // referenced ids (initial, checkpoints, branch checkpoints, fork
            // snapshots) so retention sees the full live set.
            let mut checkpoint_ids = collect_replay_references(&bundle.log)
                .into_iter()
                .collect::<Vec<_>>();
            checkpoint_ids.sort();
            checkpoint_ids.dedup();
            for id in checkpoint_ids {
                if !store.checkpoints.contains(&id) {
                    store.checkpoints.push(id);
                }
            }
            let tick_of = |id: &SnapshotId| {
                bundle
                    .log
                    .branch_checkpoints
                    .iter()
                    .find(|checkpoint| &checkpoint.snapshot_id == id)
                    .map(|checkpoint| checkpoint.tick)
                    .or_else(|| {
                        bundle
                            .log
                            .checkpoints
                            .iter()
                            .find(|(_, snapshot_id)| *snapshot_id == id)
                            .map(|(tick, _)| *tick)
                    })
                    .unwrap_or(u64::MAX)
            };
            store.checkpoints.sort_by_key(tick_of);
        }
        // Preserve the recording flag across the import; only the log content
        // is replaced.
        let recording = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.recording)
            .unwrap_or(false);
        self.app
            .world_mut()
            .get_resource_mut::<ReplayRecorder>()
            .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?
            .log = bundle.log;
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.recording = recording;
        }
        // Initialize control/timeline from the imported log. Do NOT clear
        // faults and do NOT set `reset_once` yet: both happen only on
        // successful activation below.
        self.sync_timeline_with_log();
        // Missing baseline: records without any snapshots have no restorable
        // baseline. Keep the log-only install without activation
        // (`reset_once=false`); history that needs a baseline requires an
        // explicit reset first.
        if !bundle_has_snapshots {
            self.reset_once = reset_once_backup;
            if bundle_has_history {
                return Err(anyhow!(
                    "replay bundle has records but no snapshots: no restorable baseline; explicit reset required before activation"
                ));
            }
            return Ok(());
        }
        // Position on the exported cursor when snapshots are present,
        // INCLUDING cursor-zero (restore the initial snapshot at tick 0).
        // Any activation failure returns Err WITHOUT clearing FaultState and
        // WITHOUT setting `reset_once=true`; the install above is rolled
        // back so stepping cannot proceed on half-activated state (leave
        // `reset_once=false` when activation is deferred/failed).
        let cursor = bundle_cursor;
        // Import queue policy (`ReplaceFromImport`): destination futures must
        // NOT merge into the imported history, so clear the live queue before
        // the activation restore (`restore_tick` merges pre-restore futures
        // under `PreserveCurrentFuture`; starting empty leaves nothing to
        // merge).
        if let Some(mut queue) = self.app.world_mut().get_resource_mut::<AgentActionQueue>() {
            queue.clear();
        }
        let has_snapshots = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .is_some_and(|store| !store.snapshots.is_empty());
        if has_snapshots {
            // `restore_tick` gates on `reset_once` via `ensure_reset` (which
            // would wipe the just-installed log). Temporarily mark active so
            // the restore runs against the imported history; on failure the
            // backup (usually `false`) is restored, rejecting stepping until
            // an explicit successful activation.
            let saved_reset_once = self.reset_once;
            self.reset_once = true;
            let restore_result = self.restore_tick(cursor);
            if let Err(error) = restore_result {
                self.reset_once = saved_reset_once;
                if let Err(rollback_error) = rollback(self) {
                    let message = format!(
                        "replay import activation failed ({error:?}) and rollback failed ({rollback_error:?}); world may be faulted"
                    );
                    self.app.world_mut().insert_resource(FaultState {
                        message: message.clone(),
                    });
                    // Do NOT clear FaultState; do NOT set reset_once=true.
                    return Err(anyhow!("{message}"));
                }
                // Do NOT clear FaultState; do NOT set reset_once=true.
                return Err(error);
            }
            // `ReplaceFromImport`: set the live pending queue from the
            // imported cursor snapshot's `action_queue` (or empty when the
            // cursor checkpoint has no queued inputs).
            let imported_pending = {
                let (log, timeline, branch) = (
                    self.app
                        .world()
                        .get_resource::<ReplayRecorder>()
                        .map(|recorder: &ReplayRecorder| recorder.log.clone()),
                    self.app.world().get_resource::<Timeline>().cloned(),
                    self.app
                        .world()
                        .get_resource::<AgentControlState>()
                        .map(|control| control.branch_id),
                );
                match (log, timeline, branch) {
                    (Some(log), Some(timeline), Some(branch)) => log
                        .nearest_checkpoint_for_branch(&timeline, branch, cursor)
                        .and_then(|(_, id)| imported_queues.get(&id).cloned())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                }
            };
            if let Some(mut queue) = self.app.world_mut().get_resource_mut::<AgentActionQueue>() {
                apply_pending_policy(
                    &mut queue,
                    Vec::new(),
                    imported_pending,
                    PendingInputPolicy::ReplaceFromImport,
                );
            }
            // Post-activation temporal assert: the world clock must match the
            // imported cursor, else the install is rolled back with an error.
            let live_tick = self
                .app
                .world()
                .get_resource::<SimClock>()
                .map(|clock| clock.tick)
                .unwrap_or(u64::MAX);
            if live_tick != cursor {
                self.reset_once = saved_reset_once;
                if let Err(rollback_error) = rollback(self) {
                    let message = format!(
                        "replay import cursor mismatch (world tick {live_tick} != cursor {cursor}) and rollback failed ({rollback_error:?}); world may be faulted"
                    );
                    self.app.world_mut().insert_resource(FaultState {
                        message: message.clone(),
                    });
                    return Err(anyhow!("{message}"));
                }
                return Err(anyhow!(
                    "replay import cursor mismatch: world tick {live_tick} != cursor {cursor}"
                ));
            }
        }
        // Success: mark activated and clear any prior fault.
        self.reset_once = true;
        self.app.world_mut().remove_resource::<FaultState>();
        Ok(())
    }

    /// Starts a fresh recording, capturing the current state as the baseline
    /// when no snapshot is supplied and snapshot support is installed.
    /// The baseline tick is always the current tick: a caller-supplied
    /// snapshot is only reused when its own tick matches the current tick,
    /// otherwise a fresh baseline is captured (never register an old snapshot
    /// under the current tick). The baseline is stored in
    /// `log.initial_snapshot` + `log.initial_tick`.
    pub fn start_recording(&mut self, initial_snapshot: Option<SnapshotId>) -> Result<()> {
        self.ensure_started();
        let current = self.current_tick();
        let baseline = match initial_snapshot {
            Some(id) => {
                let tick_matches = self
                    .app
                    .world()
                    .get_resource::<SnapshotStore>()
                    .and_then(|store| store.snapshots.get(&id))
                    .is_some_and(|snapshot| snapshot.manifest.tick == current);
                if tick_matches {
                    Some(id)
                } else if self.has_snapshot_support() {
                    let result = self.create_owned_checkpoint(
                        Some(format!("baseline-{current}")),
                        SnapshotRole::RecordingBaseline,
                    )?;
                    Some(result.snapshot_id)
                } else {
                    Some(id)
                }
            }
            None => {
                if self.has_snapshot_support() {
                    let result = self.create_owned_checkpoint(
                        Some(format!("baseline-{current}")),
                        SnapshotRole::RecordingBaseline,
                    )?;
                    Some(result.snapshot_id)
                } else {
                    None
                }
            }
        };
        start_recording(self.app.world_mut(), baseline);
        // `start_recording` installs a fresh root with an empty log; the
        // baseline belongs to that root tick. Record bounds + checkpoint.
        if let Some(snapshot_id) = baseline {
            let checksum = self
                .app
                .world()
                .get_resource::<SnapshotStore>()
                .and_then(|store| store.snapshots.get(&snapshot_id))
                .and_then(|snapshot| queue_excluded_checksum(snapshot).ok());
            let branch = self
                .app
                .world()
                .get_resource::<Timeline>()
                .map(|timeline| timeline.current_branch);
            if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
                recorder.log.initial_tick = current;
                recorder.log.end_tick = recorder.log.end_tick.max(current);
                if let (Some(branch), Some(checksum)) = (branch, checksum) {
                    recorder.log.push_checkpoint(branch, current, snapshot_id);
                    recorder
                        .log
                        .insert_branch_checksum(branch, current, checksum);
                }
            }
            self.sync_auto_checkpoints();
            self.prune_with_live_refs();
        } else if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>()
        {
            recorder.log.initial_tick = current;
            recorder.log.end_tick = recorder.log.end_tick.max(current);
        }
        Ok(())
    }

    /// Rebuilds timeline branches referenced by the current replay log so
    /// control state matches imported history. Branches missing from the
    /// timeline are attached under the root with a fork tick at their first
    /// referenced tick, preserving ancestor visibility without fabricating
    /// deeper topology. The current branch becomes the branch with the newest
    /// referenced tick (root when the log is empty).
    fn sync_timeline_with_log(&mut self) {
        let log = match self.app.world().get_resource::<ReplayRecorder>() {
            Some(recorder) => recorder.log.clone(),
            None => return,
        };
        self.rebuild_timeline_from_log(&log);
    }

    /// Timeline rebuild helper behind [`sync_timeline_with_log`](Self::sync_timeline_with_log).
    /// Modern bundles install `timeline_topology` verbatim (branches,
    /// active branch, cursor). Legacy logs (empty topology) migrate all
    /// default/nil records to a single legacy root.
    fn rebuild_timeline_from_log(&mut self, log: &ReplayLog) {
        // Modern path: verbatim install, no inference.
        if !log.timeline_topology.is_empty() {
            let (timeline_id, current_branch) = {
                let Some(mut timeline) = self.app.world_mut().get_resource_mut::<Timeline>() else {
                    return;
                };
                timeline.branches.clear();
                for branch in &log.timeline_topology {
                    timeline.branches.insert(branch.branch_id, branch.clone());
                }
                // Re-populate per-branch action lists from the log for
                // branches whose exported actions may be stale.
                for branch in timeline.branches.values_mut() {
                    branch.actions = log
                        .records
                        .iter()
                        .filter(|record| record.branch_id == branch.branch_id)
                        .cloned()
                        .collect();
                }
                let active = log.active_branch.unwrap_or_else(|| {
                    timeline
                        .branches
                        .iter()
                        .find(|(_, branch)| branch.parent_branch.is_none())
                        .map(|(id, _)| *id)
                        .unwrap_or_else(|| {
                            let id = bevy_agent_core::BranchId::new();
                            timeline.branches.insert(
                                id,
                                bevy_agent_replay::TimelineBranch {
                                    branch_id: id,
                                    parent_branch: None,
                                    fork_tick: 0,
                                    fork_snapshot: None,
                                    label: Some("root".to_string()),
                                    actions: Vec::new(),
                                },
                            );
                            id
                        })
                });
                // If the recorded active branch vanished (should not happen
                // after validation), fall back to an existing branch.
                if timeline.branches.contains_key(&active) {
                    timeline.current_branch = active;
                } else if let Some((id, _)) =
                    timeline.branches.iter().next().map(|(id, b)| (*id, b))
                {
                    timeline.current_branch = id;
                }
                (timeline.timeline_id, timeline.current_branch)
            };
            if let Some(mut control) = self.app.world_mut().get_resource_mut::<AgentControlState>()
            {
                control.timeline_id = timeline_id;
                control.branch_id = current_branch;
            }
            return;
        }
        // Legacy migration: single LEGACY_ROOT for all default-UUID records.
        let legacy = legacy_root_id();
        let (timeline_id, current_branch) = {
            let Some(mut timeline) = self.app.world_mut().get_resource_mut::<Timeline>() else {
                return;
            };
            let root = timeline
                .branches
                .iter()
                .find(|(_, branch)| branch.parent_branch.is_none())
                .map(|(id, _)| *id)
                .unwrap_or(timeline.current_branch);
            // Migrate nil/default records to the root instead of fabricating
            // one child per unknown id: legacy logs collapse to a single root.
            let mut migrated_newest: u64 = 0;
            for record in &log.records {
                let effective = if record.branch_id == legacy {
                    root
                } else if timeline.branches.contains_key(&record.branch_id) {
                    record.branch_id
                } else {
                    // Legacy unknown ids collapse to root (single-root
                    // migration) instead of one-child-per-id inference.
                    root
                };
                migrated_newest = migrated_newest.max(record.tick);
                if let Some(branch) = timeline.branches.get_mut(&effective)
                    && !branch.actions.iter().any(|existing| {
                        existing.tick == record.tick && existing.action == record.action
                    })
                {
                    let mut migrated = record.clone();
                    migrated.branch_id = effective;
                    branch.actions.push(migrated);
                }
            }
            // Newest referenced tick decides current; legacy single-root logs
            // stay on root.
            let mut newest_tick = migrated_newest;
            for checkpoint in &log.branch_checkpoints {
                newest_tick = newest_tick.max(checkpoint.tick);
            }
            let _ = newest_tick;
            timeline.current_branch = root;
            (timeline.timeline_id, timeline.current_branch)
        };
        if let Some(mut control) = self.app.world_mut().get_resource_mut::<AgentControlState>() {
            control.timeline_id = timeline_id;
            control.branch_id = current_branch;
        }
        // Legacy migration normalization: rewrite nil/unknown branch ids in
        // the live log to the single root, normalize branch checkpoints, and
        // install topology/active/cursor so a subsequent export carries a
        // modern bundle (export-then-import passes).
        self.normalize_legacy_log(current_branch);
    }

    /// Normalizes a legacy (topology-less) log in place after timeline
    /// rebuild: nil (`legacy_root_id`) and otherwise-unknown branch ids in
    /// records and branch checkpoints collapse to `root`; topology, active
    /// branch, cursor, and recording bounds are installed.
    fn normalize_legacy_log(&mut self, root: BranchId) {
        let (known, topology): (Vec<BranchId>, Vec<TimelineBranch>) =
            match self.app.world().get_resource::<Timeline>() {
                Some(timeline) => (
                    timeline.branches.keys().copied().collect(),
                    timeline.branches.values().cloned().collect(),
                ),
                None => return,
            };
        let cursor = self
            .app
            .world()
            .get_resource::<SimClock>()
            .map(|clock| clock.tick)
            .unwrap_or(0);
        let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() else {
            return;
        };
        if !recorder.log.timeline_topology.is_empty() {
            return;
        }
        for record in recorder.log.records.iter_mut() {
            if is_legacy_branch(record.branch_id) || !known.contains(&record.branch_id) {
                record.branch_id = root;
            }
        }
        for checkpoint in recorder.log.branch_checkpoints.iter_mut() {
            if is_legacy_branch(checkpoint.branch_id) || !known.contains(&checkpoint.branch_id) {
                checkpoint.branch_id = root;
            }
        }
        let end = global_end_tick(&recorder.log).max(cursor);
        recorder.log.timeline_topology = topology;
        let mut min_tick = u64::MAX;
        for record in &recorder.log.records {
            min_tick = min_tick.min(record.tick);
        }
        for checkpoint in &recorder.log.branch_checkpoints {
            min_tick = min_tick.min(checkpoint.tick);
        }
        for tick in recorder.log.checkpoints.keys() {
            min_tick = min_tick.min(*tick);
        }
        if min_tick == u64::MAX {
            min_tick = cursor;
        }
        recorder.log.initial_tick = min_tick.min(cursor);
        recorder.log.end_tick = end;
        recorder.log.cursor_tick = cursor.max(recorder.log.cursor_tick);
        if recorder.log.active_branch.is_none() {
            recorder.log.active_branch = Some(root);
        }
    }

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.app.update();
        self.started = true;
    }

    fn ensure_reset(&mut self) -> Result<()> {
        if !self.reset_once {
            let _ = self.reset(ResetOptions::default())?;
        }
        Ok(())
    }

    fn run_one_agent_tick(&mut self) {
        run_agent_tick(self.app.world_mut());
        // Record every live tick — including empty frames with no input —
        // so recording bounds never go stale (see `mark_tick_completed`).
        // Reconstruction uses `run_one_reconstructing_tick` and never lands
        // here, so replayed history is not re-recorded.
        mark_tick_completed(self.app.world_mut());
    }

    fn last_response(&self) -> Result<StepResponse<Observation>> {
        self.app
            .world()
            .resource::<LastStepResponse>()
            .0
            .clone()
            .ok_or_else(|| anyhow!("agent tick produced no StepResponse"))
    }

    fn step_with_source(
        &mut self,
        action: AgentAction,
        source: ActionSource,
    ) -> Result<StepResponse<Observation>> {
        self.ensure_started();
        self.ensure_no_fault()?;
        self.ensure_reset()?;
        // Terminal enforcement: reject post-terminal steps before any state
        // change. The transition step that first reports terminal is allowed;
        // only steps after a terminal response are rejected (absorbing
        // semantics via `ensure_not_terminal`).
        {
            let last_terminal = self
                .app
                .world()
                .get_resource::<LastStepResponse>()
                .and_then(|last| last.0.as_ref())
                .is_some_and(|response| response.done || response.truncated);
            if last_terminal && let Some(episode) = self.app.world().get_resource::<EpisodeState>()
            {
                episode
                    .ensure_not_terminal()
                    .map_err(|error| anyhow!("{error}"))?;
            }
        }
        self.reject_paused_or_inspect_only()?;
        // Controller boundary: validate against the catalog BEFORE
        // truncate/enqueue/advance; reject before mutating history.
        {
            let catalog = self
                .app
                .world()
                .get_resource::<AgentActionCatalog>()
                .cloned()
                .unwrap_or_default();
            validate_action_against_catalog(&catalog, &action)
                .map_err(|error| anyhow!("InvalidAction: {error}"))?;
        }
        // Post-restore stepping policy: stepping after a `restore_tick` into
        // recorded future ticks on the same branch explicitly diverges, so the
        // stale future beyond the current tick is truncated first. Forking via
        // `branch` is the non-destructive alternative; truncation here is the
        // documented explicit-diverge behavior.
        self.enforce_diverge_truncation();
        let next_tick = self.current_tick() + 1;
        self.enqueue_action_at(next_tick, source, action);
        self.run_one_agent_tick();
        // Caller-side checkpoint bookkeeping (the snapshot crate owns
        // creation/pruning): mirror any new automatic checkpoints into the
        // replay log, honor `checkpoint_on_terminal`, and make sure the
        // response reports a snapshot created on this same tick.
        self.sync_auto_checkpoints();
        self.maybe_terminal_checkpoint()?;
        self.patch_snapshot_created();
        self.last_response()
    }

    /// Truncates recorded future actions/checkpoints/checksums on the current
    /// branch beyond the current tick (explicit diverge after restore).
    /// Records on other branches are preserved, including their
    /// branch-aware checksum expectations: only the current branch's future
    /// beyond `tick` is dropped via `log.truncate_branch_data` semantics.
    fn enforce_diverge_truncation(&mut self) {
        let (branch, tick) = match (
            self.app.world().get_resource::<AgentControlState>(),
            self.app.world().get_resource::<SimClock>(),
        ) {
            (Some(control), Some(clock)) => (control.branch_id, clock.tick),
            _ => return,
        };
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.log.truncate_future(branch, tick);
        }
        if let Some(mut timeline) = self.app.world_mut().get_resource_mut::<Timeline>() {
            timeline.truncate_future(branch, tick);
        }
    }

    /// Explicit CheckpointCreated handling: mirrors only the checkpoint
    /// created in the current episode (via `control.last_snapshot_created`,
    /// set by `create_snapshot` inside `AgentTick`) into the replay log.
    /// No scan-all-store behavior: stale cross-episode checkpoints never
    /// contaminate the fresh log. Tagged with (episode, branch, tick).
    ///
    /// Owner-side indexing: after `maybe_take_snapshot` creates an id, it is
    /// immediately indexed into the log and retention is enforced with live
    /// refs (the snapshot crate no longer prunes with replay visibility).
    fn sync_auto_checkpoints(&mut self) {
        // Replay-disabled retention: even without a recorder, snapshot-only
        // retention still applies (empty reference set).
        if !self.app.world().contains_resource::<ReplayRecorder>() {
            if self.app.world().contains_resource::<SnapshotStore>() {
                enforce_retention(self.app.world_mut(), &BTreeSet::new());
            }
            return;
        }
        let (branch, created, episode) = match (
            self.app.world().get_resource::<AgentControlState>(),
            self.app.world().get_resource::<ReplayRecorder>(),
        ) {
            (Some(control), Some(recorder)) => (
                control.branch_id,
                control.last_snapshot_created,
                recorder.log.manifest.episode_id,
            ),
            _ => return,
        };
        if !self.app.world().contains_resource::<SnapshotStore>() {
            return;
        }
        let Some(id) = created else {
            return;
        };
        let known = match self.app.world().get_resource::<ReplayRecorder>() {
            Some(recorder) => recorder
                .log
                .branch_checkpoints
                .iter()
                .map(|checkpoint| checkpoint.snapshot_id)
                .chain(recorder.log.checkpoints.values().copied())
                .collect::<BTreeSet<_>>(),
            None => return,
        };
        if known.contains(&id) {
            return;
        }
        let (tick, checksum) = match self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .and_then(|store| store.snapshots.get(&id))
        {
            Some(snapshot) => (
                snapshot.manifest.tick,
                queue_excluded_checksum(snapshot).unwrap_or_else(|_| snapshot.checksum.clone()),
            ),
            None => return,
        };
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            // Only install checkpoints matching the current episode; stale
            // ids from previous episodes are ignored even if
            // `last_snapshot_created` somehow points at them (defensive).
            let current_episode = recorder.log.manifest.episode_id;
            if episode != current_episode {
                return;
            }
            recorder
                .log
                .push_checkpoint_with_episode(branch, tick, id, current_episode);
            recorder.log.insert_branch_checksum(branch, tick, checksum);
        }
        // Owner-side retention: the snapshot creation path no longer prunes
        // with replay visibility, so enforce here with live refs immediately
        // after indexing.
        self.prune_with_live_refs();
    }

    /// Honors `SnapshotPolicy::checkpoint_on_terminal`: snapshots terminal
    /// steps into the replay log on the current branch.
    fn maybe_terminal_checkpoint(&mut self) -> Result<()> {
        let terminal = match self.app.world().get_resource::<LastStepResponse>() {
            Some(last) => last
                .0
                .as_ref()
                .is_some_and(|response| response.done || response.truncated),
            None => false,
        };
        if !terminal || !self.has_snapshot_support() {
            return Ok(());
        }
        let enabled = self
            .app
            .world()
            .get_resource::<SnapshotPolicy>()
            .map(|policy| policy.checkpoint_on_terminal)
            .unwrap_or(false);
        if !enabled {
            return Ok(());
        }
        let branch = match self.app.world().get_resource::<AgentControlState>() {
            Some(control) => control.branch_id,
            None => return Ok(()),
        };
        // Avoid double-snapshotting when the interval policy already captured
        // this tick.
        let tick = self.current_tick();
        let already = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .is_some_and(|recorder| {
                recorder
                    .log
                    .branch_checkpoints
                    .iter()
                    .any(|checkpoint| checkpoint.branch_id == branch && checkpoint.tick == tick)
            });
        if already {
            return Ok(());
        }
        let result =
            self.create_owned_checkpoint(Some(format!("terminal-{tick}")), SnapshotRole::Periodic)?;
        let checksum = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .and_then(|store| store.snapshots.get(&result.snapshot_id))
            .and_then(|snapshot| queue_excluded_checksum(snapshot).ok())
            .unwrap_or_else(|| result.checksum.clone());
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder
                .log
                .push_checkpoint(branch, result.tick, result.snapshot_id);
            recorder
                .log
                .insert_branch_checksum(branch, result.tick, checksum);
        }
        self.prune_with_live_refs();
        Ok(())
    }

    /// Ensures the latest observation response reports a snapshot created on
    /// the same tick: automatic checkpoints run after the observation system
    /// inside `AgentTick`, so the stored response is patched with the current
    /// `last_snapshot_created` marker when set.
    fn patch_snapshot_created(&mut self) {
        let snapshot = self
            .app
            .world()
            .get_resource::<AgentControlState>()
            .and_then(|control| control.last_snapshot_created);
        if let Some(snapshot) = snapshot
            && let Some(mut last) = self.app.world_mut().get_resource_mut::<LastStepResponse>()
            && let Some(response) = last.0.as_mut()
        {
            response.info.snapshot_created = Some(snapshot);
        }
    }

    /// External stepping (`step`/`step_many`/`fast_forward`) is rejected while
    /// the control mode is behavioral-only. Internal reconstruction
    /// (`restore`/`restore_tick`/`branch`) bypasses `step_with_source` and may
    /// still rebuild state. Returns before any action is enqueued so the tick is
    /// left unchanged.
    fn reject_paused_or_inspect_only(&self) -> Result<()> {
        let mode = self
            .app
            .world()
            .resource::<AgentControlState>()
            .mode
            .clone();
        match mode {
            ControlMode::Paused => Err(anyhow!(
                "cannot step while control mode is Paused; resume before stepping"
            )),
            ControlMode::InspectOnly => Err(anyhow!(
                "cannot step while control mode is InspectOnly; switch modes before stepping"
            )),
            _ => Ok(()),
        }
    }

    /// Replays ticks in `(checkpoint_tick, target_tick]` one frame at a time:
    /// only the current tick's recorded frame is enqueued per iteration (perf:
    /// no whole-interval upfront enqueue), preserving recorded source/order.
    /// Empty ticks still run. Each reconstructed tick's state checksum is
    /// verified queue-excluded against the expected checksum when present;
    /// mismatch fails (see [`queue_excluded_checksum`]: pending futures
    /// beyond the target never affect tick-target state).
    ///
    /// Reconstruction policy: both the core and replay `ExecutionContext`
    /// resources are set to `Reconstructing` (the core one is what
    /// `drain_agent_actions` gates on; the replay one keeps the
    /// `record_replay_step` system quiet), `AgentDecision` is skipped,
    /// automatic snapshots suppressed. No queue sentinel is used: the old
    /// `Custom` JSON marker was caller-forgeable and is removed.
    fn replay_tick_interval(
        &mut self,
        branch: BranchId,
        records: &[ActionRecord],
        checkpoint_tick: u64,
        target_tick: u64,
    ) -> Result<()> {
        // Work-budget enforcement.
        let interval = target_tick.saturating_sub(checkpoint_tick);
        if interval > MAX_RECONSTRUCTION_TICKS {
            return Err(anyhow!(
                "reconstruction interval {interval} exceeds budget {}",
                MAX_RECONSTRUCTION_TICKS
            ));
        }
        // Index once into tick-keyed map; per-iteration enqueue uses only the
        // current tick's frame.
        let mut actions_by_tick: BTreeMap<u64, Vec<(ActionSource, AgentAction)>> = BTreeMap::new();
        for record in records {
            if record.tick > checkpoint_tick && record.tick <= target_tick {
                actions_by_tick
                    .entry(record.tick)
                    .or_default()
                    .push((record.source.clone(), record.action.clone()));
            }
        }
        // Branch-aware expected checksums via `expected_checksum`: own
        // branch map first, legacy tick map as back-compat fallback. Never
        // consult other branches' maps (no cross-branch fallback).
        let checksums = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.log.clone())
            .map(|log| {
                let mut map = BTreeMap::new();
                let mut ticks = BTreeSet::new();
                ticks.extend(log.snapshot_checksums.keys().copied());
                for per_branch in log.branch_checksums.values() {
                    ticks.extend(per_branch.keys().copied());
                }
                for tick in ticks {
                    if let Some(expected) = expected_checksum_for_tick(&log, branch, tick) {
                        map.insert(tick, expected);
                    }
                }
                map
            })
            .unwrap_or_default();

        // Preserve post-target futures across the interval.
        let future_actions = self
            .app
            .world()
            .get_resource::<AgentActionQueue>()
            .map(|queue| {
                queue
                    .pending
                    .iter()
                    .filter(|scheduled| scheduled.tick > target_tick)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(mut queue) = self.app.world_mut().get_resource_mut::<AgentActionQueue>() {
            queue.clear();
        }

        let old_core_context = self
            .app
            .world()
            .get_resource::<CoreExecutionContext>()
            .copied()
            .unwrap_or(CoreExecutionContext::Live);
        let old_replay_context = self
            .app
            .world()
            .get_resource::<ReplayExecutionContext>()
            .copied()
            .unwrap_or(ReplayExecutionContext::Live);
        self.app
            .world_mut()
            .insert_resource(CoreExecutionContext::Reconstructing);
        self.app
            .world_mut()
            .insert_resource(ReplayExecutionContext::Reconstructing);
        let old_interval = self
            .app
            .world()
            .get_resource::<SnapshotPolicy>()
            .map(|policy| policy.checkpoint_every_ticks);
        if let Some(mut policy) = self.app.world_mut().get_resource_mut::<SnapshotPolicy>() {
            policy.checkpoint_every_ticks = 0;
        }

        let mut replay_result = Ok(());
        if checkpoint_tick >= target_tick {
            // Empty interval: nothing to replay.
        } else {
            #[allow(clippy::needless_range_loop)]
            for tick in (checkpoint_tick.saturating_add(1))..=target_tick {
                // Enqueue ONLY the current tick's recorded frame. The core
                // `ExecutionContext::Reconstructing` resource (not a queue
                // marker) makes `drain_agent_actions` accept all recorded
                // sources without mode arbitration.
                if let Some(frame) = actions_by_tick.get(&tick) {
                    for (source, action) in frame {
                        self.enqueue_action_at(tick, source.clone(), action.clone());
                    }
                }
                // Empty ticks: nothing enqueued, still runs.
                self.run_one_reconstructing_tick();
                if self.app.world().resource::<LastStepResponse>().0.is_none() {
                    replay_result = Err(anyhow!("replay tick produced no StepResponse"));
                    break;
                }
                // Queue-excluded verification against the branch-aware
                // expected checksum (legacy fallback). Pending futures beyond
                // the target must not affect tick-target state, so both sides
                // exclude the queue.
                if let Some(expected) = checksums.get(&tick) {
                    match capture_queue_excluded_checksum(self.app.world_mut()) {
                        Ok(actual) => {
                            if actual.hash != expected.hash || actual.tick != expected.tick {
                                replay_result = Err(anyhow!(
                                    "reconstructed tick {tick} checksum mismatch: expected {:?}, got {:?}",
                                    expected,
                                    actual
                                ));
                                break;
                            }
                        }
                        Err(error) => {
                            replay_result = Err(anyhow!(
                                "checksum computation failed at tick {tick}: {error}"
                            ));
                            break;
                        }
                    }
                }
            }
        }

        // Restore post-target futures.
        if replay_result.is_ok() {
            for scheduled in future_actions {
                self.enqueue_action_at(scheduled.tick, scheduled.source, scheduled.action);
            }
        }

        if let Some(mut policy) = self.app.world_mut().get_resource_mut::<SnapshotPolicy>()
            && let Some(old_interval) = old_interval
        {
            policy.checkpoint_every_ticks = old_interval;
        }
        self.app.world_mut().insert_resource(old_core_context);
        self.app.world_mut().insert_resource(old_replay_context);

        replay_result
    }

    /// Runs one simulation tick without the `AgentDecision` schedule, used for
    /// history reconstruction so policy execution is disabled during replay.
    /// `CurrentInputFrame` is installed via the queue while the core
    /// `ExecutionContext::Reconstructing` resource makes
    /// `drain_agent_actions` accept all recorded sources without
    /// arbitration, preserving source as metadata.
    fn run_one_reconstructing_tick(&mut self) {
        self.app.world_mut().run_schedule(AgentPreTick);
        self.app.world_mut().run_schedule(AgentTick);
        self.app.world_mut().run_schedule(AgentPostTick);
    }

    fn has_snapshot_support(&self) -> bool {
        self.app.world().contains_resource::<SnapshotStore>()
    }

    #[cfg(feature = "visual")]
    fn capture_primary_window_impl(
        &mut self,
        options: VisualCaptureOptions,
    ) -> Result<VisualCaptureResult> {
        use bevy::render::view::screenshot::{Screenshot, save_to_disk};
        use bevy::window::{PrimaryWindow, Window};

        let (tick, frame, width, height) = {
            let world = self.app.world_mut();
            let tick = world.resource::<SimClock>().tick;
            let frame = world.resource::<AgentControlState>().frame;
            let mut windows = world.query_filtered::<&Window, With<PrimaryWindow>>();
            let window = windows
                .iter(world)
                .next()
                .ok_or_else(|| anyhow!("visual capture requires a primary window"))?;
            (
                tick,
                frame,
                window.physical_width(),
                window.physical_height(),
            )
        };
        let path = visual_capture_path(&options, tick, frame)?;
        let timeout_frames = options.timeout_frames.max(1);

        self.app
            .world_mut()
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));

        for _ in 0..=timeout_frames {
            self.app.update();
            if file_is_nonempty(&path) {
                return Ok(VisualCaptureResult {
                    tick,
                    frame,
                    path,
                    width,
                    height,
                    format: "png".to_string(),
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }

        Err(anyhow!(
            "visual capture timed out after {timeout_frames} frames: {}",
            path.display()
        ))
    }
}

#[cfg(feature = "visual")]
fn file_is_nonempty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
}

impl AgentEnvironment for AgentApp {
    type Action = AgentAction;
    type Observation = Observation;

    fn reset(&mut self, options: ResetOptions) -> Result<Self::Observation> {
        self.ensure_started();
        {
            let world = self.app.world_mut();
            world.resource_mut::<ObservationConfig>().mode = options.observation_mode;
            if let Some(seed) = options.seed {
                world.insert_resource(DeterministicRng::seeded(seed));
            }
        }

        self.app.world_mut().run_schedule(AgentReset);
        self.reset_once = true;
        // A successful reset clears any fault: the world is re-initialized.
        self.app.world_mut().remove_resource::<FaultState>();
        reset_replay_and_timeline(self.app.world_mut());

        let mut response = self.last_response()?;
        {
            let control = self.app.world().resource::<AgentControlState>();
            response.info.timeline_id = control.timeline_id;
            response.info.branch_id = control.branch_id;
        }
        if options.create_initial_snapshot && self.has_snapshot_support() {
            let snapshot = self.create_owned_checkpoint(
                Some(format!("reset-{}", response.tick)),
                SnapshotRole::Initial,
            )?;
            let branch = self
                .app
                .world()
                .get_resource::<AgentControlState>()
                .map(|control| control.branch_id);
            let checksum = self
                .app
                .world()
                .get_resource::<SnapshotStore>()
                .and_then(|store| store.snapshots.get(&snapshot.snapshot_id))
                .and_then(|stored| queue_excluded_checksum(stored).ok())
                .unwrap_or_else(|| snapshot.checksum.clone());
            if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
                recorder.log.initial_snapshot = Some(snapshot.snapshot_id);
                recorder.log.initial_tick = snapshot.tick.min(response.tick);
                if let Some(branch) = branch {
                    recorder
                        .log
                        .push_checkpoint(branch, snapshot.tick, snapshot.snapshot_id);
                    recorder
                        .log
                        .insert_branch_checksum(branch, snapshot.tick, checksum);
                } else {
                    recorder
                        .log
                        .checkpoints
                        .insert(snapshot.tick, snapshot.snapshot_id);
                    recorder
                        .log
                        .snapshot_checksums
                        .insert(snapshot.tick, checksum);
                }
            }
            response.info.snapshot_created = Some(snapshot.snapshot_id);
        }
        self.app.world_mut().resource_mut::<LastStepResponse>().0 = Some(response.clone());

        Ok(response.observation)
    }

    fn step(&mut self, action: Self::Action) -> Result<StepResponse<Self::Observation>> {
        self.step_with_source(action, ActionSource::Agent)
    }

    fn step_many(
        &mut self,
        actions: Vec<Self::Action>,
    ) -> Result<Vec<StepResponse<Self::Observation>>> {
        let mut responses = Vec::new();
        for action in actions {
            let response = self.step(action)?;
            let done = response.done || response.truncated;
            responses.push(response);
            if done {
                break;
            }
        }
        Ok(responses)
    }

    fn observe(&mut self, mode: ObservationMode) -> Result<Self::Observation> {
        self.ensure_started();
        self.ensure_reset()?;
        self.app
            .world_mut()
            .resource_mut::<ObservationConfig>()
            .mode = mode;
        collect_observation(self.app.world_mut());
        Ok(self.last_response()?.observation)
    }

    fn snapshot(&mut self) -> Result<SnapshotCreateResult> {
        self.ensure_started();
        self.ensure_reset()?;
        if !self.has_snapshot_support() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }

        let tick = self.current_tick();
        let result =
            self.create_owned_checkpoint(Some(format!("manual-{tick}")), SnapshotRole::Manual)?;
        // Tag with the current branch to preserve same-tick isolation;
        // `push_checkpoint` also keeps the legacy tick map consistent.
        let branch = self
            .app
            .world()
            .get_resource::<AgentControlState>()
            .map(|control| control.branch_id)
            .or_else(|| {
                self.app
                    .world()
                    .get_resource::<Timeline>()
                    .map(|timeline| timeline.current_branch)
            });
        let checksum = self
            .app
            .world()
            .get_resource::<SnapshotStore>()
            .and_then(|store| store.snapshots.get(&result.snapshot_id))
            .and_then(|snapshot| queue_excluded_checksum(snapshot).ok())
            .unwrap_or_else(|| result.checksum.clone());
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            if let Some(branch) = branch {
                recorder
                    .log
                    .push_checkpoint(branch, result.tick, result.snapshot_id);
                recorder
                    .log
                    .insert_branch_checksum(branch, result.tick, checksum);
            } else {
                recorder
                    .log
                    .checkpoints
                    .insert(result.tick, result.snapshot_id);
                recorder
                    .log
                    .snapshot_checksums
                    .insert(result.tick, checksum);
            }
        }
        Ok(result)
    }

    fn restore(&mut self, snapshot: SnapshotId) -> Result<()> {
        self.ensure_started();
        if !self.has_snapshot_support() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }

        restore_snapshot(self.app.world_mut(), snapshot)?;
        {
            // The snapshot restores the simulation clock but not the control
            // frame or snapshot bookkeeping. Realign the frame to the restored
            // tick and clear any stale snapshot marker so the next response does
            // not falsely report a freshly created snapshot.
            let world = self.app.world_mut();
            let restored_tick = world.resource::<SimClock>().tick;
            let mut control = world.resource_mut::<AgentControlState>();
            control.frame = restored_tick;
            control.last_snapshot_created = None;
        }
        self.reset_once = true;
        collect_observation(self.app.world_mut());
        Ok(())
    }

    fn restore_tick(&mut self, tick: u64) -> Result<()> {
        self.ensure_started();
        self.ensure_no_fault()?;
        self.ensure_reset()?;

        let (log, timeline, branch) = {
            let log = self
                .app
                .world()
                .get_resource::<ReplayRecorder>()
                .map(|recorder| recorder.log.clone())
                .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
            let timeline = self
                .app
                .world()
                .get_resource::<Timeline>()
                .cloned()
                .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
            let branch = self
                .app
                .world()
                .get_resource::<AgentControlState>()
                .map(|control| control.branch_id)
                .unwrap_or(timeline.current_branch);
            (log, timeline, branch)
        };

        // Lower-bound + range + lineage validation BEFORE any mutation: the
        // checkpoint snapshot restore below must only happen after the target
        // is known to be inside `initial_tick..=end_tick` and visible on this
        // branch's lineage.
        validate_history_target(&log, &timeline, branch, tick)?;
        if !self.has_snapshot_support() {
            return Err(anyhow!("AgentSnapshotPlugin is not installed"));
        }

        // Branch-aware checkpoint selection: nearest checkpoint at-or-before
        // the tick on this branch's lineage. A child never selects a parent
        // checkpoint recorded beyond its fork tick.
        let (checkpoint_tick, snapshot_id) = log
            .nearest_checkpoint_for_branch(&timeline, branch, tick)
            .ok_or_else(|| anyhow!("no checkpoint exists at or before tick {tick}"))?;
        // Work-budget enforcement: cap the replay interval.
        let interval = tick.saturating_sub(checkpoint_tick);
        if interval > MAX_RECONSTRUCTION_TICKS {
            return Err(anyhow!(
                "restore interval {interval} exceeds budget {}",
                MAX_RECONSTRUCTION_TICKS
            ));
        }

        // Failure atomicity: capture backups before mutating so a checksum
        // (or any replay) error can roll history navigation back instead of
        // leaving a half-replayed world. Back up the world snapshot alongside
        // control state, last response, and the reset flag.
        let backup = capture_snapshot(self.app.world_mut(), None)?;
        let control_backup = self
            .app
            .world()
            .get_resource::<AgentControlState>()
            .cloned();
        let last_backup = self.app.world().get_resource::<LastStepResponse>().cloned();
        let reset_once_backup = self.reset_once;

        // Preserve actions scheduled beyond the restore target BEFORE the
        // checkpoint restore: `restore_snapshot` overwrites the queue with the
        // checkpoint's captured queue, which may predate caller-enqueued future
        // actions (e.g. same-tick checkpoint replacement can select an older
        // snapshot). `replay_tick_interval` preserves post-restore futures;
        // these pre-restore futures are re-applied after replay below.
        let pending_future_before = self
            .app
            .world()
            .get_resource::<AgentActionQueue>()
            .map(|queue| {
                queue
                    .pending
                    .iter()
                    .filter(|scheduled| scheduled.tick > tick)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Start at the selected checkpoint.
        self.restore(snapshot_id)?;

        // Replay must never be recorded into the log. Disable recording for the
        // duration of the replay and restore the previous flag on both the
        // success and error paths.
        let old_recording = self
            .app
            .world()
            .get_resource::<ReplayRecorder>()
            .map(|recorder| recorder.recording)
            .unwrap_or(false);
        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.recording = false;
        }

        // Only lineage-visible actions are replayed; sibling-branch actions
        // are excluded by `actions_for_branch`.
        let records = log.actions_for_branch(&timeline, branch, checkpoint_tick, tick);
        let replay_result = self.replay_tick_interval(branch, &records, checkpoint_tick, tick);

        // Re-apply pre-restore futures lost by the checkpoint restore
        // (`PreserveCurrentFuture` policy shared with `branch` interactive
        // rewind; see `apply_pending_policy`).
        if !pending_future_before.is_empty()
            && let Some(mut queue) = self.app.world_mut().get_resource_mut::<AgentActionQueue>()
        {
            apply_pending_policy(
                &mut queue,
                pending_future_before,
                Vec::new(),
                PendingInputPolicy::PreserveCurrentFuture,
            );
        }

        if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
            recorder.recording = old_recording;
        }
        if let Err(error) = replay_result {
            // Atomic navigation: roll back world snapshot plus control
            // state, last response, and reset flag.
            let original = format!("{error:?}");
            let mut rollback_error: Option<String> = None;
            if let Err(rollback_err) = restore_snapshot_value(self.app.world_mut(), &backup) {
                rollback_error = Some(format!("{rollback_err:?}"));
            }
            if let Some(control) = control_backup.clone()
                && let Some(mut live) = self.app.world_mut().get_resource_mut::<AgentControlState>()
            {
                *live = control;
            }
            if let Some(last) = last_backup.clone()
                && let Some(mut live) = self.app.world_mut().get_resource_mut::<LastStepResponse>()
            {
                *live = last;
            }
            self.reset_once = reset_once_backup;
            if let Some(rollback) = rollback_error {
                // Rollback itself failed: the world may hold a partially
                // applied snapshot, so install `FaultState` carrying both
                // errors and fail closed until an explicit reset.
                let message = format!(
                    "restore_tick to {tick} failed ({original}) and rollback failed ({rollback})"
                );
                self.app.world_mut().insert_resource(FaultState {
                    message: message.clone(),
                });
                return Err(anyhow!("{message}; world may be faulted"));
            }
            return Err(anyhow!(
                "restore_tick to {tick} failed; rolled back: {original}"
            ));
        }

        Ok(())
    }

    fn branch(
        &mut self,
        from_tick: u64,
        label: Option<String>,
    ) -> Result<bevy_agent_core::BranchId> {
        self.ensure_started();
        self.ensure_no_fault()?;
        // Recorded-range + lower-bound enforcement mirrors `restore_tick`
        // (branch replays the fork point before forking). Validated BEFORE
        // any mutation.
        {
            let (log, timeline, branch) = (
                self.app
                    .world()
                    .get_resource::<ReplayRecorder>()
                    .map(|recorder| recorder.log.clone()),
                self.app.world().get_resource::<Timeline>().cloned(),
                self.app
                    .world()
                    .get_resource::<AgentControlState>()
                    .map(|control| control.branch_id),
            );
            if let (Some(log), Some(timeline), Some(branch)) = (log, timeline, branch) {
                validate_history_target(&log, &timeline, branch, from_tick)?;
            } else if let Some(end) = self
                .app
                .world()
                .get_resource::<ReplayRecorder>()
                .map(|recorder| global_end_tick(&recorder.log))
                && from_tick > end
            {
                return Err(anyhow!(
                    "branch target {from_tick} is beyond recorded end {end}"
                ));
            }
        }
        self.restore_tick(from_tick)?;
        // Backward-fork parent resolution: the new branch must hang off the
        // ancestor owning the `from_tick` interval (deepest ancestor whose
        // fork interval contains `from_tick`), not necessarily the current
        // branch. Forking off the current branch for an older tick would
        // produce `child fork < parent fork`, which bundle import rejects.
        let fork_parent = {
            let (timeline, current) = (
                self.app
                    .world()
                    .get_resource::<Timeline>()
                    .cloned()
                    .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?,
                self.app
                    .world()
                    .get_resource::<AgentControlState>()
                    .map(|control| control.branch_id)
                    .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?,
            );
            resolve_fork_parent_branch(&timeline, current, from_tick)?
        };
        // `checkpoint_on_branch` (default true) snapshots the fork point so the
        // child starts from a restorable checkpoint on its own branch.
        let checkpoint_on_branch = self
            .app
            .world()
            .get_resource::<SnapshotPolicy>()
            .map(|policy| policy.checkpoint_on_branch)
            .unwrap_or(true);
        let snapshot = if self.has_snapshot_support() && checkpoint_on_branch {
            let tick = self.current_tick();
            let result = self.create_owned_checkpoint(
                Some(format!("branch-{tick}")),
                SnapshotRole::BranchFork,
            )?;
            Some(result)
        } else {
            None
        };

        let branch_id = {
            let mut timeline = self
                .app
                .world_mut()
                .get_resource_mut::<Timeline>()
                .ok_or_else(|| anyhow!("AgentReplayPlugin is not installed"))?;
            // Reparent to the interval owner before forking:
            // `create_branch` parents off `current_branch`.
            timeline.current_branch = fork_parent;
            timeline.create_branch(
                from_tick,
                snapshot.as_ref().map(|result| result.snapshot_id),
                label,
            )
        };

        // Tag the fork checkpoint on the child branch (same-tick isolated from
        // the parent's own checkpoints) so child restores resolve locally.
        if let Some(result) = snapshot {
            let (tick, id) = (result.tick, result.snapshot_id);
            let checksum = self
                .app
                .world()
                .get_resource::<SnapshotStore>()
                .and_then(|store| store.snapshots.get(&id))
                .and_then(|stored| queue_excluded_checksum(stored).ok())
                .unwrap_or_else(|| result.checksum.clone());
            if let Some(mut recorder) = self.app.world_mut().get_resource_mut::<ReplayRecorder>() {
                recorder.log.push_checkpoint(branch_id, tick, id);
                recorder
                    .log
                    .insert_branch_checksum(branch_id, tick, checksum);
            }
            self.prune_with_live_refs();
        }

        let timeline_id = self.app.world().resource::<Timeline>().timeline_id;
        let mut control = self.app.world_mut().resource_mut::<AgentControlState>();
        control.timeline_id = timeline_id;
        control.branch_id = branch_id;
        control.mode = ControlMode::Agent;
        Ok(branch_id)
    }
}

pub fn set_episode_done(world: &mut World, reason: impl Into<String>) {
    let mut episode = world.resource_mut::<EpisodeState>();
    episode.done = true;
    episode.reason = Some(reason.into());
}

pub fn clear_episode(world: &mut World) {
    *world.resource_mut::<EpisodeState>() = EpisodeState::default();
    *world.resource_mut::<RewardState>() = RewardState::default();
    world.resource_mut::<CurrentInputFrame>().actions.clear();
}

/// Marks an episode boundary by starting a fresh replay log and timeline root.
///
/// Called from `AgentApp::reset` after the `AgentReset` schedule and before the
/// optional initial snapshot. It replaces the `ReplayLog` with a fresh one while
/// preserving the `ReplayRecorder.recording` flag, installs a fresh `Timeline`
/// root, and synchronizes `AgentControlState.timeline_id`/`branch_id` to that
/// new root. It is a no-op for apps that do not install the replay resources, so
/// core-only and snapshot-less configurations remain supported.
fn reset_replay_and_timeline(world: &mut World) {
    let environment = world
        .get_resource::<EnvironmentMetadata>()
        .cloned()
        .unwrap_or_default();
    let new_root = world.get_resource_mut::<Timeline>().map(|mut timeline| {
        *timeline = Timeline::default();
        (timeline.timeline_id, timeline.current_branch)
    });

    if let Some(mut recorder) = world.get_resource_mut::<ReplayRecorder>() {
        let recording = recorder.recording;
        // New episode id: increment from the previous log so checkpoints can
        // be tagged with (episode, branch, tick) and cross-episode sync is
        // rejected.
        let next_episode = recorder.log.manifest.episode_id.wrapping_add(1);
        recorder.log = ReplayLog::default();
        recorder.log.manifest.game_id = environment.name;
        recorder.log.manifest.game_version = environment.version;
        recorder.log.manifest.episode_id = next_episode;
        recorder.recording = recording;
        // Fresh episodes start live at the modern shape (`ensure_modern` /
        // `new_live` equivalent): install topology, active branch, and
        // cursor/end bounds from the new root so the log is never left
        // topology-less with stale bounds.
        let timeline_snapshot = world.get_resource::<Timeline>().cloned();
        if let Some(timeline) = timeline_snapshot {
            let log = &mut world.resource_mut::<ReplayRecorder>().log;
            ensure_modern_log(log, &timeline, 0);
        }
    }

    if let Some((timeline_id, branch_id)) = new_root
        && let Some(mut control) = world.get_resource_mut::<AgentControlState>()
    {
        control.timeline_id = timeline_id;
        control.branch_id = branch_id;
    }

    // A fresh episode is live by definition; never leak a Reconstructing
    // context (core gates `drain_agent_actions`, replay gates recording).
    if world.get_resource::<ReplayExecutionContext>().is_some() {
        world.insert_resource(ReplayExecutionContext::Live);
    }
    if world.get_resource::<CoreExecutionContext>().is_some() {
        world.insert_resource(CoreExecutionContext::Live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_agent_core::{AgentControlPlugin, AgentDecision};

    fn core_only_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugin::deterministic());
        app
    }

    fn grouped_agent_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(
            AgentControlPlugins::deterministic().with_snapshot_policy(SnapshotPolicy {
                checkpoint_every_ticks: 7,
                ..Default::default()
            }),
        );
        app
    }

    fn full_history_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugins::deterministic());
        app
    }

    fn frequent_checkpoint_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(
            AgentControlPlugins::deterministic().with_snapshot_policy(SnapshotPolicy {
                checkpoint_every_ticks: 2,
                ..Default::default()
            }),
        );
        app
    }

    #[derive(Resource, Default)]
    struct PolicyCalls(u64);

    fn counting_policy(
        mut calls: ResMut<PolicyCalls>,
        clock: Res<SimClock>,
        mut queue: ResMut<AgentActionQueue>,
    ) {
        calls.0 += 1;
        queue.schedule(clock.tick + 1, ActionSource::Script, AgentAction::Jump);
    }

    fn policy_driven_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AgentControlPlugins::deterministic())
            .init_resource::<PolicyCalls>()
            .add_systems(AgentDecision, counting_policy);
        app
    }

    fn record_len(env: &AgentApp) -> usize {
        env.replay_log().map(|log| log.records.len()).unwrap_or(0)
    }

    #[test]
    fn reset_without_snapshot_plugin_returns_observation() {
        let mut env = AgentApp::new(core_only_app);

        let observation = env
            .reset(ResetOptions {
                create_initial_snapshot: false,
                ..Default::default()
            })
            .unwrap();

        assert_eq!(env.current_tick(), 0);
        assert!(matches!(observation, Observation::Hybrid { .. }));
    }

    #[test]
    fn step_auto_resets_before_first_tick() {
        let mut env = AgentApp::new(core_only_app);

        let response = env.step(AgentAction::Noop).unwrap();

        assert_eq!(response.tick, 1);
        assert_eq!(response.info.actions_applied, 1);
    }

    #[test]
    fn plugin_group_installs_core_snapshot_replay_and_policy() {
        let mut env = AgentApp::new(grouped_agent_app);
        env.reset(ResetOptions::default()).unwrap();

        assert!(env.world().contains_resource::<SnapshotStore>());
        assert!(env.world().contains_resource::<ReplayRecorder>());
        assert_eq!(
            env.world()
                .resource::<SnapshotPolicy>()
                .checkpoint_every_ticks,
            7
        );
    }

    #[test]
    fn fast_forward_zero_returns_error() {
        let mut env = AgentApp::new(core_only_app);

        let error = env.fast_forward(0).unwrap_err();

        assert!(error.to_string().contains("zero ticks"));
    }

    #[test]
    fn capture_label_is_filesystem_safe() {
        assert_eq!(sanitized_capture_label(Some("After Jump!")), "after-jump");
        assert_eq!(sanitized_capture_label(Some("../bad/name")), "badname");
        assert_eq!(sanitized_capture_label(Some("   ")), "capture");
    }

    #[test]
    fn visual_capture_path_is_unique() {
        let mut output_dir = std::env::temp_dir();
        output_dir.push(format!(
            "bevy-agent-runner-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let options = VisualCaptureOptions {
            output_dir: output_dir.clone(),
            label: Some("Test Capture".to_string()),
            timeout_frames: 1,
            source: CaptureSource::Auto,
        };

        let first = visual_capture_path(&options, 3, 4).unwrap();
        std::fs::write(&first, b"exists").unwrap();
        let second = visual_capture_path(&options, 3, 4).unwrap();

        assert_eq!(
            first.file_name().unwrap().to_str().unwrap(),
            "tick-000003-frame-000004-test-capture.png"
        );
        assert_eq!(
            second.file_name().unwrap().to_str().unwrap(),
            "tick-000003-frame-000004-test-capture-1.png"
        );
        let _ = std::fs::remove_dir_all(output_dir);
    }

    #[test]
    fn snapshot_without_snapshot_plugin_returns_error() {
        let mut env = AgentApp::new(core_only_app);

        let error = env.snapshot().unwrap_err();

        assert!(error.to_string().contains("AgentSnapshotPlugin"));
    }

    #[test]
    fn episode_helpers_update_world_resources() {
        let mut env = AgentApp::new(core_only_app);
        env.reset(ResetOptions {
            create_initial_snapshot: false,
            ..Default::default()
        })
        .unwrap();

        set_episode_done(env.world_mut(), "done");
        assert!(env.world().resource::<EpisodeState>().done);
        assert_eq!(
            env.world().resource::<EpisodeState>().reason.as_deref(),
            Some("done")
        );

        env.world_mut()
            .resource_mut::<CurrentInputFrame>()
            .actions
            .push(AgentAction::Jump);
        clear_episode(env.world_mut());
        assert!(!env.world().resource::<EpisodeState>().done);
        assert!(
            env.world()
                .resource::<CurrentInputFrame>()
                .actions
                .is_empty()
        );
    }

    #[test]
    fn parent_child_same_tick_checkpoints_are_isolated() {
        let mut env = AgentApp::new(full_history_app);
        env.reset(ResetOptions::default()).unwrap();
        for _ in 0..3 {
            env.step(AgentAction::Noop).unwrap();
        }
        let parent = env.world().resource::<AgentControlState>().branch_id;
        let parent_snapshot = env.snapshot().unwrap().snapshot_id;

        let child = env.branch(3, Some("alt".to_string())).unwrap();
        assert_ne!(child, parent);

        let log = env.replay_log().unwrap().clone();
        let parent_entries = log
            .branch_checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.tick == 3 && checkpoint.branch_id == parent)
            .collect::<Vec<_>>();
        let child_entries = log
            .branch_checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.tick == 3 && checkpoint.branch_id == child)
            .collect::<Vec<_>>();
        assert!(
            parent_entries
                .iter()
                .any(|c| c.snapshot_id == parent_snapshot)
        );
        assert_eq!(child_entries.len(), 1);
        assert_ne!(child_entries[0].snapshot_id, parent_snapshot);

        // The child restores from its own fork checkpoint.
        env.restore_tick(3).unwrap();
        assert_eq!(env.current_tick(), 3);
        assert_eq!(env.world().resource::<AgentControlState>().branch_id, child);
    }

    #[test]
    fn restore_then_diverge_truncates_recorded_future() {
        let mut env = AgentApp::new(full_history_app);
        env.reset(ResetOptions::default()).unwrap();
        for _ in 0..5 {
            env.step(AgentAction::Noop).unwrap();
        }
        assert_eq!(env.current_tick(), 5);
        assert_eq!(record_len(&env), 5);

        env.restore_tick(2).unwrap();
        assert_eq!(env.current_tick(), 2);
        // Restore alone preserves the recorded future.
        assert_eq!(record_len(&env), 5);

        // Stepping into the recorded future on the same branch diverges and
        // truncates everything beyond the restore point before appending.
        let response = env.step(AgentAction::Jump).unwrap();
        assert_eq!(response.tick, 3);
        let log = env.replay_log().unwrap().clone();
        assert_eq!(log.records.len(), 3);
        assert!(log.records.iter().all(|record| record.tick <= 3));
        assert_eq!(log.records.last().unwrap().action, AgentAction::Jump);
    }

    #[test]
    fn replay_runs_no_policy_and_appends_no_records() {
        let mut env = AgentApp::new(policy_driven_app);
        env.reset(ResetOptions::default()).unwrap();
        for _ in 0..3 {
            env.step(AgentAction::Noop).unwrap();
        }
        let calls_after_live = env.world().resource::<PolicyCalls>().0;
        assert!(calls_after_live >= 3);
        let records_after_live = record_len(&env);
        assert!(records_after_live >= 3);

        // Replay must not execute the policy again and must not append records.
        env.restore_tick(1).unwrap();
        assert_eq!(env.current_tick(), 1);
        assert_eq!(env.world().resource::<PolicyCalls>().0, calls_after_live);
        assert_eq!(record_len(&env), records_after_live);
        assert_eq!(
            *env.world().resource::<CoreExecutionContext>(),
            CoreExecutionContext::Live
        );
        assert_eq!(
            *env.world().resource::<ReplayExecutionContext>(),
            ReplayExecutionContext::Live
        );
    }

    #[test]
    fn reconstruction_creates_no_snapshots() {
        let mut env = AgentApp::new(frequent_checkpoint_app);
        env.reset(ResetOptions::default()).unwrap();
        for _ in 0..5 {
            env.step(AgentAction::Noop).unwrap();
        }
        // Interval checkpoints at ticks 2 and 4 are mirrored into the log.
        let log = env.replay_log().unwrap().clone();
        assert!(log.branch_checkpoints.iter().any(|c| c.tick == 2));
        assert!(log.branch_checkpoints.iter().any(|c| c.tick == 4));
        let snapshots_before = env.world().resource::<SnapshotStore>().snapshots.len();
        let checkpoints_before = env.world().resource::<SnapshotStore>().checkpoints.len();
        let records_before = log.records.len();

        // Replaying across the tick-4 interval checkpoint must not create
        // additional snapshots (tick 4 is a multiple of the interval).
        env.restore_tick(4).unwrap();

        assert_eq!(env.current_tick(), 4);
        assert_eq!(
            env.world().resource::<SnapshotStore>().snapshots.len(),
            snapshots_before
        );
        assert_eq!(
            env.world().resource::<SnapshotStore>().checkpoints.len(),
            checkpoints_before
        );
        assert_eq!(record_len(&env), records_before);
        assert_eq!(
            *env.world().resource::<CoreExecutionContext>(),
            CoreExecutionContext::Live
        );
        assert_eq!(
            *env.world().resource::<ReplayExecutionContext>(),
            ReplayExecutionContext::Live
        );
    }

    #[test]
    fn step_response_reports_same_tick_snapshot() {
        let mut env = AgentApp::new(frequent_checkpoint_app);
        env.reset(ResetOptions::default()).unwrap();

        let response = env.step(AgentAction::Noop).unwrap();

        // Tick 2 hits the interval policy; the response must report the
        // snapshot created on that same tick, and the log must carry it.
        let second = env.step(AgentAction::Noop).unwrap();
        assert_eq!(second.tick, 2);
        assert!(second.info.snapshot_created.is_some());
        assert!(
            env.replay_log()
                .unwrap()
                .checkpoints
                .contains_key(&second.tick)
        );
        let _ = response;
    }

    #[test]
    fn terminal_step_records_checkpoint_on_branch() {
        let mut env = AgentApp::new(full_history_app);
        env.reset(ResetOptions::default()).unwrap();
        set_episode_done(env.world_mut(), "done");

        let response = env.step(AgentAction::Noop).unwrap();

        assert!(response.done);
        assert!(response.info.snapshot_created.is_some());
        let branch = env.world().resource::<AgentControlState>().branch_id;
        assert!(
            env.replay_log()
                .unwrap()
                .branch_checkpoints
                .iter()
                .any(
                    |checkpoint| checkpoint.branch_id == branch && checkpoint.tick == response.tick
                )
        );
    }

    #[test]
    fn parent_future_exclusion_restore_child_tick2_is_only_noop() {
        // Parent tick2 MoveRight (fork tick1) must not leak into the child:
        // restoring child tick2 yields only the child's Noop.
        let mut env = AgentApp::new(full_history_app);
        env.reset(ResetOptions::default()).unwrap();
        env.step(AgentAction::Noop).unwrap(); // tick1 parent
        let move_right = AgentAction::Move { x: 1.0, y: 0.0 };
        env.step(move_right.clone()).unwrap(); // tick2 parent
        let child = env.branch(1, Some("child".to_string())).unwrap();
        env.step(AgentAction::Noop).unwrap(); // tick2 child
        assert_eq!(env.current_tick(), 2);
        // Restore child tick2 via fork-bounded intervals.
        env.restore_tick(2).unwrap();
        assert_eq!(env.current_tick(), 2);
        assert_eq!(env.world().resource::<AgentControlState>().branch_id, child);
        let input = env.world().resource::<CurrentInputFrame>().clone();
        assert_eq!(input.tick, 2);
        assert_eq!(input.actions, vec![AgentAction::Noop]);
        // Log-level check: child sees shared tick1 + own tick2 only.
        let (log, timeline) = (
            env.replay_log().unwrap().clone(),
            env.world().resource::<Timeline>().clone(),
        );
        let visible = log.actions_for_branch(&timeline, child, 0, 2);
        let tick2: Vec<_> = visible.iter().filter(|r| r.tick == 2).collect();
        assert_eq!(tick2.len(), 1);
        assert_eq!(tick2[0].action, AgentAction::Noop);
    }

    #[test]
    fn paused_and_replay_mode_reconstruction_preserves_inputs() {
        for mode in [ControlMode::Paused, ControlMode::Replay] {
            let mut env = AgentApp::new(full_history_app);
            env.reset(ResetOptions::default()).unwrap();
            for _ in 0..3 {
                env.step(AgentAction::Noop).unwrap();
            }
            // Enter behavioral-only mode after recording; reconstruction
            // bypasses `step_with_source` and must still preserve inputs via
            // the Reconstructing sentinel in `drain_agent_actions`.
            env.world_mut().resource_mut::<AgentControlState>().mode = mode.clone();
            env.restore_tick(2).unwrap();
            assert_eq!(env.current_tick(), 2);
            let input = env.world().resource::<CurrentInputFrame>().clone();
            assert_eq!(input.tick, 2);
            assert_eq!(input.actions, vec![AgentAction::Noop]);
            assert_eq!(input.sources, vec![ActionSource::Agent]);
        }
    }

    #[test]
    fn import_preserves_three_level_topology_and_active_branch() {
        let mut source = AgentApp::new(full_history_app);
        source.reset(ResetOptions::default()).unwrap();
        for _ in 0..2 {
            source.step(AgentAction::Noop).unwrap();
        }
        let mid = source.branch(2, Some("mid".to_string())).unwrap();
        source.step(AgentAction::Jump).unwrap(); // tick3 on mid
        let leaf = source.branch(3, Some("leaf".to_string())).unwrap();
        source.step(AgentAction::Interact).unwrap(); // tick4 on leaf
        let bundle = source.export_replay_bundle().unwrap();
        assert_eq!(bundle.log.timeline_topology.len(), 3);
        assert_eq!(bundle.log.active_branch, Some(leaf));

        let mut fresh = AgentApp::new(full_history_app);
        fresh.load_replay_bundle(bundle).unwrap();
        let timeline = fresh.app.world().resource::<Timeline>().clone();
        let control = fresh.app.world().resource::<AgentControlState>().clone();
        assert_eq!(timeline.branches.len(), 3);
        assert_eq!(timeline.current_branch, leaf);
        assert_eq!(control.branch_id, leaf);
        // Parent links verbatim: leaf -> mid -> root.
        let leaf_branch = timeline.branches.get(&leaf).unwrap();
        assert_eq!(leaf_branch.parent_branch, Some(mid));
        let mid_branch = timeline.branches.get(&mid).unwrap();
        assert!(mid_branch.parent_branch.is_some());
        assert_ne!(mid_branch.parent_branch, Some(leaf));
        // Restorable on the leaf.
        fresh.restore_tick(4).unwrap();
        assert_eq!(fresh.current_tick(), 4);
    }

    #[test]
    fn load_bundle_syncs_timeline_and_restores_history() {
        let mut source = AgentApp::new(full_history_app);
        source.reset(ResetOptions::default()).unwrap();
        for _ in 0..3 {
            source.step(AgentAction::Noop).unwrap();
        }
        let manual = source.snapshot().unwrap().snapshot_id;
        let bundle = source.export_replay_bundle().unwrap();

        let mut fresh = AgentApp::new(full_history_app);
        // Recording flag is preserved across import.
        fresh
            .app_mut()
            .world_mut()
            .resource_mut::<ReplayRecorder>()
            .recording = false;
        fresh.load_replay_bundle(bundle).unwrap();

        assert!(!fresh.app.world().resource::<ReplayRecorder>().recording);
        // Control/timeline initialized from the imported log.
        let timeline = fresh.app.world().resource::<Timeline>().clone();
        let control = fresh.app.world().resource::<AgentControlState>().clone();
        assert_eq!(control.branch_id, timeline.current_branch);
        assert_eq!(control.timeline_id, timeline.timeline_id);
        assert!(
            fresh
                .app
                .world()
                .resource::<SnapshotStore>()
                .checkpoints
                .len()
                >= 2
        );
        assert!(fresh.has_reset());
        // Imported history is restorable.
        fresh.restore_tick(2).unwrap();
        assert_eq!(fresh.current_tick(), 2);
        // The manual snapshot survived the round trip.
        assert!(
            fresh
                .app
                .world()
                .resource::<SnapshotStore>()
                .snapshots
                .contains_key(&manual)
        );
    }
}
