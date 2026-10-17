//! Download job management.
//!
//! Every requested track becomes a job with an explicit lifecycle:
//!
//! ```text
//! Queued → Resolving → Downloading → Processing → Organizing → Completed
//!                 └→ Skipped (already in the library)
//! any non-terminal state → Failed | Cancelled
//! ```
//!
//! A job either downloads a track or converts a library file to another
//! format; both share the lifecycle (a conversion skips the resolving and
//! downloading states), the concurrency limit and duplicate handling.
//!
//! A job belongs to the users who requested it. Requesting a track that is
//! already queued adds the user to that job instead of downloading twice,
//! and when the job finishes every requester becomes an owner of the
//! library file. A pirate cancelling a shared job only withdraws from it;
//! the download stops once nobody wants it (or a captain stops it).
//!
//! Jobs wait on a semaphore so only `max_concurrent_downloads` run at once,
//! and the number of unfinished jobs is capped so a huge playlist cannot
//! exhaust memory. Job state lives in memory; it is not persisted across
//! restarts (files already in the library are, via the library index).

mod pipeline;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::UserId;
use crate::domain::DownloadProgress;
use crate::domain::length::TrackLengthLimit;
use crate::domain::metadata::TrackMetadata;
use crate::domain::provider::{ProviderKind, TrackKey};
use crate::domain::quality::{AudioFormat, OutputFormat, QualityReport};
use crate::library::{Library, LibraryEntry};
use crate::providers::{MusicProvider, TrackSummary};
use crate::tools::ffmpeg::Ffmpeg;

pub type JobId = u64;

/// Finished jobs kept for display before the oldest are dropped.
const MAX_FINISHED_JOBS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Resolving,
    Downloading,
    Processing,
    Organizing,
    Completed,
    Skipped,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Skipped | JobState::Failed | JobState::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobError {
    /// The stage the job was in when it failed.
    pub stage: JobState,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Download,
    /// Re-encode a file already in the library; nothing is downloaded.
    Convert,
}

/// Everything the UI shows about a job.
#[derive(Debug, Clone, Serialize)]
pub struct JobSnapshot {
    pub id: JobId,
    pub kind: JobKind,
    /// Library identity of the track, once known.
    pub track_key: Option<TrackKey>,
    pub source_url: String,
    pub provider: ProviderKind,
    /// The file format the user asked for.
    pub output: OutputFormat,
    pub state: JobState,
    pub title: String,
    pub artists: Vec<String>,
    pub thumbnail_url: Option<String>,
    /// Normalized metadata once resolved (description omitted for size).
    pub metadata: Option<TrackMetadata>,
    pub progress: Option<DownloadProgress>,
    /// Download completion in `0.0..=1.0` when the size is known.
    pub progress_fraction: Option<f64>,
    pub source_format: Option<AudioFormat>,
    pub source_format_summary: Option<String>,
    pub quality: Option<QualityReport>,
    /// Library-relative path of the resulting (or existing) file.
    pub destination: Option<String>,
    pub artwork_embedded: Option<bool>,
    pub note: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<JobError>,
    pub retries: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Users who asked for this track; never sent as-is to the UI.
    #[serde(skip)]
    pub requesters: BTreeSet<UserId>,
}

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Exponential backoff: base, 2×base, 4×base, … capped at `max_delay`.
    pub fn delay(&self, retry: u32) -> Duration {
        let factor = 2u32.saturating_pow(retry.saturating_sub(1));
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// Shared services the pipeline needs.
pub struct PipelineContext {
    pub library: Arc<Library>,
    pub ffmpeg: Arc<Ffmpeg>,
    pub staging_dir: PathBuf,
    pub retry: RetryPolicy,
    pub max_track_length: TrackLengthLimit,
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("download queue is full ({0} unfinished jobs); wait for some to finish")]
    QueueFull(usize),
}

struct JobEntry {
    snapshot: JobSnapshot,
    cancel: CancellationToken,
}

struct Inner {
    jobs: Mutex<BTreeMap<JobId, JobEntry>>,
    in_flight: Mutex<HashSet<(TrackKey, OutputFormat)>>,
    /// Signalled whenever a track leaves `in_flight`.
    released: Notify,
    next_id: AtomicU64,
    download_slots: Arc<Semaphore>,
    max_queued: usize,
    ctx: PipelineContext,
}

