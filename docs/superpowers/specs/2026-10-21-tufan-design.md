# Tufan — Design (Milestone 1)

Source requirements: `Tufan.md`. This document records the decisions made for
the first coherent milestone. The user asked for autonomous execution, so the
interactive approval gates were replaced by this written record.

## Decisions

| Topic | Decision | Reason |
|---|---|---|
| Language | Rust (stable) | Memory/thread safety without GC; no measured evidence favouring C. Throughput is dominated by syscalls and storage, not language. |
| UI | GTK 4 via `gtk4-rs` | Mature native widgets, accessibility, native file dialogs, light/dark via libadwaita-free CSS. Available on Linux/FreeBSD/macOS/Windows (MSYS2). Slint/egui lack native dialogs/a11y parity. |
| Journal | SQLite (`rusqlite`, bundled), WAL, `synchronous=FULL` | Transactional crash safety, schema versioning and incremental queries out of the box; a hand-rolled log would re-implement this badly. |
| Hash | SHA-256 (`sha2`), streaming | Strong, standard. |
| Concurrency | `std::thread` workers + bounded `sync_channel` | No async runtime needed; filesystem calls are blocking anyway. |

## Workspace

- `crates/tufan-core` — engine, no UI deps. Modules: `error`, `policy`,
  `scanner`, `transfer`, `integrity`, `journal`, `filesystem`, `engine`.
- `crates/tufan-cli` — `tufan copy|move|resume|jobs` command-line front end.
- `crates/tufan-gtk` — GTK 4 desktop UI, polls engine progress snapshots.
- `packaging/` — NSIS script, Arch PKGBUILD, deb/rpm metadata, AppImage notes.
- `bench/` — reproducible benchmark script.

## Publication protocol (per file)

1. Stat source (size, mtime, inode/dev on Unix) → `SourceId`.
2. Temp file `.<name>.tufan-<job>.part` in destination directory (deterministic,
   so recovery can identify *only Tufan's* temp files for a given job).
3. Stream with a reused 1 MiB buffer, hashing source bytes (Verified modes).
   Fast mode uses `std::io::copy`, which on Linux uses `copy_file_range`
   (kernel-assisted, may reflink on Btrfs/XFS).
4. Large files (≥ 64 MiB): checkpoint every 64 MiB — fsync temp, record offset
   + SHA-256 of prefix in the journal. Resume re-hashes the temp prefix, compares,
   and continues; mismatch → restart file.
5. Re-stat source; changed `SourceId` → error `SourceChanged` (retryable).
6. Verified: re-read temp, compare SHA-256 with source stream hash.
7. Apply permissions + mtime. Durable: `fsync` file.
8. Publish: no-replace rename (`renameat2(RENAME_NOREPLACE)` on Linux,
   `MoveFileExW` without REPLACE on Windows via `std::fs::hard_link` fallback
   elsewhere) or atomic replacing rename when policy is Overwrite.
9. Durable: fsync destination directory (Unix).
10. Journal item → `Committed`.

Existing destinations are never truncated: replacement only happens by rename.

## Integrity modes

- **Fast** — size + source-change check. No content hash. Not cryptographic.
- **Verified** — SHA-256 of source stream vs. SHA-256 of re-read temp before publish. Re-read may be served from page cache, so it detects software/transfer errors, not necessarily media errors.
- **DurableVerified** — Verified + fsync of file and parent directory before reporting committed and before any source deletion.

## Move

Same filesystem: no-replace rename. Cross filesystem (`EXDEV`/different dev):
copy → verify → publish → journal `Committed` (forced durable flush) → re-stat
source, delete only if `SourceId` unchanged → journal `Done`. Failure to delete →
`PartialMove`. Directories are removed bottom-up only when empty.

## Conflicts

`Skip`, `Overwrite`, `Rename` (`name (1).ext`), `Fail`. If the destination exists
under a *different* spelling (case-insensitive/normalising FS), Overwrite is
downgraded to Rename — never replace a file whose name differs.

## Links

Symlinks are never followed during traversal. Policy `CopyLink` (default)
recreates the link; `Skip` skips. Hard links are copied as independent files
(documented limitation).

## Control

`Control` = atomic state (Running/Paused/Cancelled) + condvar. Pause stops new
files from starting and pauses streaming loops at buffer boundaries. Cancel
removes the current temp file; committed files remain.

Retry: retryable io errors (Interrupted, TimedOut, WouldBlock, sharing
violation, SourceChanged) up to 5 attempts, exponential backoff 100ms·2ⁿ with
jitter, capped at 5 s.

## Journal schema (v1)

`jobs(id, kind, sources_json, dest, options_json, state, created, updated)`
`items(job_id, rel_path, state, size, src_id, offset, prefix_hash, error)`
Batches commit every 256 items or 250 ms; forced flush before source deletion.

Resume: re-scan sources (incremental), skip items in `Done/Committed` whose
destination exists with expected size; resume `Partial` items from checkpoint.
Recovery of moves never deletes a source unless the destination is committed
in the journal *and* present with matching size.

## Progress

Atomic counters (files done/discovered, bytes done/known, state); consumers
poll snapshots (UI at 4 Hz). No per-file events.

## Out of scope this milestone (tracked in TODO.md)

Windows tray/Explorer shell extension, macOS bundle signing, ACL/xattr/ADS
preservation, sparse files, parallel chunked large-file copy, CI matrix.
