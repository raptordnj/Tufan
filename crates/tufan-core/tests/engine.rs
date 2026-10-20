use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tufan_core::faults::{self, Stage};
use tufan_core::journal::{self, ItemState, Journal};
use tufan_core::{filesystem, integrity};
use tufan_core::{start, ConflictPolicy, IntegrityMode, JobKind, JobSpec, JobState, Options};

// Fault injection is process-global; serialise every test.
static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    faults::clear();
    g
}

fn opts(integrity: IntegrityMode, conflict: ConflictPolicy) -> Options {
    Options {
        integrity,
        conflict,
        workers: 4,
        queue_depth: 16,
        max_retries: 2,
        ..Options::default()
    }
}

fn spec(kind: JobKind, sources: Vec<PathBuf>, dest: &Path, o: Options) -> JobSpec {
    JobSpec {
        kind,
        sources,
        dest_dir: dest.to_path_buf(),
        options: o,
    }
}

fn journal(dir: &Path) -> Arc<Journal> {
    Arc::new(Journal::open(&dir.join("journal.sqlite3")).unwrap())
}

fn hash(p: &Path) -> Vec<u8> {
    let mut buf = vec![0; 1 << 16];
    integrity::hash_all(&mut fs::File::open(p).unwrap(), &mut buf)
        .unwrap()
        .0
}

/// Deterministic pseudo-random bytes.
fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::write(root.join("zero"), b"").unwrap();
    fs::write(root.join(".hidden"), b"h").unwrap();
    fs::write(root.join("a/b/c/d/e/deep.bin"), bytes(300_000, 1)).unwrap();
    fs::write(root.join("ünïcødé 名前.txt"), b"unicode").unwrap();
    fs::write(root.join(format!("{}.txt", "L".repeat(200))), b"long").unwrap();
    for i in 0..500 {
        fs::write(
            root.join("a").join(format!("small{i}")),
            bytes(i * 7, i as u64),
        )
        .unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("zero", root.join("link")).unwrap();
}

fn assert_trees_equal(a: &Path, b: &Path) {
    let mut n = 0;
    let mut stack = vec![(a.to_path_buf(), b.to_path_buf())];
    while let Some((x, y)) = stack.pop() {
        for e in fs::read_dir(&x).unwrap() {
            let e = e.unwrap();
            let (px, py) = (e.path(), y.join(e.file_name()));
            let ft = e.file_type().unwrap();
            if ft.is_symlink() {
                assert_eq!(fs::read_link(&px).unwrap(), fs::read_link(&py).unwrap());
            } else if ft.is_dir() {
                assert!(py.is_dir(), "{py:?}");
                stack.push((px, py));
            } else {
                assert_eq!(hash(&px), hash(&py), "{px:?}");
                n += 1;
            }
        }
    }
    assert!(n > 0);
}

fn no_temps(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".part"), "leftover temp {name}");
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            }
        }
    }
}

#[test]
fn copies_tree_verified() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    make_tree(&src);
    fs::create_dir(&dst).unwrap();
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::DurableVerified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Completed, "{:?}", h.report());
    assert_trees_equal(&src, &dst.join("src"));
    assert!(dst.join("src/empty").is_dir());
    no_temps(&dst);
}

#[test]
fn copies_fast_mode_and_large_file() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("big.bin");
    fs::write(&src, bytes(70 << 20, 9)).unwrap(); // crosses a checkpoint
    let dst = t.path().join("out");
    fs::create_dir(&dst).unwrap();
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Fast, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Completed);
    assert_eq!(hash(&src), hash(&dst.join("big.bin")));
    let s = h.snapshot();
    assert_eq!(s.bytes_done, s.bytes_known);
}

#[test]
fn conflict_policies() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("f.txt");
    fs::write(&src, b"new").unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    fs::write(dst.join("f.txt"), b"old").unwrap();
    let j = journal(t.path());
    let run = |c| {
        start(
            j.clone(),
            spec(
                JobKind::Copy,
                vec![src.clone()],
                &dst,
                opts(IntegrityMode::Verified, c),
            ),
        )
        .unwrap()
        .wait()
    };

    assert_eq!(run(ConflictPolicy::Fail), JobState::Failed);
    assert_eq!(fs::read(dst.join("f.txt")).unwrap(), b"old");
    assert_eq!(run(ConflictPolicy::Skip), JobState::Completed);
    assert_eq!(fs::read(dst.join("f.txt")).unwrap(), b"old");
    assert_eq!(run(ConflictPolicy::Rename), JobState::Completed);
    assert_eq!(fs::read(dst.join("f (1).txt")).unwrap(), b"new");
    assert_eq!(run(ConflictPolicy::Overwrite), JobState::Completed);
    assert_eq!(fs::read(dst.join("f.txt")).unwrap(), b"new");
}