#[derive(Clone)]
pub struct JobManager {
    inner: Arc<Inner>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Lock a mutex, recovering the data if a panicking thread poisoned it: job
/// bookkeeping stays usable and each update is self-contained.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl JobManager {
    pub fn new(ctx: PipelineContext, max_concurrent: usize, max_queued: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                jobs: Mutex::new(BTreeMap::new()),
                in_flight: Mutex::new(HashSet::new()),
                released: Notify::new(),
                next_id: AtomicU64::new(1),
                download_slots: Arc::new(Semaphore::new(max_concurrent)),
                max_queued,
                ctx,
            }),
        }
    }

    /// Queue a single-track URL for `requester`. If an unfinished job for
    /// the same URL and format exists, the requester joins it and its id is
    /// returned instead of creating a duplicate.
    pub fn enqueue(
        &self,
        provider: Arc<dyn MusicProvider>,
        url: Url,
        hint: Option<&TrackSummary>,
        output: OutputFormat,
        requester: UserId,
    ) -> Result<JobId, EnqueueError> {
        let display = Display {
            title: hint
                .map(|h| h.title.clone())
                .unwrap_or_else(|| url.to_string()),
            artists: hint.map(|h| h.artists.clone()).unwrap_or_default(),
            thumbnail_url: hint.and_then(|h| h.thumbnail_url.clone()),
        };
        let work = Work::Download {
            request: pipeline::JobRequest {
                url,
                track_number_hint: hint.and_then(|h| h.track_number),
                output,
            },
            provider,
        };
        self.submit(work, display, requester)
    }

    /// Queue converting a library track to `output` for `requester`. Joins
    /// an unfinished job producing the same track in the same format, so a
    /// conversion and a download of the same result never both run.
    pub fn enqueue_conversion(
        &self,
        source: &LibraryEntry,
        output: OutputFormat,
        requester: UserId,
    ) -> Result<JobId, EnqueueError> {
        let display = Display {
            title: source.title.clone(),
            artists: source.artists.clone(),
            thumbnail_url: source.artwork_url.clone(),
        };
        let work = Work::Convert(pipeline::ConversionRequest {
            key: source.key.clone(),
            source_url: source.source_url.clone(),
            output,
        });
        self.submit(work, display, requester)
    }

    fn submit(
        &self,
        work: Work,
        display: Display,
        requester: UserId,
    ) -> Result<JobId, EnqueueError> {
        let (kind, source_url, provider, output, track_key) = match &work {
            Work::Download { request, provider } => (
                JobKind::Download,
                request.url.to_string(),
                provider.kind(),
                request.output,
                None,
            ),
            Work::Convert(request) => (
                JobKind::Convert,
                request.source_url.clone(),
                request.key.provider,
                request.output,
                Some(request.key.clone()),
            ),
        };
        let cancel = CancellationToken::new();
        let id = {
            let mut jobs = lock(&self.inner.jobs);
            if let Some(existing) = jobs.values_mut().find(|j| {
                !j.snapshot.state.is_terminal()
                    && j.snapshot.output == output
                    && (j.snapshot.source_url == source_url
                        || (track_key.is_some() && j.snapshot.track_key == track_key))
            }) {
                existing.snapshot.requesters.insert(requester);
                return Ok(existing.snapshot.id);
            }
            let unfinished = jobs
                .values()
                .filter(|j| !j.snapshot.state.is_terminal())
                .count();
            if unfinished >= self.inner.max_queued {
                return Err(EnqueueError::QueueFull(unfinished));
            }

            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            let now = now_ms();
            let snapshot = JobSnapshot {
                id,
                kind,
                track_key,
                source_url,
                provider,
                output,
                state: JobState::Queued,
                title: display.title,
                artists: display.artists,
                thumbnail_url: display.thumbnail_url,
                metadata: None,
                progress: None,
                progress_fraction: None,
                source_format: None,
                source_format_summary: None,
                quality: None,
                destination: None,
                artwork_embedded: None,
                note: None,
                warnings: Vec::new(),
                error: None,
                retries: 0,
                created_at_ms: now,
                updated_at_ms: now,
                requesters: BTreeSet::from([requester]),
            };
            jobs.insert(
                id,
                JobEntry {
                    snapshot,
                    cancel: cancel.clone(),
                },
            );
            prune_finished(&mut jobs);
            id
        };

        let handle = JobHandle {
            id,
            inner: Arc::clone(&self.inner),
        };
        let slots = Arc::clone(&self.inner.download_slots);
        tokio::spawn(async move {
            // Conversions share the download slots: they are CPU and disk
            // heavy, and the same limit keeps a small server responsive.
            let permit = tokio::select! {
                permit = slots.acquire_owned() => permit.ok(),
                _ = cancel.cancelled() => None,
            };
            let result = match (permit, work) {
                (Some(_permit), Work::Download { request, provider }) => {
                    pipeline::run(&handle, provider, request, &cancel).await
                }
                (Some(_permit), Work::Convert(request)) => {
                    pipeline::convert_existing(&handle, request, &cancel).await
                }
                (None, _) => Err(pipeline::PipelineError::Cancelled),
            };
            if let Some((key, output)) = handle.finish(result, &cancel) {
                // Requesters may have joined after the file was imported;
                // the job is terminal now, so this set is final.
                let owners = handle.requesters();
                handle.ctx().library.add_owners(&key, output, &owners).await;
            }
        });
        tracing::info!(job = id, ?kind, "job queued");
        Ok(id)
    }

    /// Jobs visible to `viewer` (all jobs for `Viewer::All`), newest first.
    pub fn list(&self, viewer: Viewer) -> Vec<JobSnapshot> {
        lock(&self.inner.jobs)
            .values()
            .rev()
            .filter(|j| viewer.sees(&j.snapshot))
            .map(|j| j.snapshot.clone())
            .collect()
    }

    /// Stop a job on behalf of `viewer`. A user who shares the job with
    /// others only withdraws from it; it is really cancelled once nobody
    /// is left, or immediately for `Viewer::All` (a captain). Returns false
    /// if the job does not exist, is not visible, or has already finished.
    pub fn cancel(&self, id: JobId, viewer: Viewer) -> bool {
        let mut jobs = lock(&self.inner.jobs);
        let Some(job) = jobs.get_mut(&id) else {
            return false;
        };
        if job.snapshot.state.is_terminal() || !viewer.sees(&job.snapshot) {
            return false;
        }
        if let Viewer::User(user) = viewer {
            job.snapshot.requesters.remove(&user);
        }
        if matches!(viewer, Viewer::All) || job.snapshot.requesters.is_empty() {
            job.cancel.cancel();
        }
        true
    }

    pub fn cancel_all(&self) {
        for job in lock(&self.inner.jobs).values() {
            job.cancel.cancel();
        }
    }

    /// Hide finished jobs from `viewer` (from everyone for `Viewer::All`).
    /// Returns how many were cleared from the viewer's list.
    pub fn clear_finished(&self, viewer: Viewer) -> usize {
        let mut jobs = lock(&self.inner.jobs);
        let mut cleared = 0;
        jobs.retain(|_, j| {
            if !j.snapshot.state.is_terminal() || !viewer.sees(&j.snapshot) {
                return true;
            }
            cleared += 1;
            match viewer {
                Viewer::All => false,
                Viewer::User(user) => {
                    j.snapshot.requesters.remove(&user);
                    !j.snapshot.requesters.is_empty()
                }
            }
        });
        cleared
    }

    /// Forget a deleted user's requests; jobs nobody else wants stop.
    pub fn forget_user(&self, user: UserId) {
        let mut jobs = lock(&self.inner.jobs);
        jobs.retain(|_, j| {
            if !j.snapshot.requesters.remove(&user) || !j.snapshot.requesters.is_empty() {
                return true;
            }
            if j.snapshot.state.is_terminal() {
                return false;
            }
            j.cancel.cancel();
            true
        });
    }
}

