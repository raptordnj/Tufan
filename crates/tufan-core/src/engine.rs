//! Job orchestration: one scanner thread, a bounded queue, N workers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::control::{Control, Progress, Snapshot};
use crate::error::{Error, Result};
use crate::faults::{self, Stage};
use crate::filesystem::{self as fsx, SourceId};
use crate::journal::{path_key, ItemRecord, ItemState, JobState, Journal};
use crate::policy::{JobKind, JobSpec, LinkPolicy};
use crate::scanner::{self, Item};
use crate::transfer::{self, Ctx, Outcome};

const MAX_REPORTED_ERRORS: usize = 1000;

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub errors_truncated: bool,
    pub metadata_not_preserved: u64,
}

struct Shared {
    id: i64,
    spec: JobSpec,
    journal: Arc<Journal>,
    control: Control,
    progress: Progress,
    state: Mutex<JobState>,
    report: Mutex<Report>,
    /// Fresh jobs have no journal items to consult.
    resumed: bool,
}

/// Handle to a running job. Dropping it does not stop the job.
pub struct JobHandle {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl JobHandle {
    pub fn id(&self) -> i64 {
        self.shared.id
    }
    pub fn spec(&self) -> &JobSpec {
        &self.shared.spec
    }
    pub fn snapshot(&self) -> Snapshot {
        self.shared.progress.snapshot()
    }
    pub fn state(&self) -> JobState {
        *self.shared.state.lock().unwrap()
    }
    pub fn report(&self) -> Report {
        self.shared.report.lock().unwrap().clone()
    }
    pub fn pause(&self) {
        let mut st = self.shared.state.lock().unwrap();
        if *st == JobState::Running {
            self.shared.control.pause();
            *st = JobState::Paused;
            let _ = self
                .shared
                .journal
                .set_job_state(self.shared.id, JobState::Paused);
        }
    }
    pub fn resume(&self) {
        let mut st = self.shared.state.lock().unwrap();
        if *st == JobState::Paused {
            self.shared.control.resume();
            *st = JobState::Running;
            let _ = self
                .shared
                .journal
                .set_job_state(self.shared.id, JobState::Running);
        }
    }
    pub fn cancel(&self) {
        self.shared.control.cancel();
    }
    pub fn is_finished(&self) -> bool {
        self.thread
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|t| t.is_finished())
    }
    /// Wait for completion and return the final state.
    pub fn wait(&self) -> JobState {
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
        self.state()
    }
}

/// Start a new job.
pub fn start(journal: Arc<Journal>, spec: JobSpec) -> Result<JobHandle> {
    validate(&spec)?;
    let id = journal.create_job(&spec)?;
    Ok(launch(journal, id, spec, false))
}

/// Resume a job recorded in the journal (after pause+exit, crash, or to
/// retry failed items).
pub fn resume(journal: Arc<Journal>, id: i64) -> Result<JobHandle> {
    let row = journal
        .load_job(id)?
        .ok_or_else(|| Error::Corrupt(format!("no job {id}")))?;
    validate(&row.spec)?;
    Ok(launch(journal, id, row.spec, true))
}

fn validate(spec: &JobSpec) -> Result<()> {
    if spec.sources.is_empty() {
        return Err(Error::InvalidPath(PathBuf::new()));
    }
    let md = fs::metadata(&spec.dest_dir).map_err(|e| Error::io(&spec.dest_dir, e))?;
    if !md.is_dir() {
        return Err(Error::InvalidPath(spec.dest_dir.clone()));
    }
    for src in &spec.sources {
        let Some(name) = src.file_name() else {
            return Err(Error::InvalidPath(src.clone()));
        };
        let is_dir = fs::symlink_metadata(src)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir && fsx::is_within(&spec.dest_dir.join(name), src) {
            return Err(Error::DestinationInsideSource {
                source: src.clone(),
                dest: spec.dest_dir.clone(),
            });
        }
    }
    Ok(())
}

