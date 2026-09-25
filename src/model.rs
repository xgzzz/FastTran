use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, oneshot};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferDirection {
    Sending,
    Receiving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferState {
    Queued,
    Hashing,
    WaitingForReceiver,
    AwaitingConfirmation,
    Sending,
    Receiving,
    Completed,
    Cancelled,
    Failed,
}

impl TransferState {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Hashing
                | Self::WaitingForReceiver
                | Self::AwaitingConfirmation
                | Self::Sending
                | Self::Receiving
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "等待中",
            Self::Hashing => "校验准备中",
            Self::WaitingForReceiver => "等待对方确认",
            Self::AwaitingConfirmation => "等待确认接收",
            Self::Sending => "正在发送",
            Self::Receiving => "正在接收",
            Self::Completed => "已完成",
            Self::Cancelled => "已取消",
            Self::Failed => "失败",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationDecision {
    Accept,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSnapshot {
    pub id: Uuid,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub peer_name: String,
    pub peer_address: String,
    pub file_name: String,
    pub file_path: PathBuf,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub progress: f32,
    pub speed_bps: f64,
    pub elapsed: Duration,
    pub error: Option<String>,
    pub created_at: SystemTime,
    pub finished_at: Option<SystemTime>,
}

#[derive(Debug)]
pub struct TransferJob {
    pub id: Uuid,
    pub direction: TransferDirection,
    pub peer_name: String,
    pub peer_address: String,
    pub file_name: String,
    pub file_path: PathBuf,
    total_bytes: RwLock<u64>,
    pub created_at: SystemTime,
    state: RwLock<TransferState>,
    error: RwLock<Option<String>>,
    transferred_bytes: RwLock<u64>,
    started_at: Instant,
    finished_at: RwLock<Option<SystemTime>>,
    finished_instant: RwLock<Option<Instant>>,
    cancel_requested: AtomicBool,
    cancel_notify: Notify,
}

impl TransferJob {
    pub fn sending(file: &Path, peer_name: String, peer_address: String, total_bytes: u64) -> Self {
        Self::new(
            Uuid::new_v4(),
            TransferDirection::Sending,
            peer_name,
            peer_address,
            file.to_path_buf(),
            total_bytes,
        )
    }

    pub fn receiving(
        id: Uuid,
        peer_name: String,
        peer_address: String,
        file_path: PathBuf,
        total_bytes: u64,
    ) -> Self {
        Self::new(
            id,
            TransferDirection::Receiving,
            peer_name,
            peer_address,
            file_path,
            total_bytes,
        )
    }

    fn new(
        id: Uuid,
        direction: TransferDirection,
        peer_name: String,
        peer_address: String,
        file_path: PathBuf,
        total_bytes: u64,
    ) -> Self {
        let file_name = file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名文件".to_owned());

        Self {
            id,
            direction,
            peer_name,
            peer_address,
            file_name,
            file_path,
            total_bytes: RwLock::new(total_bytes),
            created_at: SystemTime::now(),
            state: RwLock::new(TransferState::Queued),
            error: RwLock::new(None),
            transferred_bytes: RwLock::new(0),
            started_at: Instant::now(),
            finished_at: RwLock::new(None),
            finished_instant: RwLock::new(None),
            cancel_requested: AtomicBool::new(false),
            cancel_notify: Notify::new(),
        }
    }

    pub fn set_state(&self, state: TransferState) {
        if self.state().is_finished() || self.is_cancel_requested() {
            return;
        }
        *self
            .state
            .write()
            .unwrap_or_else(|error| error.into_inner()) = state;
    }

    pub fn set_total_bytes(&self, total_bytes: u64) {
        if self.state().is_finished() {
            return;
        }
        *self
            .total_bytes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = total_bytes;
    }

    pub fn snapshot(&self) -> TransferSnapshot {
        let transferred_bytes = *self
            .transferred_bytes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let total_bytes = *self
            .total_bytes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let progress = if total_bytes == 0 {
            if self.state() == TransferState::Completed {
                1.0
            } else {
                0.0
            }
        } else {
            (transferred_bytes as f64 / total_bytes as f64).clamp(0.0, 1.0) as f32
        };
        let state = self.state();
        let finished_instant = *self
            .finished_instant
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let elapsed = finished_instant.map_or_else(
            || self.started_at.elapsed(),
            |finished| finished.saturating_duration_since(self.started_at),
        );

        TransferSnapshot {
            id: self.id,
            direction: self.direction,
            state,
            peer_name: self.peer_name.clone(),
            peer_address: self.peer_address.clone(),
            file_name: self.file_name.clone(),
            file_path: self.file_path.clone(),
            total_bytes,
            transferred_bytes,
            progress,
            speed_bps: if elapsed.as_secs_f64() > 0.0 {
                transferred_bytes as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            },
            elapsed,
            error: self
                .error
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            created_at: self.created_at,
            finished_at: *self
                .finished_at
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }

    pub fn add_bytes(&self, amount: u64) {
        let total_bytes = *self
            .total_bytes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut transferred = self
            .transferred_bytes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *transferred = (*transferred).saturating_add(amount).min(total_bytes);
    }

    pub fn is_cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Relaxed)
    }

    pub async fn wait_for_cancel(&self) {
        if self.is_cancel_requested() {
            return;
        }
        self.cancel_notify.notified().await;
    }

    pub fn request_cancel(&self) {
        if !self.state().is_finished() {
            self.cancel_requested.store(true, Ordering::Relaxed);
            self.cancel_notify.notify_one();
        }
    }

    pub fn finish(&self) -> bool {
        if self.state().is_finished() {
            return false;
        }
        if self.is_cancel_requested() {
            self.fail("传输已取消");
            return false;
        }
        *self
            .state
            .write()
            .unwrap_or_else(|error| error.into_inner()) = TransferState::Completed;
        let total_bytes = *self
            .total_bytes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *self
            .transferred_bytes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = total_bytes;
        let now = SystemTime::now();
        *self
            .finished_at
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(now);
        *self
            .finished_instant
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        true
    }

    pub fn fail(&self, error: impl Into<String>) {
        if self.state().is_finished() {
            return;
        }
        let state = if self.is_cancel_requested() {
            TransferState::Cancelled
        } else {
            TransferState::Failed
        };
        *self
            .state
            .write()
            .unwrap_or_else(|error| error.into_inner()) = state;
        if state == TransferState::Failed {
            *self
                .error
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.into());
        }
        *self
            .finished_at
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SystemTime::now());
        *self
            .finished_instant
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    }

    pub fn state(&self) -> TransferState {
        *self.state.read().unwrap_or_else(|error| error.into_inner())
    }
}

