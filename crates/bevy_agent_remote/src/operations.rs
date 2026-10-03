//! Bounded, authenticated operation outcomes shared by network adapters.
use crate::MAX_MESSAGE_BYTES;
use anyhow::{Result, anyhow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// Leave space for status metadata and its enclosing JSON-RPC response.
#[derive(Debug)]
pub(crate) struct RetryConflict;
impl std::fmt::Display for RetryConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("retry_key already identifies a different request")
    }
}
impl std::error::Error for RetryConflict {}

pub(crate) const OPERATION_RESPONSE_BYTES: usize = MAX_MESSAGE_BYTES - 4096;
const ENTRY_BYTES: usize = 1024;
const ACTIVE_BYTES: usize = OPERATION_RESPONSE_BYTES + ENTRY_BYTES;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationStatusParams {
    /// Supply exactly one identifier. Retry keys work before an operation ID is received.
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub retry_key: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}
impl OperationStatusParams {
    pub(crate) fn validate(&self) -> Result<(), String> {
        match (&self.operation_id, &self.retry_key) {
            (Some(id), None)
                if !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-') =>
            {
                Ok(())
            }
            (None, Some(key)) => validate_retry_key(key),
            _ => Err("supply exactly one valid operation_id or retry_key".into()),
        }
    }
}
pub(crate) fn validate_retry_key(key: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        Err("retry_key must contain 1..128 ASCII letters, digits, or -_.:".into())
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Queued,
    Running,
    Completed,
    Cancelled,
    Error,
}
impl OperationState {
    fn terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct OperationStatus {
    pub operation_id: String,
    pub method: String,
    pub state: OperationState,
    pub response: Option<Value>,
    pub created_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

struct Entry {
    method: String,
    retry_key: Option<String>,
    identity: Option<Arc<str>>,
    identity_bytes: usize,
    state: OperationState,
    response: Option<Arc<str>>,
    reply: Arc<Mutex<Option<Arc<str>>>>,
    created_at_ms: u64,
    finished_at_ms: Option<u64>,
    finished: Option<Instant>,
    charge: usize,
    deadline: Option<(Instant, String)>,
}
struct LedgerData {
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    bytes: usize,
    sequence: u64,
    retry_keys: HashMap<String, String>,
}
struct LedgerInner {
    data: Mutex<LedgerData>,
    changed: Condvar,
    epoch: String,
    max_entries: usize,
    max_bytes: usize,
    retention: Duration,
}
#[derive(Clone)]
pub(crate) struct OperationLedger(Arc<LedgerInner>);
#[derive(Clone)]
pub(crate) struct OperationHandle {
    ledger: OperationLedger,
    id: String,
    reply: Arc<Mutex<Option<Arc<str>>>>,
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
impl OperationLedger {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.data.lock().unwrap().entries.is_empty()
    }
    pub(crate) fn new(max_entries: usize, max_bytes: usize, retention: Duration) -> Result<Self> {
        if max_entries == 0 || max_bytes < ACTIVE_BYTES || retention.is_zero() {
            return Err(anyhow!(
                "operation limits must retain at least one maximum-size response and have a positive lifetime"
            ));
        }
        let epoch = format!(
            "{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        Ok(Self(Arc::new(LedgerInner {
            data: Mutex::new(LedgerData {
                entries: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
                sequence: 0,
                retry_keys: HashMap::new(),
            }),
            changed: Condvar::new(),
            epoch,
            max_entries,
            max_bytes,
            retention,
        })))
    }
    fn prune(&self, data: &mut LedgerData, now: Instant) {
        let expired: Vec<_> = data
            .order
            .iter()
            .filter(|id| {
                data.entries.get(*id).is_some_and(|e| {
                    e.finished
                        .is_some_and(|at| now.saturating_duration_since(at) >= self.0.retention)
                })
            })
            .cloned()
            .collect();
        for id in expired {
            Self::remove(data, &id);
        }
    }
    fn remove(data: &mut LedgerData, id: &str) {
        if let Some(entry) = data.entries.remove(id) {
            data.bytes -= entry.charge;
            if let Some(key) = entry.retry_key {
                data.retry_keys.remove(&key);
            }
        }
        data.order.retain(|candidate| candidate != id);
    }
    #[cfg(test)]
    pub(crate) fn admit(&self, method: &str) -> Result<OperationHandle> {
        self.admit_request(method, None, None)
            .map(|(handle, _)| handle)
    }
    /// Atomic key lookup and admission: concurrent retries share one execution.
    pub(crate) fn admit_request(
        &self,
        method: &str,
        retry_key: Option<String>,
        identity: Option<Arc<str>>,
    ) -> Result<(OperationHandle, bool)> {
        let mut data = self.0.data.lock().expect("operation ledger poisoned");
        self.prune(&mut data, Instant::now());
        if let Some(key) = &retry_key
            && let Some(id) = data.retry_keys.get(key)
        {
            let entry = &data.entries[id];
            if entry.method != method || entry.identity != identity {
                return Err(RetryConflict.into());
            }
            return Ok((
                OperationHandle {
                    ledger: self.clone(),
                    id: id.clone(),
                    reply: entry.reply.clone(),
                },
                false,
            ));
        }
        let identity_bytes =
            identity.as_ref().map_or(0, |s| s.len()) + retry_key.as_ref().map_or(0, String::len);
        let charge = ACTIVE_BYTES + identity_bytes;
        if charge > self.0.max_bytes {
            return Err(anyhow!("operation request exceeds ledger byte budget"));
        }
        while data.entries.len() >= self.0.max_entries || data.bytes + charge > self.0.max_bytes {
            let oldest = data
                .order
                .iter()
                .find(|id| {
                    data.entries
                        .get(*id)
                        .is_some_and(|entry| entry.state.terminal())
                })
                .cloned();
            let Some(id) = oldest else {
                return Err(anyhow!(
                    "operation ledger is full; retry after outstanding work completes"
                ));
            };
            Self::remove(&mut data, &id);
        }
        data.sequence = data
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow!("operation ID space exhausted"))?;
        let id = format!("{}-{:x}", self.0.epoch, data.sequence);
        let reply = Arc::new(Mutex::new(None));
        if let Some(key) = &retry_key {
            data.retry_keys.insert(key.clone(), id.clone());
        }
        data.entries.insert(
            id.clone(),
            Entry {
                method: method.to_owned(),
                retry_key,
                identity,
                identity_bytes,
                state: OperationState::Queued,
                response: None,
                reply: reply.clone(),
                created_at_ms: wall_ms(),
                finished_at_ms: None,
                finished: None,
                charge,
                deadline: None,
            },
        );
        data.bytes += charge;
        data.order.push_back(id.clone());
        Ok((
            OperationHandle {
                ledger: self.clone(),
                id,
                reply,
            },
            true,
        ))
    }
    pub(crate) fn status(&self, id: &str) -> Result<OperationStatus> {
        let (method, state, response, created_at_ms, finished_at_ms) = {
            let mut data = self.0.data.lock().expect("operation ledger poisoned");
            self.prune(&mut data, Instant::now());
            let entry = data.entries.get(id).ok_or_else(|| {
                anyhow!("operation is unknown or its retained outcome has expired")
            })?;
            (
                entry.method.clone(),
                entry.state,
                entry.response.clone(),
                entry.created_at_ms,
                entry.finished_at_ms,
            )
        };
        Ok(OperationStatus {
            operation_id: id.to_owned(),
            method,
            state,
            response: response
                .map(|body| serde_json::from_str(&body))
                .transpose()?,
            created_at_ms,
            finished_at_ms,
        })
    }
    pub(crate) fn status_by_key(&self, key: &str) -> Result<OperationStatus> {
        let id = {
            let mut data = self.0.data.lock().expect("operation ledger poisoned");
            self.prune(&mut data, Instant::now());
            data.retry_keys.get(key).cloned().ok_or_else(|| {
                anyhow!("retry key is unknown or its retained outcome has expired")
            })?
        };
        self.status(&id)
    }
    fn finish(data: &mut LedgerData, id: &str, state: OperationState, response: String) -> bool {
        let Some(entry) = data.entries.get_mut(id) else {
            return false;
        };
        if entry.state.terminal() {
            return false;
        }
        data.bytes -= entry.charge;
        entry.charge = ENTRY_BYTES + response.len() + entry.identity_bytes;
        data.bytes += entry.charge;
        entry.state = state;
        let response: Arc<str> = Arc::from(response);
        *entry.reply.lock().expect("operation reply poisoned") = Some(response.clone());
        entry.response = Some(response);
        entry.finished_at_ms = Some(wall_ms());
        entry.finished = Some(Instant::now());
        entry.deadline = None;
        // Capacity eviction follows completion order, rather than removing a
        // newly completed slow operation because it was admitted first.
        data.order.retain(|candidate| candidate != id);
        data.order.push_back(id.to_owned());
        true
    }
    /// The listener's one maintenance loop also owns asynchronous capture expiry.
    pub(crate) fn maintain(&self) {
        let mut data = self.0.data.lock().expect("operation ledger poisoned");
        let now = Instant::now();
        let expired: Vec<_> = data
            .entries
            .iter()
            .filter_map(|(id, e)| {
                e.deadline
                    .as_ref()
                    .filter(|(at, _)| *at <= now)
                    .map(|(_, response)| (id.clone(), response.clone()))
            })
            .collect();
        for (id, response) in expired {
            Self::finish(&mut data, &id, OperationState::Error, response);
        }
        self.prune(&mut data, now);
        self.0.changed.notify_all();
    }
    pub(crate) fn shutdown(&self) {
        // Waiting connection workers own the original JSON-RPC identifiers
        // and publish their correlated cancellation after seeing stop=true.
        self.0.changed.notify_all();
    }
}
impl OperationHandle {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    #[cfg(any(feature = "visual", test))]
    pub(crate) fn state(&self) -> Option<OperationState> {
        self.ledger
            .0
            .data
            .lock()
            .expect("operation ledger poisoned")
            .entries
            .get(&self.id)
            .map(|e| e.state)
    }
    pub(crate) fn try_claim(&self) -> bool {
        let mut data = self
            .ledger
            .0
            .data
            .lock()
            .expect("operation ledger poisoned");
        let Some(entry) = data.entries.get_mut(&self.id) else {
            return false;
        };
        if entry.state != OperationState::Queued {
            return false;
        }
        entry.state = OperationState::Running;
        self.ledger.0.changed.notify_all();
        true
    }
    pub(crate) fn complete(&self, response: String) -> bool {
        debug_assert!(response.len() <= OPERATION_RESPONSE_BYTES);
        #[derive(Deserialize)]
        struct Outcome {
            error: Option<serde::de::IgnoredAny>,
        }
        let state = if serde_json::from_str::<Outcome>(&response)
            .map_or(true, |value| value.error.is_some())
        {
            OperationState::Error
        } else {
            OperationState::Completed
        };
        let mut data = self
            .ledger
            .0
            .data
            .lock()
            .expect("operation ledger poisoned");
        if !data
            .entries
            .get(&self.id)
            .is_some_and(|e| e.state == OperationState::Running)
        {
            return false;
        }
        let finished = OperationLedger::finish(&mut data, &self.id, state, response);
        self.ledger.0.changed.notify_all();
        finished
    }
    pub(crate) fn cancel(&self, response: String) -> bool {
        let mut data = self
            .ledger
            .0
            .data
            .lock()
            .expect("operation ledger poisoned");
        if !data
            .entries
            .get(&self.id)
            .is_some_and(|e| e.state == OperationState::Queued)
        {
            return false;
        }
        let cancelled =
            OperationLedger::finish(&mut data, &self.id, OperationState::Cancelled, response);
        self.ledger.0.changed.notify_all();
        cancelled
    }
    #[cfg(any(feature = "visual", test))]
    pub(crate) fn expire_at(&self, deadline: Instant, response: String) {
        let mut data = self
            .ledger
            .0
            .data
            .lock()
            .expect("operation ledger poisoned");
        if let Some(entry) = data.entries.get_mut(&self.id)
            && entry.state == OperationState::Running
        {
            entry.deadline = Some((deadline, response));
        }
    }
    pub(crate) fn wait_until(&self, deadline: Instant, stop: &AtomicBool) -> Option<String> {
        let mut data = self
            .ledger
            .0
            .data
            .lock()
            .expect("operation ledger poisoned");
        loop {
            if let Some(response) = self
                .reply
                .lock()
                .expect("operation reply poisoned")
                .as_ref()
            {
                return Some(response.to_string());
            }
            data.entries.get(&self.id)?;
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return None;
            }
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(25));
            data = self
                .ledger
                .0
                .changed
                .wait_timeout(data, remaining)
                .expect("operation ledger poisoned")
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ledger(entries: usize) -> OperationLedger {
        OperationLedger::new(entries, ACTIVE_BYTES * 2, Duration::from_secs(60)).unwrap()
    }
    fn success() -> String {
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tick\":1}}".into()
    }
    #[test]
    fn active_operations_are_never_evicted_and_bytes_bound_admission() {
        let ledger = ledger(10);
        let a = ledger.admit("step").unwrap();
        let b = ledger.admit("step").unwrap();
        assert!(ledger.admit("step").is_err());
        assert_eq!(ledger.status(a.id()).unwrap().state, OperationState::Queued);
        assert!(a.try_claim());
        assert!(a.complete(success()));
        let c = ledger.admit("step").unwrap();
        assert!(ledger.status(b.id()).is_ok());
        assert!(ledger.status(c.id()).is_ok());
    }
    #[test]
    fn terminal_count_and_age_retention_are_bounded() {
        let ledger = ledger(1);
        let old = ledger.admit("step").unwrap();
        assert!(old.try_claim());
        assert!(old.complete(success()));
        let next = ledger.admit("step").unwrap();
        assert!(ledger.status(old.id()).is_err());
        assert!(ledger.status(next.id()).is_ok());
        let ledger = OperationLedger::new(2, ACTIVE_BYTES * 2, Duration::from_millis(1)).unwrap();
        let done = ledger.admit("step").unwrap();
        done.try_claim();
        done.complete(success());
        std::thread::sleep(Duration::from_millis(3));
        assert!(ledger.status(done.id()).is_err());
    }
    #[test]
    fn completion_once_and_timeout_keeps_eventual_outcome() {
        let ledger = ledger(2);
        let op = ledger.admit("step").unwrap();
        assert!(op.try_claim());
        let stop = AtomicBool::new(false);
        assert!(op.wait_until(Instant::now(), &stop).is_none());
        assert!(!op.cancel(success()));
        assert!(op.complete(success()));
        assert!(!op.complete(success()));
        assert_eq!(
            ledger.status(op.id()).unwrap().state,
            OperationState::Completed
        );
        assert!(ledger.status(op.id()).unwrap().response.is_some());
    }
    #[test]
    fn cancellation_skips_execution_and_timer_has_one_completion() {
        let ledger = ledger(2);
        let op = ledger.admit("step").unwrap();
        assert!(op.cancel(success()));
        assert!(!op.try_claim());
        let op = ledger.admit("capture").unwrap();
        op.try_claim();
        op.expire_at(Instant::now(), "{\"error\":{\"code\":-1}}".into());
        ledger.maintain();
        assert_eq!(op.state(), Some(OperationState::Error));
        assert!(!op.complete(success()));
    }
    #[test]
    fn racing_completion_publishes_one_outcome_and_survives_capacity_eviction() {
        let ledger = ledger(1);
        let op = ledger.admit("capture").unwrap();
        op.try_claim();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = [1, 2]
            .into_iter()
            .map(|number| {
                let op = op.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    op.complete(format!("{{\"result\":{number}}}"))
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        ledger.admit("next").unwrap();
        assert!(ledger.status(op.id()).is_err());
        assert!(
            op.wait_until(Instant::now(), &AtomicBool::new(false))
                .is_some()
        );
    }
}
