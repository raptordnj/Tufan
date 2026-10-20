//! Single-file transfer with safe destination publication.
//!
//! Protocol: write to a job-scoped temp file beside the destination, verify,
//! apply metadata, optionally fsync, then publish by rename. An existing
//! destination is never truncated; it can only be replaced atomically.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use sha2::Digest;

use crate::control::{Control, Progress};
use crate::error::{Error, Result};
use crate::faults::{self, Stage};
use crate::filesystem::{self as fsx, SourceId};
use crate::integrity::{self, Hasher};
use crate::journal::{ItemRecord, ItemState, Journal};
use crate::policy::{ConflictPolicy, Options};

/// Files at least this large record resumable checkpoints.
pub const CHECKPOINT_MIN: u64 = 64 << 20;
pub const CHECKPOINT_EVERY: u64 = 64 << 20;
/// Below this size Fast mode hands the copy to the kernel (`io::copy`
/// uses `copy_file_range`/`sendfile` on Linux) without pause granularity.
const KERNEL_COPY_MAX: u64 = 16 << 20;

pub struct Ctx<'a> {
    pub job_id: i64,
    pub opts: &'a Options,
    pub control: &'a Control,
    pub progress: &'a Progress,
    pub journal: &'a Journal,
}

#[derive(Debug)]
pub enum Outcome {
    Copied {
        dest: PathBuf,
        lost_metadata: Vec<&'static str>,
    },
    Skipped,
}

/// Decide the final destination path according to the conflict policy.
/// Returns `(path, replace_existing)` or `None` to skip.
pub fn resolve_dest(dest: &Path, policy: ConflictPolicy) -> Result<Option<(PathBuf, bool)>> {
    let existing = match fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Some((dest.to_path_buf(), false)))
        }
        Err(e) => return Err(Error::io(dest, e)),
    };
    let rename = || -> Result<Option<(PathBuf, bool)>> {
        for n in 1..10_000 {
            let c = fsx::renamed_candidate(dest, n);
            if fs::symlink_metadata(&c).is_err() {
                return Ok(Some((c, false)));
            }
        }
        Err(Error::Conflict(dest.to_path_buf()))
    };
    match policy {
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Fail => Err(Error::Conflict(dest.to_path_buf())),
        ConflictPolicy::Rename => rename(),
        ConflictPolicy::Overwrite => {
            if existing.is_dir() {
                return Err(Error::Conflict(dest.to_path_buf()));
            }
            // A different file that the filesystem treats as the same name
            // (case folding / normalisation) is never replaced.
            if fsx::existing_spelling_differs(dest) {
                return rename();
            }
            Ok(Some((dest.to_path_buf(), true)))
        }
    }
}

pub fn copy_file(
    ctx: &Ctx<'_>,
    src: &Path,
    src_md: &Metadata,
    dest: &Path,
    key: &[u8],
    prior: Option<&ItemRecord>,
    buf: &mut Vec<u8>,
) -> Result<Outcome> {
    ctx.control.checkpoint()?;
    let src_id = SourceId::of(src_md);

    // A partial checkpoint from a previous run pins the destination name.
    let resume = prior
        .filter(|r| r.state == ItemState::Partial && r.src_id.as_deref() == Some(&src_id.encode()));
    let (dest, replace) = match resume.and_then(|r| r.dest.clone()) {
        Some(d) => {
            let replace =
                fs::symlink_metadata(&d).is_ok() && ctx.opts.conflict == ConflictPolicy::Overwrite;
            if fs::symlink_metadata(&d).is_ok() && !replace {
                match resolve_dest(&d, ctx.opts.conflict)? {
                    Some(x) => x,
                    None => return Ok(Outcome::Skipped),
                }
            } else {
                (d, replace)
            }
        }
        None => match resolve_dest(dest, ctx.opts.conflict)? {
            Some(x) => x,
            None => return Ok(Outcome::Skipped),
        },
    };

    let temp = fsx::temp_path(&dest, ctx.job_id);
    let mut checkpointed = false;
    let mut added = 0u64;
    let r = write_and_publish(
        ctx,
        src,
        src_md,
        src_id,
        &dest,
        &temp,
        replace,
        key,
        resume,
        &mut checkpointed,
        &mut added,
        buf,
    );
    if r.is_err() {
        // Retries recount bytes; keep the progress total honest.
        ctx.progress.bytes_done.fetch_sub(added, Ordering::Relaxed);
        // Keep a checkpointed temp for file-level resume unless cancelled.
        let keep = checkpointed
            && !matches!(
                r,
                Err(Error::Cancelled) | Err(Error::SourceChanged(_)) | Err(Error::VerifyFailed(_))
            );
        if !keep {
            let _ = fs::remove_file(&temp);
        }
    }
    r
}

