//! Broadcast `EventBus`, single-writer `PersistenceActor`, and priority `SchedulerActor`
//! (`P1-T18` / `ADR-004` / `Vortex-Architecture-Design.md` §9 & §10.1).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vortex_application::{
    AppEvent, AppResult, DownloadRepository, EventBus, EventLogRepository, SidecarMetadata,
};
use vortex_domain::{DomainEventEnvelope, Download, DownloadId, DownloadState, Priority, RangeSet};
use vortex_error::{ErrorCode, TraceId, VortexError};

/// Non-blocking publish/subscribe [`EventBus`] backed by `tokio::sync::broadcast`.
#[derive(Debug, Clone)]
pub struct BroadcastEventBus {
    tx: broadcast::Sender<AppEvent>,
}

impl BroadcastEventBus {
    /// Creates a new [`BroadcastEventBus`] with `capacity` buffered events (`default 1024`).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(64));
        Self { tx }
    }
}

impl Default for BroadcastEventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl EventBus for BroadcastEventBus {
    fn publish(&self, event: AppEvent) {
        // Ignore `SendError` when zero subscribers are currently attached
        let _ = self.tx.send(event);
    }

    fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }
}

/// Commands processed sequentially by [`PersistenceActor`] (single SQLite writer — `§10.1`).
pub enum PersistenceMsg {
    /// Persist full `Download` aggregate + optional emitted domain events.
    SaveDownload {
        /// Aggregate snapshot.
        download: Box<Download>,
        /// Emitted events to append to audit trail.
        events: Vec<DomainEventEnvelope>,
        /// Correlation trace ID.
        trace_id: TraceId,
        /// Optional completion acknowledgement.
        ack: Option<oneshot::Sender<AppResult<()>>>,
    },
    /// Step 2 of Durable Checkpoint Protocol (strictly AFTER `FlushFileBuffers`!).
    CommitCheckpoint {
        /// Download ID.
        id: DownloadId,
        /// Durably flushed completed `RangeSet`.
        completed: RangeSet,
        /// Current state snapshot.
        state: DownloadState,
        /// Checkpoint timestamp (ms).
        updated_at_ms: i64,
        /// Optional events to log.
        events: Vec<DomainEventEnvelope>,
        /// Correlation trace ID.
        trace_id: TraceId,
        /// Mandatory ack awaited on `Pause`, `Shutdown`, and `Complete`.
        ack: oneshot::Sender<AppResult<()>>,
    },
}

/// Handle to the single-writer [`PersistenceActor`].
#[derive(Debug, Clone)]
pub struct PersistenceHandle {
    tx: mpsc::Sender<PersistenceMsg>,
}

impl PersistenceHandle {
    /// Spawns [`PersistenceActor`] with a bounded MPSC queue (`capacity = 256`).
    #[must_use]
    pub fn spawn(
        download_repo: Arc<dyn DownloadRepository>,
        event_repo: Arc<dyn EventLogRepository>,
        event_bus: Arc<dyn EventBus>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<PersistenceMsg>(256);
        let join = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    PersistenceMsg::SaveDownload {
                        download,
                        events,
                        trace_id,
                        ack,
                    } => {
                        let res = download_repo.save(&download, trace_id).await;
                        if res.is_ok() && !events.is_empty() {
                            let _ = event_repo.append(&events, trace_id).await;
                            for env in events {
                                event_bus.publish(AppEvent::Domain(env));
                            }
                        }
                        if let Some(reply) = ack {
                            let _ = reply.send(res);
                        }
                    }
                    PersistenceMsg::CommitCheckpoint {
                        id,
                        completed,
                        state,
                        updated_at_ms,
                        events,
                        trace_id,
                        ack,
                    } => {
                        let res = download_repo
                            .checkpoint(id, &completed, &state, updated_at_ms, trace_id)
                            .await;
                        if res.is_ok() && !events.is_empty() {
                            let _ = event_repo.append(&events, trace_id).await;
                            for env in events {
                                event_bus.publish(AppEvent::Domain(env));
                            }
                        }
                        let _ = ack.send(res);
                    }
                }
            }
        });
        (Self { tx }, join)
    }

    /// Saves `download` and publishes `events`, awaiting SQLite commit.
    ///
    /// # Errors
    /// Returns an error if the persistence channel closed or SQLite failed.
    pub async fn save_and_wait(
        &self,
        download: Download,
        events: Vec<DomainEventEnvelope>,
        trace_id: TraceId,
    ) -> AppResult<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(PersistenceMsg::SaveDownload {
                download: Box::new(download),
                events,
                trace_id,
                ack: Some(ack_tx),
            })
            .await
            .map_err(|_| {
                VortexError::coded(
                    ErrorCode::DbCorruptOrIncompatible,
                    trace_id,
                    "PersistenceActor channel closed",
                )
            })?;
        ack_rx.await.map_err(|_| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                "PersistenceActor dropped ack",
            )
        })?
    }

    /// Commits a durable checkpoint to SQLite and awaits ACID completion.
    ///
    /// # Errors
    /// Returns an error if SQLite commit fails.
    pub async fn commit_checkpoint(
        &self,
        id: DownloadId,
        completed: RangeSet,
        state: DownloadState,
        updated_at_ms: i64,
        events: Vec<DomainEventEnvelope>,
        trace_id: TraceId,
    ) -> AppResult<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(PersistenceMsg::CommitCheckpoint {
                id,
                completed,
                state,
                updated_at_ms,
                events,
                trace_id,
                ack: ack_tx,
            })
            .await
            .map_err(|_| {
                VortexError::coded(
                    ErrorCode::DbCorruptOrIncompatible,
                    trace_id,
                    "PersistenceActor channel closed",
                )
            })?;
        ack_rx.await.map_err(|_| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                "PersistenceActor dropped checkpoint ack",
            )
        })?
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct QueuedEntry {
    priority: Priority,
    seq: Reverse<u64>,
    id: DownloadId,
}