/// Whose jobs an operation applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewer {
    /// A captain: every job.
    All,
    /// A pirate (or a captain acting as themselves): jobs they requested.
    User(UserId),
}

impl Viewer {
    fn sees(self, job: &JobSnapshot) -> bool {
        match self {
            Viewer::All => true,
            Viewer::User(user) => job.requesters.contains(&user),
        }
    }
}

/// What a job shows before its metadata is known.
struct Display {
    title: String,
    artists: Vec<String>,
    thumbnail_url: Option<String>,
}

enum Work {
    Download {
        request: pipeline::JobRequest,
        provider: Arc<dyn MusicProvider>,
    },
    Convert(pipeline::ConversionRequest),
}

fn prune_finished(jobs: &mut BTreeMap<JobId, JobEntry>) {
    let finished: Vec<JobId> = jobs
        .iter()
        .filter(|(_, j)| j.snapshot.state.is_terminal())
        .map(|(id, _)| *id)
        .collect();
    if let Some(excess) = finished.len().checked_sub(MAX_FINISHED_JOBS) {
        for id in finished.into_iter().take(excess) {
            jobs.remove(&id);
        }
    }
}

/// A running job's view of the manager.
pub(crate) struct JobHandle {
    id: JobId,
    inner: Arc<Inner>,
}

/// Removes a track identity from the in-flight set when dropped.
pub(crate) struct InFlightGuard {
    key: (TrackKey, OutputFormat),
    inner: Arc<Inner>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        lock(&self.inner.in_flight).remove(&self.key);
        self.inner.released.notify_waiters();
    }
}

impl JobHandle {
    pub fn id(&self) -> JobId {
        self.id
    }