#[allow(clippy::too_many_arguments)]
fn write_and_publish(
    ctx: &Ctx<'_>,
    src: &Path,
    src_md: &Metadata,
    src_id: SourceId,
    dest: &Path,
    temp: &Path,
    replace: bool,
    key: &[u8],
    resume: Option<&ItemRecord>,
    checkpointed: &mut bool,
    added: &mut u64,
    buf: &mut Vec<u8>,
) -> Result<Outcome> {
    let opts = ctx.opts;
    let size = src_md.len();
    if buf.len() != opts.buffer_size {
        buf.resize(opts.buffer_size, 0);
    }
    let mut input = File::open(src).map_err(|e| Error::io(src, e))?;
    // Open handle must refer to the file we stat'ed.
    if SourceId::of(&input.metadata().map_err(|e| Error::io(src, e))?) != src_id {
        return Err(Error::SourceChanged(src.to_path_buf()));
    }

    let (mut out, mut hasher, mut written) = match try_resume(ctx, temp, resume, buf)? {
        Some((f, h, off)) => {
            input
                .seek(SeekFrom::Start(off))
                .map_err(|e| Error::io(src, e))?;
            ctx.progress.bytes_done.fetch_add(off, Ordering::Relaxed);
            *added += off;
            *checkpointed = true;
            (f, h, off)
        }
        None => {
            let _ = fs::remove_file(temp);
            let f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temp)
                .map_err(|e| Error::io(temp, e))?;
            (f, Hasher::new(), 0u64)
        }
    };

    let hashing = opts.integrity.hashes() || size >= CHECKPOINT_MIN;
    if !hashing && size <= KERNEL_COPY_MAX && written == 0 {
        faults::hit(Stage::Write).map_err(|e| Error::io(temp, e))?;
        written = io::copy(&mut input, &mut out).map_err(|e| Error::io(temp, e))?;
        ctx.progress
            .bytes_done
            .fetch_add(written, Ordering::Relaxed);
        *added += written;
    } else {
        let mut next_cp = written + CHECKPOINT_EVERY;
        loop {
            ctx.control.checkpoint()?;
            let n = match input.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::io(src, e)),
            };
            faults::hit(Stage::Write).map_err(|e| Error::io(temp, e))?;
            // write_all retries short writes; a zero-length write is an error.
            out.write_all(&buf[..n]).map_err(|e| Error::io(temp, e))?;
            if hashing {
                hasher.update(&buf[..n]);
            }
            written += n as u64;
            ctx.progress
                .bytes_done
                .fetch_add(n as u64, Ordering::Relaxed);
            *added += n as u64;
            if size >= CHECKPOINT_MIN && written >= next_cp && written < size {
                out.sync_data().map_err(|e| Error::io(temp, e))?;
                ctx.journal.put_durable(
                    ctx.job_id,
                    ItemRecord {
                        key: key.to_vec(),
                        state: ItemState::Partial,
                        dest: Some(dest.to_path_buf()),
                        size,
                        src_id: Some(src_id.encode()),
                        offset: written,
                        prefix_hash: Some(integrity::digest(&hasher)),
                        error: None,
                    },
                )?;
                *checkpointed = true;
                next_cp = written + CHECKPOINT_EVERY;
            }
        }
    }

    // Detect modification during the copy.
    let now = fs::symlink_metadata(src).map_err(|e| Error::io(src, e))?;
    if written != size || SourceId::of(&now) != src_id {
        return Err(Error::SourceChanged(src.to_path_buf()));
    }

    if opts.integrity.hashes() {
        out.flush().map_err(|e| Error::io(temp, e))?;
        let expected = hasher.finalize().to_vec();
        let mut check = File::open(temp).map_err(|e| Error::io(temp, e))?;
        let (actual, n) = integrity::hash_all(&mut check, buf).map_err(|e| Error::io(temp, e))?;
        if faults::hit(Stage::Verify).is_err() || n != size || actual != expected {
            return Err(Error::VerifyFailed(dest.to_path_buf()));
        }
    }

    let lost_metadata = fsx::apply_metadata(&out, src_md);
    if opts.integrity.durable() {
        out.sync_all().map_err(|e| Error::io(temp, e))?;
    }
    drop(out);

    faults::hit(Stage::Publish).map_err(|e| Error::io(dest, e))?;
    if replace {
        fs::rename(temp, dest).map_err(|e| Error::io(dest, e))?;
    } else {
        match fsx::rename_noreplace(temp, dest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Raced with another writer: never clobber it.
                return Err(Error::Conflict(dest.to_path_buf()));
            }
            Err(e) => return Err(Error::io(dest, e)),
        }
    }
    if opts.integrity.durable() {
        if let Some(parent) = dest.parent() {
            fsx::sync_dir(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    Ok(Outcome::Copied {
        dest: dest.to_path_buf(),
        lost_metadata,
    })
}

/// Validate a checkpoint by re-hashing the temp prefix. Any inconsistency
/// discards the checkpoint and the file restarts from zero.
fn try_resume(
    ctx: &Ctx<'_>,
    temp: &Path,
    resume: Option<&ItemRecord>,
    buf: &mut [u8],
) -> Result<Option<(File, Hasher, u64)>> {
    let Some(rec) = resume else { return Ok(None) };
    let (off, Some(expected)) = (rec.offset, rec.prefix_hash.as_ref()) else {
        return Ok(None);
    };
    if off == 0 {
        return Ok(None);
    }
    let Ok(mut f) = OpenOptions::new().read(true).write(true).open(temp) else {
        return Ok(None);
    };
    match f.metadata() {
        Ok(m) if m.is_file() && m.len() >= off => {}
        _ => return Ok(None),
    }
    ctx.control.checkpoint()?;
    let Ok((h, n)) = integrity::hash_prefix(&mut f, off, buf) else {
        return Ok(None);
    };
    if n != off || &integrity::digest(&h) != expected {
        return Ok(None);
    }
    if f.set_len(off).is_err() || f.seek(SeekFrom::Start(off)).is_err() {
        return Ok(None);
    }
    Ok(Some((f, h, off)))
}
