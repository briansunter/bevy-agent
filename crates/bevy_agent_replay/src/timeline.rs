use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use bevy_agent_core::{BranchId, SnapshotId, TimelineId, validate_clock_tick};
use serde::{Deserialize, Serialize};

/// Maximum branch ids visited during any parent-chain traversal.
pub const MAX_LINEAGE_DEPTH: usize = 1024;
/// Maximum branches admitted into a live topology.
pub const MAX_TIMELINE_BRANCHES: usize = 10_000;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineBranch {
    pub branch_id: BranchId,
    pub parent_branch: Option<BranchId>,
    pub fork_tick: u64,
    pub fork_snapshot: Option<SnapshotId>,
    pub label: Option<String>,
}

#[derive(Resource, Clone, Debug, Serialize)]
pub struct Timeline {
    pub(crate) timeline_id: TimelineId,
    pub(crate) current_branch: BranchId,
    pub(crate) branches: HashMap<BranchId, TimelineBranch>,
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
    #[must_use]
    pub fn timeline_id(&self) -> TimelineId {
        self.timeline_id
    }

    #[must_use]
    pub fn current_branch(&self) -> BranchId {
        self.current_branch
    }

    #[must_use]
    pub fn branches(&self) -> &HashMap<BranchId, TimelineBranch> {
        &self.branches
    }

    /// Admit an inspectable topology only after validating its complete tree.
    pub fn from_branches(
        timeline_id: TimelineId,
        current_branch: BranchId,
        branches: Vec<TimelineBranch>,
    ) -> Result<Self, String> {
        let count = branches.len();
        let branches: HashMap<_, _> = branches
            .into_iter()
            .map(|branch| (branch.branch_id, branch))
            .collect();
        if branches.len() != count {
            return Err("timeline has duplicate branch ids".to_string());
        }
        let timeline = Self {
            timeline_id,
            current_branch,
            branches,
        };
        timeline.validate()?;
        Ok(timeline)
    }

    pub fn select_branch(&mut self, branch: BranchId) -> Result<(), String> {
        if !self.branches.contains_key(&branch) {
            return Err(format!("cannot select unknown branch {branch:?}"));
        }
        self.current_branch = branch;
        Ok(())
    }

    pub fn create_branch(
        &mut self,
        fork_tick: u64,
        fork_snapshot: Option<SnapshotId>,
        label: Option<String>,
    ) -> Result<BranchId, String> {
        validate_clock_tick(fork_tick).map_err(|error| error.to_string())?;
        if self.branches.len() >= MAX_TIMELINE_BRANCHES {
            return Err(format!(
                "timeline exceeds maximum of {MAX_TIMELINE_BRANCHES} branches"
            ));
        }
        let parent = self.current_branch;
        let parent_info = self
            .branches
            .get(&parent)
            .ok_or_else(|| "timeline current branch is missing".to_string())?;
        if fork_tick < parent_info.fork_tick {
            return Err("branch fork precedes parent fork".to_string());
        }
        if self.ancestors_bounded(parent)?.len() >= MAX_LINEAGE_DEPTH {
            return Err(format!(
                "timeline lineage exceeds maximum depth {MAX_LINEAGE_DEPTH}"
            ));
        }
        let branch_id = BranchId::new();
        self.branches.insert(
            branch_id,
            TimelineBranch {
                branch_id,
                parent_branch: Some(parent),
                fork_tick,
                fork_snapshot,
                label,
            },
        );
        self.current_branch = branch_id;
        Ok(branch_id)
    }