#[test]
fn journal_items_log_each_outcome() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("s");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("new.txt"), b"new").unwrap();
    fs::write(src.join("clash.txt"), b"new").unwrap();
    let dst = t.path().join("d");
    fs::create_dir_all(dst.join("s")).unwrap();
    fs::write(dst.join("s/clash.txt"), b"old").unwrap();
    let j = journal(t.path());
    let h = start(
        j.clone(),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::PartiallyCompleted);
    let items = j.items(h.id()).unwrap();
    let find = |name: &str| {
        items
            .iter()
            .find(|r| journal::key_path(&r.key).unwrap().ends_with(name))
            .unwrap()
    };
    assert_eq!(find("new.txt").state, ItemState::Done);
    let clash = find("clash.txt");
    assert_eq!(clash.state, ItemState::Failed);
    assert!(clash.error.as_deref().unwrap().contains("already exists"));
}

/// A crash can publish files before their journal batch is committed. On
/// resume those destinations already hold exactly the source content and
/// must count as done, not as conflicts. Simulated with a fresh journal.
#[test]
fn identical_destination_is_not_a_conflict() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    make_tree(&src);
    fs::create_dir(&dst).unwrap();
    let run = |kind, sources: Vec<PathBuf>, journal_name: &str| {
        let j = Arc::new(Journal::open(&t.path().join(journal_name)).unwrap());
        let h = start(
            j,
            spec(
                kind,
                sources,
                &dst,
                opts(IntegrityMode::Verified, ConflictPolicy::Fail),
            ),
        )
        .unwrap();
        (h.wait(), h.report())
    };
    let (st, rep) = run(JobKind::Copy, vec![src.clone()], "a.sqlite3");
    assert_eq!(st, JobState::Completed, "{rep:?}");
    let (st, rep) = run(JobKind::Copy, vec![src.clone()], "b.sqlite3");
    assert_eq!(st, JobState::Completed, "{rep:?}");
    assert_trees_equal(&src, &dst.join("src"));

    // Different content is still a conflict under the Fail policy.
    fs::write(dst.join("src/.hidden"), b"changed").unwrap();
    let (st, _) = run(JobKind::Copy, vec![src.clone()], "c.sqlite3");
    assert_eq!(st, JobState::PartiallyCompleted);
    assert_eq!(fs::read(dst.join("src/.hidden")).unwrap(), b"changed");

    // A move onto an identical copy completes by removing the source.
    fs::write(dst.join("src/.hidden"), b"h").unwrap();
    let (st, rep) = run(JobKind::Move, vec![src.clone()], "d.sqlite3");
    assert_eq!(st, JobState::Completed, "{rep:?}");
    assert!(!src.join("a/small7").exists());
    assert!(dst.join("src/a/small7").exists());
}

#[test]
fn rejects_destination_inside_source() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("s");
    fs::create_dir_all(src.join("inner")).unwrap();
    let r = start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &src.join("inner"),
            Options::default(),
        ),
    );
    assert!(matches!(
        r,
        Err(tufan_core::Error::DestinationInsideSource { .. })
    ));
}

#[test]
fn move_cross_directory_removes_sources() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    make_tree(&src);
    let reference = t.path().join("ref");
    fs::create_dir(&reference).unwrap();
    start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &reference,
            Options::default(),
        ),
    )
    .unwrap()
    .wait();
    fs::create_dir(&dst).unwrap();
    // Pre-existing destination forces the per-file move path.
    fs::create_dir(dst.join("src")).unwrap();
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Move,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Completed, "{:?}", h.report());
    assert!(!src.exists(), "source tree should be gone");
    assert_trees_equal(&reference.join("src"), &dst.join("src"));
}