fn launch(journal: Arc<Journal>, id: i64, spec: JobSpec, resumed: bool) -> JobHandle {
    let shared = Arc::new(Shared {
        id,
        spec,
        journal,
        control: Control::new(),
        progress: Progress::default(),
        state: Mutex::new(JobState::Running),
        report: Mutex::new(Report::default()),
        resumed,
    });
    let s = shared.clone();
    let t = thread::Builder::new()
        .name(format!("tufan-job-{id}"))
        .spawn(move || run(s))
        .expect("spawn job thread");
    JobHandle {
        shared,
        thread: Mutex::new(Some(t)),
    }
}

fn run(s: Arc<Shared>) {
    let _ = s.journal.set_job_state(s.id, JobState::Running);
    let final_state = match run_inner(&s) {
        Ok(()) => {
            let p = s.progress.snapshot();
            if s.control.is_cancelled() {
                JobState::Cancelled
            } else if p.files_failed == 0 {
                JobState::Completed
            } else if p.files_done + p.files_skipped > 0 {
                JobState::PartiallyCompleted
            } else {
                JobState::Failed
            }
        }
        Err(Error::Cancelled) => JobState::Cancelled,
        Err(e) => {
            record_error(&s, e.to_string());
            JobState::Failed
        }
    };
    let _ = s.journal.flush();
    let _ = s.journal.set_job_state(s.id, final_state);
    *s.state.lock().unwrap() = final_state;
}

fn run_inner(s: &Arc<Shared>) -> Result<()> {
    let is_move = s.spec.kind == JobKind::Move;
    let mut sources = s.spec.sources.clone();

    // Same-filesystem move of a whole source: a single no-replace rename.
    if is_move {
        let dest_dev = fsx::device_of(&s.spec.dest_dir);
        sources.retain(|src| {
            let Some(name) = src.file_name() else {
                return true;
            };
            if dest_dev.is_none() || fsx::device_of(src) != dest_dev {
                return true;
            }
            let dest = s.spec.dest_dir.join(name);
            match fsx::rename_noreplace(src, &dest) {
                Ok(()) => {
                    s.progress.files_discovered.fetch_add(1, Ordering::Relaxed);
                    s.progress.files_done.fetch_add(1, Ordering::Relaxed);
                    false
                }
                // Destination exists or rename unsupported: per-file path.
                Err(_) => true,
            }
        });
        if sources.is_empty() {
            s.progress.scan_complete.store(1, Ordering::Relaxed);
            return Ok(());
        }
    }

    let opts = &s.spec.options;
    let (tx, rx) = mpsc::sync_channel::<Item>(opts.queue_depth.max(1));
    let rx = Arc::new(Mutex::new(rx));
    let mut workers = Vec::with_capacity(opts.workers);
    for n in 0..opts.workers.max(1) {
        let (s2, rx2) = (s.clone(), rx.clone());
        workers.push(
            thread::Builder::new()
                .name(format!("tufan-w{n}"))
                .spawn(move || worker(&s2, &rx2))
                .expect("spawn worker"),
        );
    }

    let scan = scanner::scan(
        &sources,
        &s.spec.dest_dir,
        &tx,
        &s.control,
        &s.progress,
        is_move,
    );
    drop(tx);
    for w in workers {
        let _ = w.join();
    }
    s.journal.flush()?;
    let scan = scan?;

    // Remove now-empty source directories, deepest first. remove_dir fails
    // on non-empty directories, so anything left behind is preserved.
    if is_move && !s.control.is_cancelled() {
        for d in scan.dirs.iter().rev() {
            let _ = fs::remove_dir(d);
        }
    }
    Ok(())
}