    /// Ancestry chain from `branch` up to the root (branch first, root last).
    #[must_use]
    pub fn lineage(&self, branch: BranchId) -> Vec<BranchId> {
        self.ancestors_bounded(branch).unwrap_or_default()
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

    /// Validate the live tree before importing or navigating history.
    /// Readers also use bounded traversal as a defense against corrupted input.
    pub fn validate(&self) -> Result<(), String> {
        if self.branches.len() > MAX_TIMELINE_BRANCHES {
            return Err(format!(
                "timeline exceeds maximum of {MAX_TIMELINE_BRANCHES} branches"
            ));
        }
        if !self.branches.contains_key(&self.current_branch) {
            return Err(format!(
                "timeline current branch {:?} is unknown",
                self.current_branch
            ));
        }
        let roots = self
            .branches
            .values()
            .filter(|branch| branch.parent_branch.is_none())
            .count();
        if roots != 1 {
            return Err(format!(
                "timeline must contain exactly one root, found {roots}"
            ));
        }
        for (id, branch) in &self.branches {
            validate_clock_tick(branch.fork_tick).map_err(|error| error.to_string())?;
            if *id != branch.branch_id {
                return Err(format!(
                    "timeline key {id:?} disagrees with branch {:?}",
                    branch.branch_id
                ));
            }
            if let Some(parent_id) = branch.parent_branch {
                let parent = self.branches.get(&parent_id).ok_or_else(|| {
                    format!("timeline branch {id:?} has unknown parent {parent_id:?}")
                })?;
                if branch.fork_tick < parent.fork_tick {
                    return Err(format!("timeline branch {id:?} fork precedes parent fork"));
                }
            }
            self.ancestors_bounded(*id)?;
        }
        Ok(())
    }
}

/// Bounded ancestry chain from `branch` up to the root (branch first,
/// root last).
///
/// Walks at most [`MAX_LINEAGE_DEPTH`] (1024) parent links; returns `Err` on
/// a cycle (repeated id, including self-parents) or when the chain exceeds
/// the depth bound, or when a branch or parent id is unknown.
pub fn ancestors_bounded(timeline: &Timeline, branch: BranchId) -> Result<Vec<BranchId>, String> {
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
            None => {
                return Err(format!(
                    "timeline lineage for {branch:?} references unknown branch {id:?}"
                ));
            }
            Some(info) => current = info.parent_branch,
        }
    }
    if current.is_none() {
        Ok(chain)
    } else {
        Err(format!(
            "timeline lineage for {branch:?} exceeds maximum depth {MAX_LINEAGE_DEPTH}"
        ))
    }
}

/// Returns true when `ancestor` equals `descendant` or appears in the
/// descendant's parent chain.
#[must_use]
pub fn lineage_contains(timeline: &Timeline, ancestor: BranchId, descendant: BranchId) -> bool {
    timeline
        .ancestors_bounded(descendant)
        .is_ok_and(|chain| chain.contains(&ancestor))
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
    let chain = timeline.ancestors_bounded(descendant).ok()?;
    chain.windows(2).find_map(|pair| {
        (pair[1] == ancestor)
            .then(|| {
                timeline
                    .branches
                    .get(&pair[0])
                    .map(|branch| branch.fork_tick)
            })
            .flatten()
    })
}

/// Fork-bounded visibility shared by action and checkpoint queries.
/// Ancestors contribute `(own_fork, child_fork]`; the target branch
/// contributes its own history. Unknown branch identities are rejected.
#[must_use]
pub fn branch_record_visible(
    timeline: &Timeline,
    record_branch: BranchId,
    record_tick: u64,
    target: BranchId,
) -> bool {
    BranchVisibility::new(timeline, target).contains(record_branch, record_tick)
}

/// Precomputed fork intervals for one query, avoiding a fresh ancestry walk
/// for every action/checkpoint/checksum in a long recording.
pub(crate) struct BranchVisibility {
    intervals: HashMap<BranchId, Option<(u64, u64)>>,
}

impl BranchVisibility {
    pub(crate) fn new(timeline: &Timeline, target: BranchId) -> Self {
        let mut intervals = HashMap::new();
        if !timeline.branches.contains_key(&target) {
            return Self { intervals };
        }
        let Ok(chain) = timeline.ancestors_bounded(target) else {
            return Self { intervals };
        };
        intervals.insert(target, None);
        for pair in chain.windows(2) {
            let child = &timeline.branches[&pair[0]];
            let parent = &timeline.branches[&pair[1]];
            intervals.insert(parent.branch_id, Some((parent.fork_tick, child.fork_tick)));
        }
        Self { intervals }
    }

    pub(crate) fn contains(&self, branch: BranchId, tick: u64) -> bool {
        match self.intervals.get(&branch) {
            Some(None) => true,
            Some(Some((own_fork, child_fork))) => tick > *own_fork && tick <= *child_fork,
            None => false,
        }
    }

    pub(crate) fn range(&self, branch: BranchId, start: u64, end: u64) -> Option<(u64, u64)> {
        let (start, end) = match self.intervals.get(&branch)? {
            None => (start, end),
            Some((own_fork, child_fork)) => (start.max(*own_fork), end.min(*child_fork)),
        };
        (start < end).then_some((start, end))
    }
}