#[test]
fn move_same_fs_rename() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("tree");
    make_tree(&src);
    let dst = t.path().join("dst");
    fs::create_dir(&dst).unwrap();
    let h = start(
        journal(t.path()),
        spec(JobKind::Move, vec![src.clone()], &dst, Options::default()),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Completed);
    assert!(!src.exists());
    assert!(dst.join("tree/a/b/c/d/e/deep.bin").is_file());
}

#[test]
fn verify_fault_never_publishes() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("x");
    fs::write(&src, bytes(5000, 3)).unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    faults::arm(Stage::Verify, 100);
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Failed);
    assert!(!dst.join("x").exists());
    no_temps(&dst);
}

#[test]
fn publish_fault_keeps_existing_destination() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("x");
    fs::write(&src, b"replacement").unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    fs::write(dst.join("x"), b"original").unwrap();
    faults::arm(Stage::Publish, 100);
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Overwrite),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Failed);
    assert_eq!(fs::read(dst.join("x")).unwrap(), b"original");
    no_temps(&dst);
}

#[test]
fn write_fault_retries_then_succeeds() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("x");
    fs::write(&src, bytes(10_000, 4)).unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    // Injected Write faults are not retryable (Other): one failure, then a
    // `resume` retries the failed item.
    faults::arm(Stage::Write, 1);
    let j = journal(t.path());
    let h = start(
        j.clone(),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Failed);
    let h2 = tufan_core::resume(j, h.id()).unwrap();
    assert_eq!(h2.wait(), JobState::Completed);
    assert_eq!(hash(&src), hash(&dst.join("x")));
}

#[test]
fn source_delete_failure_is_partial_move() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("s");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), b"data").unwrap();
    let dst = t.path().join("d");
    fs::create_dir_all(dst.join("s")).unwrap();
    faults::arm(Stage::SourceDelete, 1);
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Move,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Failed);
    assert_eq!(fs::read(src.join("f")).unwrap(), b"data", "source kept");
    assert_eq!(
        fs::read(dst.join("s/f")).unwrap(),
        b"data",
        "destination kept"
    );
}

#[test]
fn crash_after_commit_recovers_move() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("s");
    fs::create_dir(&src).unwrap();
    for i in 0..20 {
        fs::write(src.join(format!("f{i}")), bytes(100 + i, i as u64)).unwrap();
    }
    let dst = t.path().join("d");
    fs::create_dir_all(dst.join("s")).unwrap();
    let j = journal(t.path());
    faults::arm(Stage::CrashAfterCommit, 5);
    let h = start(
        j.clone(),
        spec(
            JobKind::Move,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::PartiallyCompleted);
    // Committed-but-not-deleted sources still exist alongside destinations.
    let left = fs::read_dir(&src).unwrap().count();
    assert_eq!(left, 5);
    faults::clear();
    let h2 = tufan_core::resume(j, h.id()).unwrap();
    assert_eq!(h2.wait(), JobState::Completed, "{:?}", h2.report());
    assert!(!src.exists());
    assert_eq!(fs::read_dir(dst.join("s")).unwrap().count(), 20);
}

#[test]
fn cancel_leaves_no_partial_files_visible() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("s");
    fs::create_dir(&src).unwrap();
    for i in 0..2000 {
        fs::write(src.join(format!("f{i}")), bytes(2000, i)).unwrap();
    }
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    h.pause();
    std::thread::sleep(std::time::Duration::from_millis(50));
    h.resume();
    h.cancel();
    assert_eq!(h.wait(), JobState::Cancelled);
    no_temps(&dst);
    // Every published file is complete. Cancellation may land before the
    // scanner has created the destination directory at all.
    let Ok(published) = fs::read_dir(dst.join("s")) else {
        return;
    };
    for e in published.flatten() {
        assert_eq!(hash(&e.path()), hash(&src.join(e.file_name())));
    }
}