impl Ord for QueuedEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| self.seq.cmp(&other.seq))
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for QueuedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority FIFO scheduler (`BinaryHeap<(Priority, Reverse<Seq>)>` — `§9` & `P1-T18`).
#[derive(Debug)]
pub struct PriorityQueueScheduler {
    max_parallel: usize,
    next_seq: u64,
    active_set: HashSet<DownloadId>,
    waiting_heap: BinaryHeap<QueuedEntry>,
    cancelled_waiting: HashSet<DownloadId>,
}

impl PriorityQueueScheduler {
    /// Creates a new [`PriorityQueueScheduler`] with `max_parallel` concurrency slots.
    #[must_use]
    pub fn new(max_parallel: usize) -> Self {
        Self {
            max_parallel: max_parallel.max(1),
            next_seq: 1,
            active_set: HashSet::new(),
            waiting_heap: BinaryHeap::new(),
            cancelled_waiting: HashSet::new(),
        }
    }

    /// Updates `max_parallel` live (Hot-Reload).
    pub fn set_max_parallel(&mut self, max_parallel: usize) {
        self.max_parallel = max_parallel.max(1);
    }

    /// Attempts to acquire an active slot immediately; returns `false` and enqueues in `BinaryHeap` if full.
    pub fn try_acquire_or_enqueue(&mut self, id: DownloadId, priority: Priority) -> bool {
        self.cancelled_waiting.remove(&id);
        if self.active_set.contains(&id) {
            return true;
        }
        if self.active_set.len() < self.max_parallel {
            self.active_set.insert(id);
            true
        } else {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.saturating_add(1);
            self.waiting_heap.push(QueuedEntry {
                priority,
                seq: Reverse(seq),
                id,
            });
            false
        }
    }

    /// Releases `id`'s slot (or removes it from the waiting queue) and returns the next promoted `DownloadId`s.
    pub fn release_and_promote(&mut self, id: DownloadId) -> Vec<DownloadId> {
        if !self.active_set.remove(&id) {
            self.cancelled_waiting.insert(id);
        }
        self.drain_available_promotions()
    }

    /// Promotes waiting downloads while `active_set.len() < max_parallel`.
    pub fn drain_available_promotions(&mut self) -> Vec<DownloadId> {
        let mut promoted = Vec::new();
        while self.active_set.len() < self.max_parallel {
            let Some(next) = self.waiting_heap.pop() else {
                break;
            };
            if self.cancelled_waiting.remove(&next.id) || self.active_set.contains(&next.id) {
                continue;
            }
            self.active_set.insert(next.id);
            promoted.push(next.id);
        }
        promoted
    }

    /// Number of currently active transfers.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active_set.len()
    }
}

/// Builds a [`SidecarMetadata`] snapshot from a [`Download`] and its durably flushed [`RangeSet`].
#[must_use]
pub fn build_sidecar_snapshot(
    download: &Download,
    durable_ranges: RangeSet,
    updated_at_ms: i64,
) -> SidecarMetadata {
    SidecarMetadata {
        version: 1,
        download_id: download.id,
        url: vortex_observability::RedactedUrl::parse(&download.resource.url).to_string(),
        final_url: download
            .resource
            .final_url
            .as_deref()
            .map(|u| vortex_observability::RedactedUrl::parse(u).to_string()),
        file_name: download.file_name.clone(),
        save_dir: download.save_dir.clone(),
        total_size: download.total_size,
        supports_range: download.supports_range,
        validators: download.resource.validators.clone(),
        completed_ranges: durable_ranges,
        expected_hash: download.expected_hash.clone(),
        updated_at_ms,
    }
}

/// Cooperative cancellation helper token wrapper.
#[must_use]
pub fn child_token(parent: &CancellationToken) -> CancellationToken {
    parent.child_token()
}