    pub fn ctx(&self) -> &PipelineContext {
        &self.inner.ctx
    }

    pub fn update(&self, f: impl FnOnce(&mut JobSnapshot)) {
        if let Some(job) = lock(&self.inner.jobs).get_mut(&self.id) {
            f(&mut job.snapshot);
            job.snapshot.updated_at_ms = now_ms();
        }
    }

    pub fn set_state(&self, state: JobState) {
        tracing::debug!(job = self.id, ?state, "job state");
        self.update(|s| {
            s.state = state;
            s.note = None;
        });
    }

    pub fn state(&self) -> JobState {
        lock(&self.inner.jobs)
            .get(&self.id)
            .map(|j| j.snapshot.state)
            .unwrap_or(JobState::Cancelled)
    }

    /// Users currently wanting this job's result.
    pub fn requesters(&self) -> BTreeSet<UserId> {
        lock(&self.inner.jobs)
            .get(&self.id)
            .map(|j| j.snapshot.requesters.clone())
            .unwrap_or_default()
    }

    /// Claim a track, waiting while another job processes it (e.g. the same
    /// track queued through a different URL). Waiting rather than skipping
    /// means this job's requesters still get the file once it is there.
    pub async fn claim_or_wait(
        &self,
        key: (TrackKey, OutputFormat),
        cancel: &CancellationToken,
    ) -> Option<InFlightGuard> {
        let mut noted = false;
        loop {
            // Register for the wake-up before trying, so a release between
            // the attempt and the wait is not missed.
            let released = self.inner.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if let Some(guard) = self.claim(key.clone()) {
                if noted {
                    self.update(|s| s.note = None);
                }
                return Some(guard);
            }
            if !noted {
                self.update(|s| {
                    s.note = Some("Waiting for another download of this track to finish".into())
                });
                noted = true;
            }
            tokio::select! {
                _ = released => {}
                _ = cancel.cancelled() => return None,
            }
        }
    }

    /// Mark a track as being processed; `None` if another job already is.
    fn claim(&self, key: (TrackKey, OutputFormat)) -> Option<InFlightGuard> {
        let mut in_flight = lock(&self.inner.in_flight);
        if !in_flight.insert(key.clone()) {
            return None;
        }
        Some(InFlightGuard {
            key,
            inner: Arc::clone(&self.inner),
        })
    }

    /// Record the outcome. Returns the identity of the library file the
    /// requesters should own, if the job produced or found one.
    fn finish(
        &self,
        result: Result<pipeline::Outcome, pipeline::PipelineError>,
        cancel: &CancellationToken,
    ) -> Option<(TrackKey, OutputFormat)> {
        let stage = self.state();
        match result {
            Ok(pipeline::Outcome::Completed(entry)) => {
                let grant = (entry.key.clone(), entry.output);
                tracing::info!(job = self.id, path = %entry.path.display(), "job completed");
                self.update(|s| {
                    s.state = JobState::Completed;
                    s.progress = None;
                    s.note = None;
                    s.destination = Some(entry.path.display().to_string());
                });
                Some(grant)
            }
            Ok(pipeline::Outcome::Skipped { reason, existing }) => {
                tracing::info!(job = self.id, %reason, "job skipped");
                let grant = (existing.key.clone(), existing.output);
                self.update(|s| {
                    s.state = JobState::Skipped;
                    s.note = Some(reason);
                    s.destination = Some(existing.path.display().to_string());
                    s.quality = Some(existing.quality);
                });
                Some(grant)
            }
            Err(_) if cancel.is_cancelled() => {
                tracing::info!(job = self.id, "job cancelled");
                self.update(|s| {
                    s.state = JobState::Cancelled;
                    s.progress = None;
                    s.note = None;
                });
                None
            }
            Err(error) => {
                tracing::error!(job = self.id, ?stage, error = %error, "job failed");
                self.update(|s| {
                    s.state = JobState::Failed;
                    s.note = None;
                    s.error = Some(JobError {
                        stage,
                        retryable: error.is_retryable(),
                        message: error.to_string(),
                    });
                });
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(10),
        };
        assert_eq!(policy.delay(1), Duration::from_secs(2));
        assert_eq!(policy.delay(2), Duration::from_secs(4));
        assert_eq!(policy.delay(3), Duration::from_secs(8));
        assert_eq!(policy.delay(4), Duration::from_secs(10));
        assert_eq!(policy.delay(40), Duration::from_secs(10));
    }

    #[test]
    fn terminal_states() {
        assert!(JobState::Completed.is_terminal());
        assert!(JobState::Skipped.is_terminal());
        assert!(!JobState::Downloading.is_terminal());
    }
}
