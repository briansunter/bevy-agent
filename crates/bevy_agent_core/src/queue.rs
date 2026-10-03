//! Checked, tick-indexed future input and deterministic queue restoration.

use crate::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScheduledAction<A = AgentAction> {
    pub tick: u64,
    pub source: ActionSource,
    pub action: A,
}

#[derive(Resource, Clone, Debug, Serialize)]
pub struct AgentActionQueue<A = AgentAction> {
    pending: BTreeMap<u64, Vec<ScheduledAction<A>>>,
}

impl<A> Default for AgentActionQueue<A> {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
        }
    }
}

impl<'de, A: Deserialize<'de>> Deserialize<'de> for AgentActionQueue<A> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct QueueData<A> {
            pending: BTreeMap<u64, Vec<ScheduledAction<A>>>,
        }
        let data = QueueData::<A>::deserialize(deserializer)?;
        for (tick, bucket) in &data.pending {
            validate_clock_tick(*tick).map_err(serde::de::Error::custom)?;
            if bucket.is_empty() || bucket.iter().any(|action| action.tick != *tick) {
                return Err(serde::de::Error::custom(
                    "invalid pending action tick bucket",
                ));
            }
        }
        Ok(Self {
            pending: data.pending,
        })
    }
}

impl<A> AgentActionQueue<A> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &ScheduledAction<A>> {
        self.pending.values().flat_map(|bucket| bucket.iter())
    }
    pub fn at_tick(&self, tick: u64) -> impl Iterator<Item = &ScheduledAction<A>> {
        self.pending
            .get(&tick)
            .into_iter()
            .flat_map(|bucket| bucket.iter())
    }
    pub fn future_after(&self, tick: u64) -> impl Iterator<Item = &ScheduledAction<A>> {
        self.pending
            .range((std::ops::Bound::Excluded(tick), std::ops::Bound::Unbounded))
            .flat_map(|(_, bucket)| bucket.iter())
    }
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn drain_before_or_at(&mut self, tick: u64) -> Vec<ScheduledAction<A>> {
        let future = self.pending.split_off(&tick);
        // Only the due bucket is materialized; stale buckets are discarded.
        self.pending = future;
        self.pending.remove(&tick).unwrap_or_default()
    }

    fn insert(&mut self, scheduled: ScheduledAction<A>) {
        self.pending
            .entry(scheduled.tick)
            .or_default()
            .push(scheduled);
    }
}

impl AgentActionQueue {
    #[allow(clippy::too_many_arguments)]
    pub fn schedule(
        &mut self,
        catalog: &AgentActionCatalog,
        clock: &SimClock,
        control: &AgentControlState,
        context: &ExecutionContext,
        tick: u64,
        source: ActionSource,
        action: AgentAction,
    ) -> ControlResult<()> {
        clock.validate()?;
        validate_scheduled_action(
            catalog,
            clock.tick,
            &control.mode,
            context,
            tick,
            &source,
            &action,
        )?;
        self.insert(ScheduledAction {
            tick,
            source,
            action,
        });
        Ok(())
    }

    /// Restores already accepted future input atomically. Source arbitration
    /// remains request-local: a later control-mode change cannot erase intent.
    pub fn replace_pending(
        &mut self,
        catalog: &AgentActionCatalog,
        current_tick: u64,
        actions: Vec<ScheduledAction>,
    ) -> ControlResult<()> {
        for scheduled in &actions {
            validate_future_action(catalog, current_tick, scheduled)?;
        }
        self.clear();
        for action in actions {
            self.insert(action);
        }
        Ok(())
    }
}

pub fn validate_future_action(
    catalog: &AgentActionCatalog,
    current_tick: u64,
    scheduled: &ScheduledAction,
) -> ControlResult<()> {
    validate_clock_tick(current_tick)?;
    validate_clock_tick(scheduled.tick)?;
    if scheduled.tick <= current_tick {
        return Err(AgentControlError::InvalidAction(format!(
            "scheduled tick {} must follow current tick {current_tick}",
            scheduled.tick
        )));
    }
    catalog.validate_action(&scheduled.action)
}

#[allow(clippy::too_many_arguments)]
pub fn validate_scheduled_action(
    catalog: &AgentActionCatalog,
    current_tick: u64,
    mode: &ControlMode,
    context: &ExecutionContext,
    tick: u64,
    source: &ActionSource,
    action: &AgentAction,
) -> ControlResult<()> {
    validate_clock_tick(current_tick)?;
    validate_clock_tick(tick)?;
    if tick <= current_tick {
        return Err(AgentControlError::InvalidAction(format!(
            "scheduled tick {tick} must follow current tick {current_tick}"
        )));
    }
    catalog.validate_action(action)?;
    if *context != ExecutionContext::Reconstructing && !mode.accepts_source(source) {
        return Err(AgentControlError::InvalidAction(format!(
            "source {source:?} is not accepted in mode {mode:?}"
        )));
    }
    Ok(())
}

/// Schedule against the world's actual catalog, cursor, source policy and
/// execution context, without mutating the queue on rejection.
pub fn schedule_action(
    world: &mut World,
    tick: u64,
    source: ActionSource,
    action: AgentAction,
) -> ControlResult<()> {
    let catalog = world
        .get_resource::<AgentActionCatalog>()
        .ok_or(AgentControlError::MissingResource("AgentActionCatalog"))?;
    let clock = world
        .get_resource::<SimClock>()
        .ok_or(AgentControlError::MissingResource("SimClock"))?;
    clock.validate()?;
    let control = world
        .get_resource::<AgentControlState>()
        .ok_or(AgentControlError::MissingResource("AgentControlState"))?;
    let context = world
        .get_resource::<ExecutionContext>()
        .ok_or(AgentControlError::MissingResource("ExecutionContext"))?;
    validate_scheduled_action(
        catalog,
        clock.tick,
        &control.mode,
        context,
        tick,
        &source,
        &action,
    )?;
    world
        .get_resource_mut::<AgentActionQueue>()
        .ok_or(AgentControlError::MissingResource("AgentActionQueue"))?
        .insert(ScheduledAction {
            tick,
            source,
            action,
        });
    Ok(())
}

/// Merge only missing occurrences, preserving source, multiplicity and order
/// within each tick. Validation completes before the queue changes.
pub fn merge_pending_actions(
    queue: &mut AgentActionQueue,
    catalog: &AgentActionCatalog,
    current_tick: u64,
    before: Vec<ScheduledAction>,
) -> ControlResult<()> {
    for action in &before {
        validate_future_action(catalog, current_tick, action)?;
    }
    // Compare only actions at the same tick and count each existing match
    // once, rather than repeatedly scanning all earlier future inputs.
    let mut counts: BTreeMap<u64, Vec<(&ScheduledAction, usize, usize)>> = BTreeMap::new();
    for scheduled in &before {
        let bucket = counts.entry(scheduled.tick).or_default();
        let index = match bucket
            .iter()
            .position(|(action, _, _)| *action == scheduled)
        {
            Some(index) => index,
            None => {
                let present = queue
                    .at_tick(scheduled.tick)
                    .filter(|candidate| *candidate == scheduled)
                    .count();
                bucket.push((scheduled, 0, present));
                bucket.len() - 1
            }
        };
        let (_, needed, present) = &mut bucket[index];
        *needed += 1;
        if *present < *needed {
            queue.insert(scheduled.clone());
            *present += 1;
        }
    }
    Ok(())
}
