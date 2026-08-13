use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use orca_core::cancel::CancelToken;
use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const JOB_SCHEMA_VERSION: u8 = 1;
const JOB_LEASE_MS: i64 = 10 * 60 * 1_000;
const JOB_RETRY_DELAY_MS: i64 = 30 * 1_000;
const JOB_MAX_ATTEMPTS: u32 = 3;
const JOB_RETENTION: usize = 128;
const JOB_MAX_ACTIVE: usize = 64;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoryJobStatus {
    Pending,
    Running,
    Committed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MemoryExtractionJob {
    pub(crate) schema_version: u8,
    pub(crate) job_id: String,
    pub(crate) status: MemoryJobStatus,
    pub(crate) source: String,
    pub(crate) source_digest: String,
    pub(crate) turn_id: String,
    pub(crate) session_id: String,
    pub(crate) extractor_provider: String,
    pub(crate) extractor_model: String,
    pub(crate) extractor_prompt_version: u8,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) attempts: u32,
    pub(crate) next_retry_at_ms: Option<i64>,
    pub(crate) lease_id: Option<String>,
    pub(crate) lease_expires_at_ms: Option<i64>,
    pub(crate) committed_candidates: Option<usize>,
    pub(crate) last_error: Option<String>,
}

pub(crate) struct ClaimedMemoryJob {
    pub(crate) job: MemoryExtractionJob,
    pub(crate) path: PathBuf,
    pub(crate) lease_id: String,
}

pub(crate) struct NewMemoryJob<'a> {
    pub(crate) source: &'a str,
    pub(crate) source_digest: &'a str,
    pub(crate) turn_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) extractor_provider: &'a str,
    pub(crate) extractor_model: &'a str,
    pub(crate) extractor_prompt_version: u8,
}

pub(crate) fn enqueue(
    project_root: &Path,
    new_job: NewMemoryJob<'_>,
    cancel: &CancelToken,
) -> Result<Option<PathBuf>, String> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let jobs_dir = jobs_dir(project_root);
    fs::create_dir_all(&jobs_dir)
        .map_err(|error| format!("failed to create memory jobs directory: {error}"))?;
    let Some(_lock) = super::acquire_memory_lock(&jobs_lock_path(project_root), cancel)? else {
        return Ok(None);
    };
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let job_id = super::sha256_hex(
        format!(
            "{}\0{}\0{}",
            new_job.session_id, new_job.turn_id, new_job.source_digest
        )
        .as_bytes(),
    );
    let path = jobs_dir.join(format!("{job_id}.json"));
    if path.exists() {
        return Ok(Some(path));
    }
    let existing_jobs = read_jobs(&jobs_dir)?;
    let active_jobs = existing_jobs
        .iter()
        .filter(|(_, job)| job.is_active())
        .count();
    if active_jobs >= JOB_MAX_ACTIVE {
        return Err(format!(
            "automatic memory queue is full ({JOB_MAX_ACTIVE} active jobs)"
        ));
    }
    let now = Utc::now().timestamp_millis();
    let job = MemoryExtractionJob {
        schema_version: JOB_SCHEMA_VERSION,
        job_id,
        status: MemoryJobStatus::Pending,
        source: new_job.source.to_string(),
        source_digest: new_job.source_digest.to_string(),
        turn_id: new_job.turn_id.to_string(),
        session_id: new_job.session_id.to_string(),
        extractor_provider: new_job.extractor_provider.to_string(),
        extractor_model: new_job.extractor_model.to_string(),
        extractor_prompt_version: new_job.extractor_prompt_version,
        created_at_ms: now,
        updated_at_ms: now,
        attempts: 0,
        next_retry_at_ms: None,
        lease_id: None,
        lease_expires_at_ms: None,
        committed_candidates: None,
        last_error: None,
    };
    write_job(&path, &job)?;
    prune_committed_jobs(&jobs_dir)?;
    Ok(Some(path))
}