pub struct TransferHub {
    jobs: RwLock<Vec<Arc<TransferJob>>>,
    pending_receives: Mutex<HashMap<Uuid, oneshot::Sender<ConfirmationDecision>>>,
}

impl Default for TransferHub {
    fn default() -> Self {
        Self {
            jobs: RwLock::new(Vec::new()),
            pending_receives: Mutex::new(HashMap::new()),
        }
    }
}

impl TransferHub {
    pub fn register(&self, job: Arc<TransferJob>) {
        let mut jobs = self.jobs.write().unwrap_or_else(|error| error.into_inner());
        jobs.retain(|existing| existing.id != job.id);
        jobs.push(job);

        if jobs.len() > 500 {
            let excess = jobs.len() - 500;
            let mut removed = 0;
            jobs.retain(|job| {
                if removed < excess && job.state().is_finished() {
                    removed += 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn get(&self, id: Uuid) -> Option<Arc<TransferJob>> {
        self.jobs
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .find(|job| job.id == id)
            .cloned()
    }

    pub fn contains(&self, id: Uuid) -> bool {
        self.get(id).is_some()
    }

    pub fn snapshots(&self) -> Vec<TransferSnapshot> {
        self.jobs
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .rev()
            .map(|job| job.snapshot())
            .collect()
    }

    pub fn register_pending_receive(
        &self,
        id: Uuid,
        sender: oneshot::Sender<ConfirmationDecision>,
    ) {
        self.pending_receives
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, sender);
    }

    pub fn discard_pending_receive(&self, id: Uuid) {
        self.pending_receives
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id);
    }

    pub fn accept_incoming(&self, id: Uuid) -> bool {
        self.send_receive_decision(id, ConfirmationDecision::Accept)
    }

    pub fn reject_incoming(&self, id: Uuid) -> bool {
        if let Some(job) = self.get(id) {
            job.request_cancel();
        }
        self.send_receive_decision(id, ConfirmationDecision::Reject)
    }

    fn send_receive_decision(&self, id: Uuid, decision: ConfirmationDecision) -> bool {
        self.pending_receives
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id)
            .is_some_and(|sender| sender.send(decision).is_ok())
    }

    pub fn request_cancel(&self, id: Uuid) -> bool {
        let Some(job) = self.get(id) else {
            return false;
        };
        job.request_cancel();
        if job.state() == TransferState::AwaitingConfirmation {
            let _ = self.send_receive_decision(id, ConfirmationDecision::Reject);
        }
        true
    }

    pub fn cancel_receiving(&self) {
        let receiving_ids = self
            .jobs
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|job| job.direction == TransferDirection::Receiving && job.state().is_active())
            .map(|job| job.id)
            .collect::<Vec<_>>();
        for id in receiving_ids {
            self.request_cancel(id);
        }
    }

    pub fn remove(&self, id: Uuid) {
        self.pending_receives
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id);
        self.jobs
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|job| job.id != id);
    }

    pub fn clear_finished(&self) {
        self.jobs
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|job| !job.state().is_finished());
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::{TransferHub, TransferState};

    #[test]
    fn cancellation_is_visible_to_worker() {
        let hub = TransferHub::default();
        let job = super::TransferJob::sending(
            Path::new("file.bin"),
            "Peer".into(),
            "127.0.0.1:1234".into(),
            10,
        );
        let id = job.id;
        hub.register(job.into());

        assert!(hub.request_cancel(id));
        let job = hub.get(id).unwrap();
        assert!(job.is_cancel_requested());
    }

    #[test]
    fn stopping_services_only_cancels_receiving_jobs() {
        let hub = TransferHub::default();
        let sending = std::sync::Arc::new(super::TransferJob::sending(
            Path::new("send.bin"),
            "Peer".into(),
            "127.0.0.1:1234".into(),
            10,
        ));
        let receiving = std::sync::Arc::new(super::TransferJob::receiving(
            uuid::Uuid::new_v4(),
            "Peer".into(),
            "127.0.0.1:1234".into(),
            Path::new("received.bin").to_path_buf(),
            10,
        ));
        hub.register(sending.clone());
        hub.register(receiving.clone());

        hub.cancel_receiving();

        assert!(!sending.is_cancel_requested());
        assert!(receiving.is_cancel_requested());
    }

    #[tokio::test]
    async fn cancellation_wakes_waiting_worker() {
        use std::sync::Arc;

        let job = Arc::new(super::TransferJob::sending(
            Path::new("file.bin"),
            "Peer".into(),
            "127.0.0.1:1234".into(),
            10,
        ));
        let waiting_job = job.clone();
        let waiter = tokio::spawn(async move {
            waiting_job.wait_for_cancel().await;
        });
        tokio::task::yield_now().await;
        job.request_cancel();

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancellation notification timed out")
            .unwrap();
    }

    #[test]
    fn completed_job_stops_counting_time() {
        let hub = TransferHub::default();
        let job = super::TransferJob::sending(Path::new("a"), "A".into(), "A".into(), 1);
        let id = job.id;
        hub.register(job.into());
        hub.get(id).unwrap().finish();
        let first = hub.get(id).unwrap().snapshot();
        std::thread::sleep(Duration::from_millis(20));
        let second = hub.get(id).unwrap().snapshot();
        assert_eq!(first.elapsed, second.elapsed);
        assert_eq!(second.state, TransferState::Completed);
    }

    #[test]
    fn snapshots_are_newest_first() {
        let hub = TransferHub::default();
        let first = super::TransferJob::sending(Path::new("a"), "A".into(), "A".into(), 1);
        let first_id = first.id;
        let second = super::TransferJob::sending(Path::new("b"), "B".into(), "B".into(), 1);
        let second_id = second.id;
        hub.register(first.into());
        hub.register(second.into());

        let snapshots = hub.snapshots();
        assert_eq!(snapshots[0].id, second_id);
        assert_eq!(snapshots[1].id, first_id);
        assert_eq!(snapshots[1].state, TransferState::Queued);
    }
}