fn worker(s: &Shared, rx: &Mutex<Receiver<Item>>) {
    let mut buf = Vec::new();
    loop {
        let item = match rx.lock().unwrap().recv() {
            Ok(i) => i,
            Err(_) => return,
        };
        if s.control.is_cancelled() {
            continue; // drain so the scanner unblocks
        }
        let (path, result) = match item {
            Item::Failed { path, error } => (path, Err(error)),
            Item::Symlink { src, dest } => {
                let r = with_retry(s, || copy_symlink(s, &src, &dest));
                (src, r)
            }
            Item::File { src, dest, md } => {
                let r = with_retry(s, || process_file(s, &src, &md, &dest, &mut buf));
                (src, r)
            }
        };
        let p = &s.progress;
        match result {
            Ok(Done::Done) => p.files_done.fetch_add(1, Ordering::Relaxed),
            Ok(Done::Skipped) => p.files_skipped.fetch_add(1, Ordering::Relaxed),
            Err(Error::Cancelled) => 0,
            Err(e) => {
                let key = path_key(&path);
                // Never erase a recorded commitment or checkpoint: recovery
                // relies on it to finish a move or resume a large file.
                let rec = match s.journal.item(s.id, &key) {
                    Ok(Some(r)) if matches!(r.state, ItemState::Committed | ItemState::Partial) => {
                        ItemRecord {
                            error: Some(e.to_string()),
                            ..r
                        }
                    }
                    _ => ItemRecord {
                        key,
                        state: ItemState::Failed,
                        dest: None,
                        size: 0,
                        src_id: None,
                        offset: 0,
                        prefix_hash: None,
                        error: Some(e.to_string()),
                    },
                };
                let _ = s.journal.put(s.id, rec);
                record_error(s, e.to_string());
                p.files_failed.fetch_add(1, Ordering::Relaxed)
            }
        };
    }
}

enum Done {
    Done,
    Skipped,
}

fn with_retry(s: &Shared, mut f: impl FnMut() -> Result<Done>) -> Result<Done> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(e) if e.is_retryable() && attempt < s.spec.options.max_retries => {
                let base = 100u64.saturating_mul(1 << attempt.min(10)).min(5000);
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64 % (base / 2 + 1))
                    .unwrap_or(0);
                s.control.sleep(Duration::from_millis(base / 2 + jitter))?;
                attempt += 1;
            }
            r => return r,
        }
    }
}

fn process_file(
    s: &Shared,
    src: &Path,
    md: &fs::Metadata,
    dest: &Path,
    buf: &mut Vec<u8>,
) -> Result<Done> {
    let key = path_key(src);
    let is_move = s.spec.kind == JobKind::Move;
    let src_id = SourceId::of(md);
    let prior = if s.resumed {
        s.journal.item(s.id, &key).unwrap_or(None)
    } else {
        None
    };

    // Job-level resume: trust a previous result only if the destination is
    // still present with the expected size.
    if let Some(rec) = &prior {
        let dest_ok = |d: &Option<PathBuf>| {
            d.as_ref()
                .and_then(|d| fs::symlink_metadata(d).ok())
                .is_some_and(|m| m.is_file() && m.len() == rec.size)
        };
        match rec.state {
            ItemState::Done | ItemState::Skipped if !is_move => {
                if rec.state == ItemState::Skipped || dest_ok(&rec.dest) {
                    s.progress.bytes_done.fetch_add(md.len(), Ordering::Relaxed);
                    return Ok(Done::Done);
                }
            }
            ItemState::Committed if is_move && dest_ok(&rec.dest) => {
                // Crash after commit, before source removal: finish the move.
                let dest = rec.dest.clone().expect("checked");
                s.progress.bytes_done.fetch_add(md.len(), Ordering::Relaxed);
                return finish_move(s, src, src_id, rec.src_id.as_deref(), &dest, &key, rec.size);
            }
            _ => {}
        }
    }

    // Copying a file onto itself would destroy it on move.
    // Compared by identity (dev/inode) to avoid per-file canonicalisation.
    if let Ok(d) = fs::symlink_metadata(dest) {
        let did = SourceId::of(&d);
        if cfg!(unix) && (did.dev, did.ino) == (src_id.dev, src_id.ino) {
            return Err(Error::Conflict(dest.to_path_buf()));
        }
    }

    let ctx = Ctx {
        job_id: s.id,
        opts: &s.spec.options,
        control: &s.control,
        progress: &s.progress,
        journal: &s.journal,
    };
    let outcome = transfer::copy_file(&ctx, src, md, dest, &key, prior.as_ref(), buf)?;
    let rec = |state, dest: Option<PathBuf>| ItemRecord {
        key: key.clone(),
        state,
        dest,
        size: md.len(),
        src_id: Some(src_id.encode()),
        offset: 0,
        prefix_hash: None,
        error: None,
    };
    match outcome {
        Outcome::Skipped => {
            s.journal.put(s.id, rec(ItemState::Skipped, None))?;
            Ok(Done::Skipped)
        }
        Outcome::Copied {
            dest,
            lost_metadata,
        } => {
            if !lost_metadata.is_empty() {
                s.report.lock().unwrap().metadata_not_preserved += 1;
            }
            if !is_move {
                s.journal.put(s.id, rec(ItemState::Done, Some(dest)))?;
                return Ok(Done::Done);
            }
            // Destination commitment must be durable before deleting the source.
            if !s.spec.options.integrity.durable() {
                if let Some(parent) = dest.parent() {
                    fsx::sync_dir(parent).map_err(|e| Error::io(parent, e))?;
                }
            }
            s.journal
                .put_durable(s.id, rec(ItemState::Committed, Some(dest.clone())))?;
            faults::hit(Stage::CrashAfterCommit).map_err(|e| Error::io(src, e))?;
            finish_move(
                s,
                src,
                src_id,
                Some(&src_id.encode()),
                &dest,
                &key,
                md.len(),
            )
        }
    }
}