pub(crate) fn claim_next(
    project_root: &Path,
    extractor_provider: &str,
    extractor_model: &str,
    cancel: &CancelToken,
) -> Result<Option<ClaimedMemoryJob>, String> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let jobs_dir = jobs_dir(project_root);
    if !jobs_dir.is_dir() {
        return Ok(None);
    }
    let Some(_lock) = super::acquire_memory_lock(&jobs_lock_path(project_root), cancel)? else {
        return Ok(None);
    };
    let now = Utc::now().timestamp_millis();
    let mut jobs = read_jobs(&jobs_dir)?;
    jobs.sort_by_key(|(_, job)| job.created_at_ms);
    let Some((path, mut job)) = jobs.into_iter().find(|(_, job)| job.is_claimable(now)) else {
        return Ok(None);
    };
    let lease_id = Uuid::now_v7().to_string();
    job.status = MemoryJobStatus::Running;
    job.attempts = job.attempts.saturating_add(1);
    job.updated_at_ms = now;
    job.extractor_provider = extractor_provider.to_string();
    job.extractor_model = extractor_model.to_string();
    job.next_retry_at_ms = None;
    job.lease_id = Some(lease_id.clone());
    job.lease_expires_at_ms = Some(now.saturating_add(JOB_LEASE_MS));
    job.last_error = None;
    write_job(&path, &job)?;
    Ok(Some(ClaimedMemoryJob {
        job,
        path,
        lease_id,
    }))
}

pub(crate) fn next_claim_delay(
    project_root: &Path,
    cancel: &CancelToken,
) -> Result<Option<Duration>, String> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    let jobs_dir = jobs_dir(project_root);
    if !jobs_dir.is_dir() {
        return Ok(None);
    }
    let Some(_lock) = super::acquire_memory_lock(&jobs_lock_path(project_root), cancel)? else {
        return Ok(None);
    };
    let now = Utc::now().timestamp_millis();
    let next_at = read_jobs(&jobs_dir)?
        .into_iter()
        .filter_map(|(_, job)| job.next_claim_at(now))
        .min();
    Ok(next_at.map(|next_at| Duration::from_millis(next_at.saturating_sub(now).max(0) as u64)))
}

pub(crate) fn heartbeat(project_root: &Path, claimed: &ClaimedMemoryJob) -> Result<(), String> {
    update_claimed(project_root, claimed, |job, now| {
        job.updated_at_ms = now;
        job.lease_expires_at_ms = Some(now.saturating_add(JOB_LEASE_MS));
    })
}

#[cfg(test)]
pub(crate) fn commit(
    project_root: &Path,
    claimed: &ClaimedMemoryJob,
    committed_candidates: usize,
) -> Result<(), String> {
    update_claimed(project_root, claimed, |job, now| {
        job.status = MemoryJobStatus::Committed;
        job.updated_at_ms = now;
        job.lease_id = None;
        job.lease_expires_at_ms = None;
        job.committed_candidates = Some(committed_candidates);
        job.last_error = None;
    })
}

pub(crate) fn publish_and_commit(
    project_root: &Path,
    claimed: &ClaimedMemoryJob,
    cancel: &CancelToken,
    publish: impl FnOnce() -> Result<usize, String>,
) -> Result<usize, String> {
    let lock_cancel = CancelToken::new();
    let Some(_lock) = super::acquire_memory_lock(&jobs_lock_path(project_root), &lock_cancel)?
    else {
        return Err("memory job lock acquisition was cancelled".to_string());
    };
    let mut job = read_job(&claimed.path)?;
    let now = Utc::now().timestamp_millis();
    validate_live_claim(&job, claimed, now)?;

    // Renew before entering candidate publication. Holding the jobs lock is
    // the fencing authority; persisting the renewed deadline also prevents a
    // later process from treating this attempt as abandoned after a crash.
    job.updated_at_ms = now;
    job.lease_expires_at_ms = Some(now.saturating_add(JOB_LEASE_MS));
    write_job(&claimed.path, &job)?;

    let committed_candidates = publish()?;
    let now = Utc::now().timestamp_millis();
    if cancel.is_cancelled() && committed_candidates == 0 {
        job.status = MemoryJobStatus::Pending;
        job.attempts = job.attempts.saturating_sub(1);
        job.next_retry_at_ms = None;
        job.committed_candidates = None;
    } else {
        job.status = MemoryJobStatus::Committed;
        job.next_retry_at_ms = None;
        job.committed_candidates = Some(committed_candidates);
    }
    job.updated_at_ms = now;
    job.lease_id = None;
    job.lease_expires_at_ms = None;
    job.last_error = None;
    write_job(&claimed.path, &job)?;
    Ok(committed_candidates)
}

pub(crate) fn fail(
    project_root: &Path,
    claimed: &ClaimedMemoryJob,
    error: &str,
) -> Result<(), String> {
    update_claimed(project_root, claimed, |job, now| {
        job.status = MemoryJobStatus::Failed;
        job.updated_at_ms = now;
        job.next_retry_at_ms = Some(now.saturating_add(JOB_RETRY_DELAY_MS));
        job.lease_id = None;
        job.lease_expires_at_ms = None;
        job.last_error = Some(super::truncate_utf8(error, 1_024).to_string());
    })
}

