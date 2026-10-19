use crate::*;
use async_trait::async_trait;
use cayw_ai::CancellationToken;
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct ReadPolicy {
    pub capability: String,
    pub reuse: bool,
    pub coalesce: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOrigin {
    pub execution: CellExecution,
    pub sequence: u64,
}
/// Supplied only by an independently admitted, trusted Host ticket. The content
/// digest is filled from capture; metadata never stands in for content bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadStamp {
    pub workspace: String,
    pub incarnation: String,
    pub generation: u64,
    pub semantics: String,
}
#[async_trait]
pub trait ReadSource: Send {
    /// A trusted admitted source can require an original uncached capture when
    /// an unconfined writer made a cooperating frozen view unavailable.
    fn can_share(&self) -> bool {
        true
    }
    async fn capture(&mut self, cancel: CancellationToken) -> Result<Value, CapabilityError>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadRole {
    Leader,
    Follower,
    Ready,
    Uncached,
}
#[derive(Clone)]
struct Published {
    value: Value,
    dependencies: DependencySnapshot,
    source: ReadOrigin,
}
type Delivery = Result<Published, CapabilityError>;
struct Entry {
    tx: watch::Sender<Option<Delivery>>,
    ticket: u64,
}
#[derive(Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    epochs: BTreeMap<String, u64>,
    frozen: BTreeSet<String>,
    next_ticket: u64,
}
pub struct ReadCoordinator {
    policies: BTreeMap<String, ReadPolicy>,
    limits: CellLimits,
    state: Mutex<State>,
}
impl ReadCoordinator {
    pub fn new(policies: Vec<ReadPolicy>, limits: CellLimits) -> Result<Arc<Self>, CellError> {
        limits.validate()?;
        let mut registered = BTreeMap::new();
        for policy in policies {
            if policy.capability.is_empty()
                || policy.capability.len() > 256
                || registered
                    .insert(policy.capability.clone(), policy)
                    .is_some()
            {
                return Err(CellError::Invalid(
                    "duplicate or invalid trusted read policy".into(),
                ));
            }
        }
        Ok(Arc::new(Self {
            policies: registered,
            limits,
            state: Mutex::default(),
        }))
    }
    pub fn invalidate(&self, session: &str) {
        let mut state = self.state.lock().expect("read coordinator lock poisoned");
        let epoch = state.epochs.entry(session.into()).or_default();
        match epoch.checked_add(1) {
            Some(next) => *epoch = next,
            None => {
                state.frozen.insert(session.into());
            }
        }
        reject_entries(&mut state, "dependency invalidated before write");
    }
    pub fn freeze(&self, session: &str) {
        let mut state = self.state.lock().expect("read coordinator lock poisoned");
        state.frozen.insert(session.into());
        reject_entries(&mut state, "session snapshot frozen");
    }
    pub(crate) fn is_frozen(&self, session: &str) -> bool {
        self.state
            .lock()
            .expect("read coordinator lock poisoned")
            .frozen
            .contains(session)
    }
    pub fn thaw(&self, scope: &StoppedScope) -> Result<(), CellError> {
        scope.finish()?;
        self.thaw_session(&scope.identity().session_id);
        Ok(())
    }
    pub(crate) fn thaw_session(&self, session: &str) {
        self.state
            .lock()
            .expect("read coordinator lock poisoned")
            .frozen
            .remove(session);
        self.invalidate(session);
    }
    pub async fn read(
        self: &Arc<Self>,
        origin: ReadOrigin,
        name: &str,
        arguments: &Value,
        stamp: ReadStamp,
        source: &mut dyn ReadSource,
        cancel: CancellationToken,
    ) -> Result<ReadCandidate, CapabilityError> {
        let fail = |error: CellError| CapabilityError::new(error.to_string());
        origin.execution.identity.validate().map_err(fail)?;
        if origin.sequence == 0 || cancel.is_cancelled() {
            return Err(CapabilityError::new(
                "read origin sequence is zero or call cancelled",
            ));
        }
        validate_json(arguments, self.limits.json_depth, self.limits.item_bytes).map_err(fail)?;
        for coordinate in [&stamp.workspace, &stamp.incarnation, &stamp.semantics] {
            if coordinate.is_empty() || coordinate.len() > 256 {
                return Err(CapabilityError::new("invalid frozen read stamp"));
            }
        }
        let policy = self
            .policies
            .get(name)
            .ok_or_else(|| CapabilityError::new("read policy not explicitly registered"))?;
        let sharing = source.can_share();
        let identity = &origin.execution.identity;
        let (key, epoch, follower, ticket) = {
            let mut state = self.state.lock().expect("read coordinator lock poisoned");
            if state.frozen.contains(&identity.session_id) {
                return Err(CapabilityError::new("session snapshot frozen"));
            }
            let epoch = *state.epochs.entry(identity.session_id.clone()).or_default();
            let key=serde_json::to_string(&json!({"identity":identity,"name":name,"arguments":arguments,"stamp":stamp,"epoch":epoch})).expect("bounded canonical JSON");
            let follower = state.entries.get(&key).and_then(|e| {
                let ready = e.tx.borrow().is_some();
                (sharing && ((ready && policy.reuse) || (!ready && policy.coalesce)))
                    .then(|| (e.tx.subscribe(), ready))
            });
            let ticket = if sharing
                && follower.is_none()
                && !state.entries.contains_key(&key)
                && (policy.reuse || policy.coalesce)
                && state.entries.len() < self.limits.cache_entries
            {
                if let Some(next) = state.next_ticket.checked_add(1) {
                    state.next_ticket = next;
                    let (tx, _) = watch::channel(None);
                    state
                        .entries
                        .insert(key.clone(), Entry { tx, ticket: next });
                    Some(next)
                } else {
                    None
                }
            } else {
                None
            };
            (key, epoch, follower, ticket)
        };
        if let Some((mut receiver, ready_hit)) = follower {
            loop {
                // A cancelled caller can never consume an already-ready entry.
                if cancel.is_cancelled() {
                    return Err(CapabilityError::new("read waiter cancelled"));
                }
                let delivery = receiver.borrow_and_update().clone();
                if let Some(delivery) = delivery {
                    let state = self.state.lock().expect("read coordinator lock poisoned");
                    if state.frozen.contains(&identity.session_id)
                        || state
                            .epochs
                            .get(&identity.session_id)
                            .copied()
                            .unwrap_or_default()
                            != epoch
                    {
                        return Err(CapabilityError::new("read snapshot changed while waiting"));
                    }
                    drop(state);
                    let published = delivery?;
                    return Ok(ReadCandidate {
                        value: Some(published.value),
                        dependencies: published.dependencies,
                        source: published.source,
                        role: if ready_hit {
                            ReadRole::Ready
                        } else {
                            ReadRole::Follower
                        },
                        coordinator: self.clone(),
                        session: identity.session_id.clone(),
                        publication: None,
                        acknowledged: false,
                    });
                }
                tokio::select! {biased;
                    _=cancel.cancelled()=>return Err(CapabilityError::new("read waiter cancelled")),
                    changed=receiver.changed()=>if changed.is_err(){return Err(CapabilityError::new("read owner stopped"));},
                }
            }
        }
        let mut leader = Leader {
            coordinator: self.clone(),
            key: key.clone(),
            ticket,
            settled: false,
        };
        let result = source.capture(cancel.clone()).await;
        if result.as_ref().is_err_and(|e| e.outcome_unknown) {
            self.freeze(&identity.session_id);
        }
        let value = result?;
        if let Err(error) = validate_json(&value, self.limits.json_depth, self.limits.item_bytes) {
            discard_json(value);
            return Err(fail(error));
        }
        if cancel.is_cancelled() {
            discard_json(value);
            return Err(CapabilityError::new("read capture cancelled"));
        }
        let state = self.state.lock().expect("read coordinator lock poisoned");
        if state.frozen.contains(&identity.session_id)
            || state
                .epochs
                .get(&identity.session_id)
                .copied()
                .unwrap_or_default()
                != epoch
        {
            discard_json(value);
            return Err(CapabilityError::new("read snapshot changed during capture"));
        }
        drop(state);
        let dependencies = DependencySnapshot {
            fingerprint: HostResult::value_hash(&value),
            permission_context: identity.permission_context.clone(),
            versions: BTreeMap::from([
                ("workspace".into(), stamp.workspace),
                ("incarnation".into(), stamp.incarnation),
                ("generation".into(), stamp.generation.to_string()),
                ("semantics".into(), stamp.semantics),
            ]),
        };
        dependencies
            .validate(&identity.permission_context, &self.limits)
            .map_err(fail)?;
        leader.settled = true;
        Ok(ReadCandidate {
            value: Some(value),
            dependencies,
            source: origin.clone(),
            role: if ticket.is_some() {
                ReadRole::Leader
            } else {
                ReadRole::Uncached
            },
            coordinator: self.clone(),
            session: identity.session_id.clone(),
            publication: ticket.map(|ticket| Publication {
                key,
                ticket,
                epoch,
                reuse: policy.reuse,
            }),
            acknowledged: false,
        })
    }
}
struct Publication {
    key: String,
    ticket: u64,
    epoch: u64,
    reuse: bool,
}
/// A leader is only a candidate until the original Cells commit acknowledges it.
/// Followers never borrow the leader's authorization or terminal identity.
pub struct ReadCandidate {
    value: Option<Value>,
    pub dependencies: DependencySnapshot,
    pub source: ReadOrigin,
    pub role: ReadRole,
    coordinator: Arc<ReadCoordinator>,
    session: String,
    publication: Option<Publication>,
    acknowledged: bool,
}
impl ReadCandidate {
    pub fn value(&self) -> &Value {
        self.value.as_ref().expect("candidate value")
    }
    /// Completes a confirmed refusal after its original terminal committed.
    /// Unlike abandonment, this wakes only this leader's waiters without freezing
    /// unrelated admitted calls or removing an already-published ready entry.
    pub fn reject(mut self, reason: &str) {
        if let Some(publication) = self.publication.take() {
            let mut state = self
                .coordinator
                .state
                .lock()
                .expect("read coordinator lock poisoned");
            if state
                .entries
                .get(&publication.key)
                .is_some_and(|entry| entry.ticket == publication.ticket)
                && let Some(entry) = state.entries.remove(&publication.key)
            {
                entry
                    .tx
                    .send_replace(Some(Err(CapabilityError::new(bounded_error(reason)))));
            }
        }
        self.acknowledged = true;
    }
    pub fn acknowledge(mut self) {
        if let Some(publication) = self.publication.take() {
            let mut state = self
                .coordinator
                .state
                .lock()
                .expect("read coordinator lock poisoned");
            let valid = !state.frozen.contains(&self.session)
                && state.epochs.get(&self.session).copied().unwrap_or_default()
                    == publication.epoch;
            if valid
                && state
                    .entries
                    .get(&publication.key)
                    .is_some_and(|e| e.ticket == publication.ticket)
            {
                state
                    .entries
                    .get(&publication.key)
                    .expect("entry checked")
                    .tx
                    .send_replace(Some(Ok(Published {
                        value: self.value.take().expect("candidate value"),
                        dependencies: self.dependencies.clone(),
                        source: self.source.clone(),
                    })));
                if !publication.reuse {
                    state.entries.remove(&publication.key);
                }
            }
        }
        self.acknowledged = true;
    }
}
impl Drop for ReadCandidate {
    fn drop(&mut self) {
        if !self.acknowledged {
            self.coordinator.freeze(&self.session);
        }
        if let Some(value) = self.value.take() {
            discard_json(value);
        }
    }
}
fn reject_entries(state: &mut State, message: &str) {
    for (_, entry) in std::mem::take(&mut state.entries) {
        entry
            .tx
            .send_replace(Some(Err(CapabilityError::new(message))));
    }
}
struct Leader {
    coordinator: Arc<ReadCoordinator>,
    key: String,
    ticket: Option<u64>,
    settled: bool,
}
impl Drop for Leader {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut state = self
            .coordinator
            .state
            .lock()
            .expect("read coordinator lock poisoned");
        if state
            .entries
            .get(&self.key)
            .is_some_and(|e| Some(e.ticket) == self.ticket)
            && let Some(entry) = state.entries.remove(&self.key)
        {
            entry.tx.send_replace(Some(Err(CapabilityError::new(
                "read owner stopped before Cells acknowledgment",
            ))));
        }
    }
}