#[test]
fn large_file_resume_from_checkpoint() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("big");
    fs::write(&src, bytes(200 << 20, 11)).unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    let j = journal(t.path());
    let o = Options {
        buffer_size: 1 << 20,
        ..opts(IntegrityMode::Verified, ConflictPolicy::Fail)
    };
    // Fail on the 150th buffer write (~150 MiB, after two checkpoints).
    faults::arm(Stage::Write, 0);
    let h = start(j.clone(), spec(JobKind::Copy, vec![src.clone()], &dst, o)).unwrap();
    // Let it get past checkpoints, then cancel-free interruption via fault.
    while h.snapshot().bytes_done < 140 << 20 && !h.is_finished() {
        std::thread::yield_now();
    }
    faults::arm(Stage::Write, 1);
    assert_eq!(h.wait(), JobState::Failed);
    faults::clear();
    let temp = filesystem::temp_path(&dst.join("big"), h.id(), &journal::path_key(&src));
    assert!(temp.exists(), "checkpointed temp kept for resume");
    let h2 = tufan_core::resume(j, h.id()).unwrap();
    assert_eq!(h2.wait(), JobState::Completed);
    // Resume started from the checkpoint, not from zero.
    assert!(h2.snapshot().bytes_done >= 200 << 20);
    assert_eq!(hash(&src), hash(&dst.join("big")));
    assert!(!temp.exists());
}

#[test]
fn corrupt_checkpoint_restarts_file() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("big");
    fs::write(&src, bytes(150 << 20, 12)).unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    let j = journal(t.path());
    let h = start(
        j.clone(),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    while h.snapshot().bytes_done < 100 << 20 && !h.is_finished() {
        std::thread::yield_now();
    }
    faults::arm(Stage::Write, 1);
    assert_eq!(h.wait(), JobState::Failed);
    faults::clear();
    let temp = filesystem::temp_path(&dst.join("big"), h.id(), &journal::path_key(&src));
    // Corrupt the checkpointed prefix.
    let mut data = fs::read(&temp).unwrap();
    data[10] ^= 0xff;
    fs::write(&temp, data).unwrap();
    let h2 = tufan_core::resume(j, h.id()).unwrap();
    assert_eq!(h2.wait(), JobState::Completed);
    assert_eq!(hash(&src), hash(&dst.join("big")));
}

#[test]
fn source_change_is_detected() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("grow");
    fs::write(&src, bytes(80 << 20, 13)).unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    let o = Options {
        max_retries: 0,
        ..opts(IntegrityMode::Verified, ConflictPolicy::Fail)
    };
    let h = start(
        journal(t.path()),
        spec(JobKind::Copy, vec![src.clone()], &dst, o),
    )
    .unwrap();
    while h.snapshot().bytes_done < 1 << 20 && !h.is_finished() {
        std::thread::yield_now();
    }
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&src)
        .unwrap()
        .write_all(b"more")
        .unwrap();
    assert_eq!(h.wait(), JobState::Failed);
    assert!(!dst.join("grow").exists());
}

#[test]
fn invalid_journal_rows_are_rejected() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("j.sqlite3");
    {
        let j = Journal::open(&p).unwrap();
        j.create_job(&spec(
            JobKind::Copy,
            vec![t.path().join("x")],
            t.path(),
            Options::default(),
        ))
        .unwrap();
    }
    let c = rusqlite_conn(&p);
    c.execute("UPDATE jobs SET kind = 'bogus'", []).unwrap();
    drop(c);
    let j = Journal::open(&p).unwrap();
    assert!(j.load_job(1).is_err());
    assert!(j.list_jobs().unwrap().is_empty());
}

fn rusqlite_conn(p: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(p).unwrap()
}

#[test]
fn copy_in_place_keeps_both() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("f.txt");
    fs::write(&f, b"data").unwrap();
    let run = |c| {
        start(
            journal(t.path()),
            spec(
                JobKind::Copy,
                vec![f.clone()],
                t.path(),
                opts(IntegrityMode::Verified, c),
            ),
        )
        .unwrap()
        .wait()
    };
    // Never replaces a file with itself.
    assert_eq!(run(ConflictPolicy::Overwrite), JobState::Failed);
    assert_eq!(run(ConflictPolicy::Rename), JobState::Completed);
    assert_eq!(fs::read(t.path().join("f (1).txt")).unwrap(), b"data");
    assert_eq!(fs::read(&f).unwrap(), b"data");
}

#[test]
fn temp_names_differ_for_case_variants() {
    let d = Path::new("dir");
    let a = filesystem::temp_path(&d.join("Readme.md"), 1, b"/s/Readme.md");
    let b = filesystem::temp_path(&d.join("README.md"), 1, b"/s/README.md");
    assert_ne!(
        a.to_string_lossy().to_lowercase(),
        b.to_string_lossy().to_lowercase()
    );
    assert!(filesystem::is_temp_for_job(
        &a.file_name().unwrap().to_string_lossy(),
        1
    ));
}