pub(crate) fn release_cancelled(
    project_root: &Path,
    claimed: &ClaimedMemoryJob,
) -> Result<(), String> {
    update_claimed(project_root, claimed, |job, now| {
        job.status = MemoryJobStatus::Pending;
        job.updated_at_ms = now;
        job.attempts = job.attempts.saturating_sub(1);
        job.next_retry_at_ms = None;
        job.lease_id = None;
        job.lease_expires_at_ms = None;
        job.last_error = None;
    })
}

fn update_claimed(
    project_root: &Path,
    claimed: &ClaimedMemoryJob,
    update: impl FnOnce(&mut MemoryExtractionJob, i64),
) -> Result<(), String> {
    let cancel = CancelToken::new();
    let Some(_lock) = super::acquire_memory_lock(&jobs_lock_path(project_root), &cancel)? else {
        return Err("memory job lock acquisition was cancelled".to_string());
    };
    let mut job = read_job(&claimed.path)?;
    let now = Utc::now().timestamp_millis();
    validate_live_claim(&job, claimed, now)?;
    update(&mut job, now);
    write_job(&claimed.path, &job)
}

fn validate_live_claim(
    job: &MemoryExtractionJob,
    claimed: &ClaimedMemoryJob,
    now: i64,
) -> Result<(), String> {
    if job.status != MemoryJobStatus::Running
        || job.lease_id.as_deref() != Some(claimed.lease_id.as_str())
        || !job
            .lease_expires_at_ms
            .is_some_and(|expires_at| expires_at > now)
    {
        return Err(format!("memory job lease was lost: {}", job.job_id));
    }
    Ok(())
}

impl MemoryExtractionJob {
    fn is_active(&self) -> bool {
        self.status != MemoryJobStatus::Committed && self.attempts < JOB_MAX_ATTEMPTS
    }

    fn is_claimable(&self, now: i64) -> bool {
        self.next_claim_at(now)
            .is_some_and(|next_at| next_at <= now)
    }

    fn next_claim_at(&self, now: i64) -> Option<i64> {
        if self.attempts >= JOB_MAX_ATTEMPTS || self.status == MemoryJobStatus::Committed {
            return None;
        }
        match self.status {
            MemoryJobStatus::Pending => Some(now),
            MemoryJobStatus::Failed => Some(self.next_retry_at_ms.unwrap_or(now)),
            MemoryJobStatus::Running => self.lease_expires_at_ms,
            MemoryJobStatus::Committed => None,
        }
    }
}

fn jobs_dir(project_root: &Path) -> PathBuf {
    project_root.join("jobs")
}

fn jobs_lock_path(project_root: &Path) -> PathBuf {
    jobs_dir(project_root).join("jobs.lock")
}

fn read_jobs(jobs_dir: &Path) -> Result<Vec<(PathBuf, MemoryExtractionJob)>, String> {
    fs::read_dir(jobs_dir)
        .map_err(|error| format!("failed to list memory jobs: {error}"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| read_job(&path).map(|job| (path, job)))
        .collect()
}

fn read_job(path: &Path) -> Result<MemoryExtractionJob, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read memory job {}: {error}", path.display()))?;
    let job: MemoryExtractionJob = serde_json::from_str(&content)
        .map_err(|error| format!("invalid memory job {}: {error}", path.display()))?;
    if job.schema_version != JOB_SCHEMA_VERSION
        || job.job_id.trim().is_empty()
        || job.source.trim().is_empty()
        || job.source_digest.trim().is_empty()
        || job.turn_id.trim().is_empty()
        || job.session_id.trim().is_empty()
        || job.extractor_provider.trim().is_empty()
    {
        return Err(format!("invalid memory job {}", path.display()));
    }
    Ok(job)
}

fn write_job(path: &Path, job: &MemoryExtractionJob) -> Result<(), String> {
    let content = serde_json::to_vec_pretty(job)
        .map_err(|error| format!("failed to serialize memory job: {error}"))?;
    atomic_write(path, &content, AtomicWritePolicy::NoFollow)
        .map_err(|error| format!("failed to publish memory job: {error}"))
}

