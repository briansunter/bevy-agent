use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use bevy_agent_core::{ActionSource, AgentAction, BranchId, SnapshotChecksum, SnapshotId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::TimelineBranch;

/// Current replay wire contract, paired with explicit snapshot type identities.
pub const REPLAY_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayManifest {
    pub schema_version: u32,
    pub replay_id: Uuid,
    pub game_id: String,
    pub game_version: String,
    /// Episode that owns this recording and its checkpoints.
    pub episode_id: u64,
}

impl Default for ReplayManifest {
    fn default() -> Self {
        Self {
            schema_version: REPLAY_SCHEMA_VERSION,
            replay_id: Uuid::new_v4(),
            game_id: "unknown-game".to_string(),
            game_version: env!("CARGO_PKG_VERSION").to_string(),
            episode_id: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionRecord {
    pub tick: u64,
    pub branch_id: BranchId,
    pub source: ActionSource,
    pub action: AgentAction,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BranchCheckpoint {
    pub tick: u64,
    pub branch_id: BranchId,
    pub snapshot_id: SnapshotId,
    pub episode: u64,
}

/// Portable recording data. Every history record belongs to one explicit
/// branch; timeline topology defines ancestor visibility at fork boundaries.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayLog {
    pub manifest: ReplayManifest,
    pub initial_snapshot: Option<SnapshotId>,
    /// Executed ticks, including frames without actions, keyed by branch UUID.
    pub completed_ticks: BTreeMap<BranchId, BTreeSet<u64>>,
    pub records: Vec<ActionRecord>,
    /// Expected snapshot checksums keyed by branch identity, then tick.
    pub branch_checksums: BTreeMap<BranchId, BTreeMap<u64, SnapshotChecksum>>,
    pub branch_checkpoints: Vec<BranchCheckpoint>,
    pub timeline_topology: Vec<TimelineBranch>,
    pub active_branch: Option<BranchId>,
    pub cursor_tick: u64,
    pub initial_tick: u64,
    pub end_tick: u64,
}

#[derive(Resource, Clone, Debug, Serialize)]
pub struct ReplayRecorder {
    pub(crate) recording: bool,
    pub(crate) log: ReplayLog,
    #[serde(skip)]
    pub(crate) record_index: BTreeMap<BranchId, BTreeMap<u64, Vec<usize>>>,
    #[serde(skip)]
    pub(crate) max_history_bytes: usize,
    #[serde(skip)]
    pub(crate) retained_bytes: usize,
}

impl Default for ReplayRecorder {
    fn default() -> Self {
        Self {
            recording: true,
            log: ReplayLog::default(),
            record_index: BTreeMap::new(),
            max_history_bytes: 64 * 1024 * 1024,
            retained_bytes: 0,
        }
    }
}