fn finish_move(
    s: &Shared,
    src: &Path,
    current: SourceId,
    recorded: Option<&str>,
    dest: &Path,
    key: &[u8],
    size: u64,
) -> Result<Done> {
    // Never delete a source that differs from what was copied.
    let unchanged = recorded.and_then(SourceId::decode) == Some(current)
        && fs::symlink_metadata(src)
            .map(|m| SourceId::of(&m) == current)
            .unwrap_or(false);
    if !unchanged {
        return Err(Error::SourceChanged(src.to_path_buf()));
    }
    let removal = faults::hit(Stage::SourceDelete).and_then(|_| fs::remove_file(src));
    match removal {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            // Partial move: destination is valid and kept; source remains.
            return Err(Error::io(
                src,
                io::Error::new(e.kind(), format!("partial move, source not removed: {e}")),
            ));
        }
    }
    s.journal.put(
        s.id,
        ItemRecord {
            key: key.to_vec(),
            state: ItemState::Done,
            dest: Some(dest.to_path_buf()),
            size,
            src_id: None,
            offset: 0,
            prefix_hash: None,
            error: None,
        },
    )?;
    Ok(Done::Done)
}

fn copy_symlink(s: &Shared, src: &Path, dest: &Path) -> Result<Done> {
    if s.spec.options.links == LinkPolicy::Skip {
        return Ok(Done::Skipped);
    }
    let target = fs::read_link(src).map_err(|e| Error::io(src, e))?;
    let Some((dest, replace)) = transfer::resolve_dest(dest, s.spec.options.conflict)? else {
        return Ok(Done::Skipped);
    };
    if replace {
        // Only ever replace another symlink, never a regular file.
        match fs::symlink_metadata(&dest) {
            Ok(m) if m.file_type().is_symlink() => {
                fs::remove_file(&dest).map_err(|e| Error::io(&dest, e))?
            }
            Ok(_) => return Err(Error::Conflict(dest)),
            Err(_) => {}
        }
    }
    fsx::make_symlink(&target, &dest).map_err(|e| Error::io(&dest, e))?;
    if s.spec.kind == JobKind::Move {
        fs::remove_file(src).map_err(|e| Error::io(src, e))?;
    }
    Ok(Done::Done)
}

fn record_error(s: &Shared, msg: String) {
    let mut r = s.report.lock().unwrap();
    if r.errors.len() < MAX_REPORTED_ERRORS {
        r.errors.push(msg);
    } else {
        r.errors_truncated = true;
    }
}

/// Remove this job's abandoned temp files under `dir` (non-recursive).
/// Only exact `.<name>.tufan-<job>.part` names are touched.
pub fn clean_temps(dir: &Path, job_id: i64) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let name = e.file_name();
        if fsx::is_temp_for_job(&name.to_string_lossy(), job_id)
            && e.file_type().map(|t| t.is_file()).unwrap_or(false)
            && fs::remove_file(e.path()).is_ok()
        {
            n += 1;
        }
    }
    n
}