fn prune_committed_jobs(jobs_dir: &Path) -> Result<(), String> {
    let mut committed = read_jobs(jobs_dir)?
        .into_iter()
        .filter(|(_, job)| job.status == MemoryJobStatus::Committed)
        .collect::<Vec<_>>();
    committed.sort_by_key(|(_, job)| job.updated_at_ms);
    let remove = committed.len().saturating_sub(JOB_RETENTION);
    for (path, _) in committed.into_iter().take(remove) {
        fs::remove_file(&path).map_err(|error| {
            format!(
                "failed to prune committed memory job {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn read_job_for_test(path: &Path) -> Result<MemoryExtractionJob, String> {
    read_job(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn new_job<'a>(turn_id: &'a str, digest: &'a str) -> NewMemoryJob<'a> {
        NewMemoryJob {
            source: "[user]\nRemember a stable project decision.",
            source_digest: digest,
            turn_id,
            session_id: "session-job-capacity",
            extractor_provider: "old-provider",
            extractor_model: "old-model",
            extractor_prompt_version: 2,
        }
    }

    #[test]
    fn exhausted_jobs_do_not_consume_active_queue_capacity() {
        let dir = TempDir::new().unwrap();
        let cancel = CancelToken::new();
        for index in 0..JOB_MAX_ACTIVE {
            let turn_id = format!("turn-{index}");
            let digest = format!("digest-{index}");
            let path = enqueue(dir.path(), new_job(&turn_id, &digest), &cancel)
                .unwrap()
                .expect("job path");
            let mut job = read_job(&path).unwrap();
            job.status = MemoryJobStatus::Failed;
            job.attempts = JOB_MAX_ATTEMPTS;
            write_job(&path, &job).unwrap();
        }

        assert!(
            enqueue(
                dir.path(),
                new_job("turn-after-exhaustion", "digest-after-exhaustion"),
                &cancel,
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn an_existing_job_remains_idempotent_when_the_queue_is_full() {
        let dir = TempDir::new().unwrap();
        let cancel = CancelToken::new();
        let first = enqueue(dir.path(), new_job("turn-0", "digest-0"), &cancel)
            .unwrap()
            .expect("first job");
        for index in 1..JOB_MAX_ACTIVE {
            let turn_id = format!("turn-{index}");
            let digest = format!("digest-{index}");
            enqueue(dir.path(), new_job(&turn_id, &digest), &cancel)
                .unwrap()
                .expect("fill job");
        }

        assert_eq!(
            enqueue(dir.path(), new_job("turn-0", "digest-0"), &cancel).unwrap(),
            Some(first)
        );
    }

    #[test]
    fn failed_job_exposes_its_next_worker_wake_deadline() {
        let dir = TempDir::new().unwrap();
        let cancel = CancelToken::new();
        let path = enqueue(dir.path(), new_job("turn-retry", "digest-retry"), &cancel)
            .unwrap()
            .expect("job path");
        let claimed = claim_next(dir.path(), "current-provider", "current-model", &cancel)
            .unwrap()
            .expect("claim");
        fail(dir.path(), &claimed, "transient provider error").unwrap();

        let delay = next_claim_delay(dir.path(), &cancel)
            .unwrap()
            .expect("retry delay");
        let failed = read_job(&path).unwrap();

        assert_eq!(failed.status, MemoryJobStatus::Failed);
        assert!(delay <= Duration::from_millis(JOB_RETRY_DELAY_MS as u64));
        assert!(delay > Duration::ZERO);
    }

    #[test]
    fn expired_lease_cannot_publish_or_commit_after_ownership_is_lost() {
        let dir = TempDir::new().unwrap();
        let cancel = CancelToken::new();
        let path = enqueue(
            dir.path(),
            new_job("turn-expired", "digest-expired"),
            &cancel,
        )
        .unwrap()
        .expect("job path");
        let claimed = claim_next(dir.path(), "current-provider", "current-model", &cancel)
            .unwrap()
            .expect("claim");
        let mut job = read_job(&path).unwrap();
        job.lease_expires_at_ms = Some(Utc::now().timestamp_millis().saturating_sub(1));
        write_job(&path, &job).unwrap();
        let mut published = false;

        let result = publish_and_commit(dir.path(), &claimed, &cancel, || {
            published = true;
            Ok(1)
        });

        assert!(result.is_err());
        assert!(!published, "stale worker must be fenced before publication");
        let persisted = read_job(&path).unwrap();
        assert_eq!(persisted.status, MemoryJobStatus::Running);
        assert_eq!(
            persisted.lease_id.as_deref(),
            Some(claimed.lease_id.as_str())
        );
    }
}