#[test]
fn read_only_files_copy_overwrite_and_move() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("s");
    fs::create_dir(&src).unwrap();
    let ro = |p: &Path| {
        let mut perm = fs::metadata(p).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(p, perm).unwrap();
    };
    fs::write(src.join("pack.idx"), b"objects").unwrap();
    ro(&src.join("pack.idx"));
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    let run = |kind, c| {
        let h = start(
            journal(t.path()),
            spec(
                kind,
                vec![src.clone()],
                &dst,
                opts(IntegrityMode::Verified, c),
            ),
        )
        .unwrap();
        (h.wait(), h.report())
    };
    let (st, rep) = run(JobKind::Copy, ConflictPolicy::Fail);
    assert_eq!(st, JobState::Completed, "{rep:?}");
    assert!(fs::metadata(dst.join("s/pack.idx"))
        .unwrap()
        .permissions()
        .readonly());
    // Replace the read-only copy with different content.
    let mut perm = fs::metadata(src.join("pack.idx")).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    fs::set_permissions(src.join("pack.idx"), perm).unwrap();
    fs::write(src.join("pack.idx"), b"objects v2").unwrap();
    ro(&src.join("pack.idx"));
    let (st, rep) = run(JobKind::Copy, ConflictPolicy::Overwrite);
    assert_eq!(st, JobState::Completed, "{rep:?}");
    assert_eq!(fs::read(dst.join("s/pack.idx")).unwrap(), b"objects v2");
    // Moving onto the identical read-only copy removes the read-only source.
    let (st, rep) = run(JobKind::Move, ConflictPolicy::Fail);
    assert_eq!(st, JobState::Completed, "{rep:?}");
    assert!(!src.join("pack.idx").exists());
}

#[cfg(unix)]
#[test]
fn absolute_links_inside_the_tree_point_at_the_copy() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("proj");
    fs::create_dir_all(src.join("storage/public")).unwrap();
    fs::write(src.join("storage/public/a.txt"), b"a").unwrap();
    std::os::unix::fs::symlink(src.join("storage/public"), src.join("public_storage")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", src.join("outside")).unwrap();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Copy,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    assert_eq!(h.wait(), JobState::Completed, "{:?}", h.report());
    assert_eq!(
        fs::read_link(dst.join("proj/public_storage")).unwrap(),
        dst.join("proj/storage/public")
    );
    // Targets outside the copied tree are kept as they are.
    assert_eq!(
        fs::read_link(dst.join("proj/outside")).unwrap(),
        Path::new("/etc/hostname")
    );
}

/// Junctions (e.g. Laravel's `public/storage`, npm workspaces) and directory
/// symlinks are recreated as links into the copy, never as broken links.
#[cfg(windows)]
#[test]
fn windows_junctions_and_dir_links_are_recreated() {
    let _g = serial();
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("proj");
    fs::create_dir_all(src.join("storage/app/public")).unwrap();
    fs::write(src.join("storage/app/public/a.txt"), b"a").unwrap();
    fs::create_dir(src.join("public")).unwrap();
    junction::create(src.join("storage/app/public"), src.join("public/storage")).unwrap();
    let dir_link = std::os::windows::fs::symlink_dir("storage", src.join("rel")).is_ok();
    let dst = t.path().join("d");
    fs::create_dir(&dst).unwrap();
    let h = start(
        journal(t.path()),
        spec(
            JobKind::Move,
            vec![src.clone()],
            &dst,
            opts(IntegrityMode::Verified, ConflictPolicy::Fail),
        ),
    )
    .unwrap();
    // Cross-check through a fresh move: links must survive source removal.
    assert_eq!(h.wait(), JobState::Completed, "{:?}", h.report());
    let j = dst.join("proj/public/storage");
    assert!(junction::exists(&j).unwrap());
    assert_eq!(
        filesystem::strip_verbatim(&junction::get_target(&j).unwrap()),
        dst.join("proj/storage/app/public")
    );
    assert_eq!(fs::read(j.join("a.txt")).unwrap(), b"a");
    if dir_link {
        assert_eq!(
            fs::read(dst.join("proj/rel/app/public/a.txt")).unwrap(),
            b"a"
        );
    }
}
