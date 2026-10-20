# Tufan — High-Performance, Fault-Tolerant, Cross-Platform File Copy and Move Application

## 1. Role and Mission

Act as a senior systems engineer, storage-engine specialist, concurrency expert, cross-platform desktop developer, and performance engineer.

Build **Tufan**, a high-performance, lightweight, fault-tolerant desktop application for copying and moving files and directories.

Tufan must provide:

- Exceptional performance when copying millions of small files, especially `node_modules`, source repositories, package caches, and build artifacts.
- High throughput for large files.
- Safe pause, resume, cancellation, retry, and crash recovery.
- Strong protection against incomplete, corrupted, or accidentally overwritten files.
- Bounded memory usage and predictable resource consumption.
- A minimal, smooth, visually relaxing desktop interface.
- Native Windows Explorer integration and persistent system-tray operation.
- Filesystem-aware optimization across Windows, Linux, macOS, and FreeBSD.
- Installers and distribution packages for supported operating systems and CPU architectures.

Treat data integrity as the highest priority. Optimize throughput only when the resulting implementation preserves its documented safety guarantees.

Do not claim absolute zero corruption, zero memory leaks, perfect crash safety, or maximum possible throughput. Engineer for strong integrity, controlled resource use, reliable recovery, and measurable performance.

## 2. Technology Decision: C vs. Rust

Before committing to the implementation language, briefly evaluate C and Rust against the actual requirements.

### 2.1 Compare the candidates

Evaluate both languages for:

- Real-world small-file throughput.
- Large-file streaming performance.
- CPU and memory consumption.
- Thread safety and concurrency complexity.
- Memory-safety guarantees.
- Windows filesystem and shell integration.
- Linux, macOS, and FreeBSD compatibility.
- Availability of maintained libraries.
- Native UI integration.
- Build and packaging complexity.
- Binary size, startup time, and long-term maintenance.

Do not assume that either language is automatically faster. Filesystem latency, metadata operations, storage hardware, scheduling, and synchronization frequently dominate transfer performance.

If useful, build small equivalent prototypes for directory traversal, bounded concurrent copying, and streaming verification. Compare optimized release builds under identical conditions.

### 2.2 Preferred implementation

Use the following default unless repository constraints or measured evidence justify a different choice:

- **Core language: Rust, using the stable toolchain.**
- **UI: evaluate GTK 4 with Rust bindings against Slint and egui.**
- **Architecture: keep the transfer engine independent of the UI toolkit.**
- **Windows integration: use supported native Windows APIs through maintained Rust bindings or narrowly scoped FFI.**
- **Build system: Cargo, with reproducible release configurations.**

Rust is preferred because its ownership model and type system help prevent use-after-free, double-free, and many data-race errors without requiring a garbage collector.

C remains a valid alternative if a demonstrated technical requirement makes it preferable. If C is selected, use a modern C standard, strict ownership rules, checked arithmetic, comprehensive cleanup paths, sanitizers, and static analysis.

Do not implement two complete production engines. Make one documented technology decision and maintain one authoritative implementation.

### 2.3 Select the UI independently

Evaluate UI options based on actual cross-platform compatibility, not language popularity.

- **GTK 4:** mature native widgets, strong Linux integration, and bindings for Rust.
- **Slint:** declarative UI with a custom visual style and potential for a compact application.
- **egui:** a Rust-oriented immediate-mode interface that may suit a compact custom design.
- **Qt 6 Widgets:** a mature alternative if allowing a C++ UI layer is acceptable; Qt is not a pure-C toolkit.

Verify current toolkit versions, target support, accessibility, system-tray support, native dialogs, clipboard integration, packaging, and dependency requirements.

Select the best overall option and proceed. Do not stop after presenting alternatives.

## 3. Repository Inspection and Development Workflow

Before changing files:

1. Inspect the current repository, existing code, project structure, dependencies, build configuration, and available development tools.
2. Preserve useful existing functionality and conventions.
3. Identify the most important data-integrity and platform-integration risks.
4. Write a concise implementation plan and maintain a `TODO.md` checklist.
5. Establish a working development and release build.

Implement the application incrementally. Prioritize a correct, testable transfer engine before polishing the GUI.

Do not stop after creating an architecture document, empty modules, pseudocode, or a UI prototype. Implement working functionality.

If the entire scope cannot be completed in one session, deliver a coherent working milestone and explicitly document remaining work.

## 4. Cross-Platform Architecture

Keep the architecture modular but not unnecessarily elaborate.

Suggested structure:

- `src/core/` — application lifecycle and transfer orchestration.
- `src/scanner/` — incremental directory traversal.
- `src/scheduler/` — bounded queues and worker scheduling.
- `src/transfer/` — file copying, moving, publication, and recovery.
- `src/integrity/` — content hashing and verification.
- `src/journal/` — durable job persistence and recovery.
- `src/filesystem/` — filesystem capabilities and metadata handling.
- `src/platform/windows/` — Windows APIs, Explorer integration, and tray.
- `src/platform/linux/` — Linux-specific behavior.
- `src/platform/macos/` — macOS integration.
- `src/platform/freebsd/` — FreeBSD-specific integration.
- `src/ui/` — widgets, rendering, and interaction.
- `src/config/` — preferences and configuration.
- `tests/` — unit, integration, stress, and fault-injection tests.
- `packaging/` — installers and release configurations.

Adapt this structure to the selected language and existing repository.

Keep the transfer engine usable without the GUI. Separate UI events from filesystem operations, verification, and persistent state management.

Use typed errors, explicit resource ownership, clear module boundaries, and platform-specific implementations only where necessary.

## 5. Core File Operations

Implement complete support for:

- Copying individual files.
- Copying complete directory trees.
- Moving files and directory trees.
- Multiple sources and destination selection.
- Hidden files and empty directories.
- Deep directory structures.
- Large files and mixed-size workloads.
- Existing destination conflicts.
- Overwrite, skip, rename, and conflict-resolution policies.
- Retryable and non-retryable errors.
- Progress tracking and completion summaries.
- Filesystem metadata preservation where supported.
- Cross-volume and cross-filesystem operations.
- Safe handling of symbolic links, junctions, reparse points, and hard links.
- Files that are changing while being copied.

Do not follow symbolic links recursively by default. Provide explicit, documented behavior for copying links versus copying their targets.

Prevent recursive copying when the destination lies inside the source tree.

Handle destination filename collisions caused by case sensitivity, Unicode normalization, reserved characters, or filesystem restrictions. Never silently overwrite a different file because the destination filesystem considers two names equivalent.

## 6. Data Integrity and Safe Destination Publication

Design every transfer around a safe destination-publication protocol.

### 6.1 File-copy procedure

For each destination file:

1. Validate the source and destination paths.
2. Establish source identity and metadata needed for validation.
3. Create a uniquely named temporary file in the destination directory when possible.
4. Stream the source into the temporary destination using bounded buffers.
5. Correctly handle short writes and partial failures.
6. Persist safe progress checkpoints when required.
7. Verify the output according to the configured integrity policy.
8. Apply supported metadata.
9. Flush and synchronize file data when required by the selected durability policy and supported by the filesystem.
10. Publish the completed file using the safest supported commit operation.
11. Synchronize relevant directory metadata when supported and required.
12. Persist the final state and report completion.

Never mark an incomplete or unverified file as completed.

Never truncate an existing destination file before a replacement has been prepared safely.

Use atomic rename or replacement when its semantics and filesystem constraints permit it. Document cases where atomic publication or durability cannot be guaranteed.

### 6.2 Integrity verification

Support:

- Size validation.
- Source-change detection.
- Optional full content verification.
- Streaming SHA-256 verification or another appropriate strong hash.
- Chunk-based verification for large-file resume.
- Detection of inconsistent recovery checkpoints.

Do not load entire files into memory for hashing.

Provide clear integrity policies, for example:

- **Fast:** use appropriate metadata and transfer checks, with weaker protection against undetected content changes.
- **Verified:** verify complete content before publishing.
- **Durable verified:** verify content and perform the supported synchronization operations required by the selected durability policy.

Explain the guarantees and performance costs of each mode. Never describe metadata comparison alone as cryptographic integrity verification.

Hardware failures, filesystem defects, remote servers, and storage firmware can violate assumptions. Report detectable failures and the actual guarantees of the chosen mode.

## 7. Safe Move Semantics

A move must never delete the source before the destination is safely committed.

### Same-filesystem moves

Use native rename operations where appropriate, respecting filesystem boundaries, overwrite policy, open-file behavior, and the desired semantics.

### Cross-filesystem moves

1. Copy into a temporary destination.
2. Verify the destination according to the integrity policy.
3. Commit the destination.
4. Persist the destination-committed state.
5. Remove the source only after destination commitment is confirmed.
6. Record the final outcome.

If source deletion fails, preserve the valid destination and report a partial move.

If the application crashes after committing the destination but before deleting the source, recover without deleting either copy prematurely.

If a source file changes during transfer, do not remove the changed source based on an outdated snapshot.

Never report a move as completely successful if the source remains unexpectedly or the destination is not safely committed.

## 8. Pause, Resume, Cancel, and Retry

Every job must support:

- Pause.
- Resume.
- Cancel.
- Retry failed items.
- Recovery after application restart.
- Recovery after unexpected process termination or power loss.

Pause should prevent new tasks from starting and let in-flight work reach safe checkpoints. Cancellation must not expose incomplete files as completed files.

Support two levels of resumability.

**Job-level resume**

- Restore directory-scanning progress.
- Skip files already verified and committed.
- Revalidate relevant source and destination state.
- Continue unfinished files and recoverable tasks.

**File-level resume**

- Record the temporary destination, source identity, size, modification metadata, checkpoint offset, and verification information.
- Validate the source and temporary file before resuming.
- Checkpoint at safe boundaries.
- Avoid trusting stale or inconsistent offsets.
- Revalidate the completed output before publication.
- Restart an individual file safely if its checkpoint is invalid.

Use bounded exponential backoff and jitter for retryable failures. Classify permanent errors such as invalid paths and persistent permission denial so they do not cause infinite retries.

Display honest job states: queued, scanning, copying, paused, verifying, finalizing, completed, partially completed, cancelled, and failed.

## 9. Durable Job Journal

Implement persistent, versioned job state.

Evaluate SQLite or a simpler journal format based on recovery guarantees, transaction overhead, portability, and performance.

Requirements:

- Crash-safe updates.
- Schema versioning.
- Validation of restored state.
- Recovery from incomplete journal writes.
- Safe handling of stale paths.
- Durable recording of destination commitment before destructive source deletion.
- Bounded memory usage.
- Efficient handling of very large directory trees.
- Cleanup of abandoned temporary files without deleting user data.

If using SQLite, select a suitable driver or binding and configure transaction and durability behavior deliberately.

Batch journal updates where safe. Do not synchronize storage after every tiny file unnecessarily, but never weaken the durability guarantees of destructive operations without documenting the consequences.

Recovery must be idempotent: repeating it should not corrupt files or delete valid data.

## 10. High-Performance Small-File Engine

Treat millions of small files as a first-class workload.

Optimize:

- Directory traversal.
- Metadata access.
- File creation and closure.
- Bounded task scheduling.
- Destination directory creation.
- Journal write amplification.
- Path allocation and string processing.
- Verification overhead.
- Lock contention.
- Progress-event frequency.
- Memory allocation and garbage-free or low-allocation hot paths where useful.

Use a bounded producer-consumer pipeline.

Do not scan the entire tree into an unbounded in-memory collection. Discover and schedule work incrementally.

Use separate scanning, transfer, verification, and finalization stages only when profiling justifies the added complexity.

Implement configurable concurrency and sensible defaults. Consider adaptive scheduling based on queue depth, throughput, storage latency, and observed errors.

Avoid one thread or task per file without a strict bound.

Minimize unnecessary file synchronization while retaining the chosen safety guarantees.

## 11. Large-File Performance

- Use streaming I/O and bounded buffers.
- Select buffer sizes through benchmarks.
- Reuse buffers safely.
- Avoid redundant copies.
- Evaluate native fast-copy APIs.
- Support portable fallbacks.
- Consider parallel chunked copying only when benchmarks demonstrate benefits and recovery remains correct.
- Avoid unbounded buffering.
- Keep verification streaming.
- Handle files larger than 4 GiB correctly wherever the target filesystem supports them.

Measure the effect of concurrency, buffering, verification, and durability independently.

## 12. Memory, Thread, and Resource Management

Bound:

- Active transfers.
- Open file handles.
- Queued work.
- Memory buffers.
- Simultaneous verification tasks.
- Journal batches.
- Progress events.
- Background worker counts.

Ensure every worker, task, lock, channel, handle, and temporary file has a clear lifecycle.

Avoid deadlocks, race conditions, unbounded queues, runaway retries, busy loops, and retained references.

Support graceful shutdown and cancellation. Filesystem calls that cannot be interrupted immediately must be allowed to reach a safe state.

For Rust, minimize `unsafe` code and isolate any necessary FFI. For C, enforce strict ownership conventions, checked arithmetic, cleanup paths, and sanitizer coverage.

## 13. Filesystem-Aware Optimization

Support filesystem types available on the operating systems and mounted storage, subject to actual platform capabilities.

Examples include:

- Windows: NTFS, ReFS, FAT32, exFAT.
- Linux: ext4, XFS, Btrfs, F2FS, ZFS, tmpfs, overlayfs.
- macOS: APFS, HFS+, exFAT.
- FreeBSD: UFS and ZFS.

Also handle supported SMB/NFS shares, removable drives, virtual filesystems, container overlay filesystems, and other mounted filesystems.

These are compatibility targets, not guarantees that every filesystem is available on every OS.

### 13.1 Capability detection

Determine, where possible:

- Filesystem identity and type.
- Same-volume versus cross-volume transfers.
- Atomic rename and replacement semantics.
- Filename restrictions and maximum file sizes.
- Case sensitivity and Unicode behavior.
- Symbolic links and other reference types.
- ACLs, permissions, extended attributes, and alternate data streams.
- Sparse files and hard links.
- Timestamp precision.
- File-locking behavior.
- Flush and durability semantics.
- Network and removable-device failure behavior.

Use capability detection rather than relying exclusively on filesystem names. Fall back safely when information is unavailable.

### 13.2 Workload-specific strategies

**SSD and NVMe:** optimize bounded concurrency and metadata throughput.

**HDD:** avoid excessive random I/O and uncontrolled parallelism.

**USB and removable storage:** handle disconnections, capacity exhaustion, and checkpoint recovery.

**Network shares:** use conservative bounded concurrency, handle transient network failures, and respect remote filesystem semantics.

**Copy-on-write filesystems:** evaluate reflinks or native cloning where supported, without confusing shared extents with independent physical copies.

**Sparse files:** preserve sparse allocation when supported and requested.

**Memory-backed and virtual filesystems:** do not assume conventional disk durability or physical storage capacity.

### 13.3 Native fast-copy and clone operations

Evaluate platform-appropriate facilities, including:

- Linux `copy_file_range` and other supported kernel-assisted operations.
- Windows native file-copy capabilities.
- macOS cloning or optimized copy operations where available.
- FreeBSD-specific facilities when supported.

Validate the semantics and limitations of each operation. Fall back to bounded streaming I/O whenever native behavior is unsuitable.

Use optimizations only when their preconditions are satisfied.

### 13.4 Metadata policy

Define and document behavior for:

- Permissions and ownership.
- Modification timestamps.
- ACLs and extended attributes.
- Windows alternate data streams.
- Symbolic links, junctions, and reparse points.
- Hard-link relationships.
- Sparse allocation.
- Unsupported metadata.

Report what could not be preserved. Do not silently claim a complete metadata-preserving copy when only file contents were copied.

## 14. Windows-First Native Integration

Windows is a first-class platform and must receive dedicated performance and integration work.

### 14.1 Windows filesystem optimization

Use supported native Windows APIs where beneficial.

Evaluate:

- Efficient directory enumeration.
- File handles and sharing modes.
- NTFS-aware metadata handling.
- Fast-copy operations.
- File flushing and write-through behavior.
- Atomic replacement and rename.
- File locking and sharing violations.
- Windows permissions and ACLs.
- Reparse points and junctions.
- Sparse files and supported alternate data streams.

Use platform-specific code and validate every optimization against the selected Windows versions and filesystem.

Do not assume every Windows volume uses NTFS.

### 14.2 Windows Explorer clipboard workflow

Implement a reliable Explorer workflow for `Ctrl+C`, `Ctrl+X`, and `Ctrl+V`.

Desired experience:

1. Select files or directories in Explorer.
2. Press `Ctrl+C` to copy or `Ctrl+X` to request a move.
3. Navigate to the destination and press `Ctrl+V`.
4. Tufan receives or detects the relevant operation, resolves source and destination context, and starts the transfer through its own engine.
5. Progress, conflicts, pause/resume, and recovery are available in Tufan.

Investigate Windows clipboard formats such as `CF_HDROP`, shell copy/cut formats, and the supported Windows shell interfaces needed to obtain paste context.

A clipboard path list alone does not reliably reveal the destination or prove that a paste operation occurred.

If reliable interception requires a shell extension or another native component:

- Implement it as an optional, documented component.
- Require explicit user consent for installation.
- Provide installation, upgrade, and removal procedures.
- Avoid global keyboard hooks where a safer supported integration exists.
- Never claim universal paste interception unless it is implemented and tested.

Provide a reliable fallback: import clipboard file paths into Tufan, choose a destination, select copy or move, and explicitly start the operation.

Do not modify or clear the user's clipboard unexpectedly.

### 14.3 Windows system tray

Tufan must continue running in the system tray when the main window is hidden.

Provide:

- Tray icon.
- Open Tufan.
- Show active jobs.
- Pause All.
- Resume All.
- Exit.
- Optional concise progress notifications.
- Configurable close-to-tray behavior.
- Reliable window restoration.
- Single-instance coordination.
- Background job continuity.
- Safe application shutdown.

Use a tray implementation compatible with the selected UI toolkit and supported Windows versions.

Active jobs must not depend on the visibility of the main window.

### 14.4 Run at login

Provide optional per-user startup at login.

- Make the setting easy to enable or disable.
- Avoid requiring administrator privileges for ordinary per-user startup.
- Prevent duplicate startup registrations.
- Handle executable relocation.
- Restore unfinished jobs safely.
- Do not install a Windows service unless a separate service mode is explicitly required.

## 15. Minimal and Relaxing UI

Create a clean, lightweight interface named **Tufan**.

### Visual design

- Neutral background.
- Restrained accent color.
- Clear typography.
- Comfortable spacing.
- Subtle borders and separators.
- Minimal animation.
- Light and dark themes.
- Native-feeling controls.
- No unnecessary gradients, decorative cards, or clutter.

The UI should feel calm, responsive, and polished rather than flashy.

### Main window

Include:

1. Tufan branding.
2. Source selection.
3. Destination selection.
4. Copy and Move actions.
5. Active job list.
6. Overall progress summary.
7. Compact settings access.

Use appropriate native file dialogs wherever feasible.

### Job display

Show:

- Source and destination.
- Current state.
- Progress bar.
- Files completed versus total discovered.
- Bytes completed versus known total.
- Current transfer speed.
- Estimated remaining time when meaningful.
- Pause/resume, cancel, and retry controls.
- Clear, actionable errors.

Represent unknown totals honestly while scanning. Avoid unstable progress percentages and misleading ETAs.

### UI responsiveness

- Never block rendering on filesystem operations.
- Keep hashing, directory scanning, and synchronization off the UI thread.
- Aggregate progress updates.
- Avoid per-buffer or per-file rendering events.
- Ensure large job lists remain responsive.
- Support keyboard navigation and accessible labels.
- Keep tray operations available while the main window is hidden.

## 16. Cross-Platform Operating Systems

Support Windows, Linux, macOS, and FreeBSD.

### Windows

- Windows 10 and Windows 11.
- Native filesystem operations and Explorer integration.
- Tray support.
- NSIS installer.

### Linux

Target major distributions, including:

- Debian and Ubuntu.
- Fedora.
- Arch Linux.
- openSUSE.

Avoid hard dependencies on a single desktop environment. Account for Wayland/X11 differences, native file dialogs, clipboard behavior, and tray support.

### macOS

- Apple Silicon and Intel Macs.
- Standard application bundles.
- Native filesystem integration.
- Menu-bar or tray behavior where appropriate.
- APFS and supported mounted filesystems.
- Optional signing and notarization for official releases.

### FreeBSD

- Prioritize validated FreeBSD architectures.
- Use portable implementations by default.
- Add BSD-specific optimizations only when supported and tested.
- Validate filesystem and desktop integration separately.

Do not assume a feature available on Linux is also available on FreeBSD.

## 17. CPU Architecture Matrix

Target the relevant CPU architectures supported by the complete language, UI, graphics, and dependency stack.

| Operating system | Target architectures |
|---|---|
| Windows | x86-64, ARM64, and 32-bit x86 where practical |
| Linux | x86-64, ARM64, 32-bit x86, ARM32, and additional supported architectures where feasible |
| macOS | Apple Silicon ARM64 and Intel x86-64 |
| FreeBSD | x86-64 and ARM64 where the complete dependency stack supports them |

Treat these as target goals, not assumed compatibility.

For each target, distinguish:

- Compiled.
- Successfully packaged.
- Tested.
- Officially supported.

Use native CI runners when required for UI compilation, signing, packaging, and platform testing. Cross-compile where reliable and practical.

Never advertise a platform/architecture combination as supported merely because the core engine compiles.

## 18. Packaging and Distribution

Create reproducible release artifacts for validated targets.

### 18.1 Windows NSIS installer

Build a professional NSIS installer with:

- Per-user installation.
- Optional machine-wide installation.
- Start Menu shortcuts.
- Optional desktop shortcut.
- Application icon and version information.
- Optional startup-at-login configuration.
- Optional Explorer integration.
- Upgrade support.
- Preservation of user settings and recovery journals.
- Safe handling of running instances.
- Clean uninstallation.
- Removal of registered shell integrations and startup entries.
- Protection against accidental deletion of user data.

Generate architecture-appropriate packages and label them clearly.

Never silently enable shell extensions or startup behavior.

### 18.2 Linux

Provide appropriate artifacts where supported:

- `.deb` for Debian/Ubuntu.
- `.rpm` for Fedora-compatible distributions.
- Arch Linux PKGBUILD or `.pkg.tar.zst`.
- AppImage where feasible.
- Flatpak where appropriate and compatible with required filesystem access.

Document sandbox limitations and native integration requirements.

### 18.3 macOS

Provide:

- `.app` bundles.
- Separate ARM64 and Intel builds or a universal binary when practical.
- `.dmg` distribution where appropriate.
- Optional `.pkg` installers.
- Code signing and notarization when the necessary credentials and infrastructure are available.

### 18.4 FreeBSD

Provide suitable standalone binaries, archives, packages, or ports based on the validated target environment.

### 18.5 Release engineering

Automate builds, tests, packaging, versioning, checksums, and release artifact generation.

Use reproducible build settings where feasible. Include license notices and third-party dependency information.

Never report signing, notarization, packaging, or testing as complete when required credentials, infrastructure, or validation are unavailable.

## 19. Security and Safe Filesystem Handling

- Treat paths and persisted journal entries as untrusted or potentially stale.
- Prevent path traversal.
- Avoid unsafe recursive deletion.
- Avoid unintended symbolic-link traversal.
- Handle TOCTOU risks where practical.
- Protect user data during overwrites and recovery.
- Avoid logging unnecessary sensitive path information.
- Validate paths before destructive actions.
- Handle concurrent jobs targeting the same destination.
- Detect source modification during transfer.
- Avoid overwriting unrelated files during recovery.
- Document limitations for network shares, removable drives, and filesystems with weaker durability guarantees.

## 20. Testing and Quality Assurance

Implement unit, integration, stress, and fault-injection tests.

Cover:

- Empty files and directories.
- Arbitrary binary contents.
- Unicode and long filenames.
- Deep directory trees.
- Millions of small files.
- Large files.
- Destination conflicts.
- Case and Unicode filename collisions.
- Source modification during transfer.
- Permission failures.
- Disk-full conditions.
- Locked files.
- Removable-device disconnection.
- Network interruptions.
- Unsupported metadata.
- Symbolic links, hard links, junctions, and reparse points.
- Pause, resume, cancellation, and retry.
- Crash recovery.
- Invalid journal data.
- Short writes and failed synchronization.
- Destination publication failures.
- Source deletion failures during moves.
- Concurrent jobs and duplicate destinations.
- Memory and handle exhaustion.
- Filesystem-specific edge cases.

Use deterministic fault injection to simulate errors at every critical stage.

Verify file contents independently using hashes. Ensure incomplete transfers cannot appear completed and recovery does not delete valid source data.

### Language-specific checks

For Rust:

- `cargo test`
- `cargo clippy --all-targets --all-features`
- Appropriate formatting checks.
- Fuzzing and Miri where applicable.
- Sanitizer and platform-specific checks where supported.

For C, if selected:

- Unit and integration tests.
- AddressSanitizer.
- UndefinedBehaviorSanitizer.
- Leak detection where supported.
- Static analysis.
- Concurrency testing and fuzzing.

For all builds, run platform-appropriate integration tests and validate the release artifacts themselves.

## 21. Performance Benchmarking

Build repeatable benchmarks for:

1. Millions of tiny files.
2. Tens of thousands of mixed-size files.
3. Large sequential files.
4. Deeply nested directory trees.
5. Multiple simultaneous jobs.
6. Slow rotational storage.
7. NVMe storage.
8. USB and removable storage.
9. Network shares where available.
10. Interrupted and resumed transfers.
11. Full content verification.
12. Low-memory environments.

Measure:

- Files per second.
- Bytes per second.
- Total completion time.
- Peak resident memory.
- CPU usage.
- Allocation behavior.
- Context switching and synchronization overhead where practical.
- Open file handles.
- Recovery time.
- Verification overhead.
- Startup time.
- Idle resource consumption.
- Packaged binary size.

Compare against appropriate platform-native tools under comparable conditions.

Record hardware, operating system, filesystem, dataset, cache conditions, concurrency, and integrity settings.

Do not fabricate benchmark results or claim universal performance superiority.

## 22. Implementation Phases

### Phase 1 — Inspect and decide

Inspect the repository, evaluate C versus Rust and UI options, document the decisions, and establish the build.

### Phase 2 — Core engine

Implement safe single-file copying, destination publication, move semantics, error handling, cancellation, and independent integrity tests.

### Phase 3 — High-performance directory engine

Implement incremental traversal, bounded scheduling, small-file optimization, progress aggregation, and resource limits.

### Phase 4 — Recovery and resumability

Implement the durable journal, file checkpoints, crash recovery, verification, and safe retry behavior.

### Phase 5 — UI

Build the main interface, file pickers, job management, pause/resume controls, progress display, themes, and keyboard interactions.

### Phase 6 — Windows integration

Implement tray behavior, startup-at-login configuration, clipboard workflows, Explorer integration where feasible, and the NSIS installer.

### Phase 7 — Filesystem and platform optimization

Implement capability detection, native fast-copy paths, filesystem-aware behavior, and safe fallbacks.

### Phase 8 — Cross-platform release

Build, test, package, and document supported OS/architecture combinations.

### Phase 9 — Final validation

Run automated tests, fault injection, sanitizers or equivalent tools, stress tests, and reproducible benchmarks. Fix critical issues before declaring the release ready.

Maintain and update `TODO.md` as work progresses.

## 23. Definition of Done

Tufan's initial release is complete only when:

- The selected language and UI architecture are implemented.
- File and directory copy/move operations work correctly.
- Data-integrity and safe-move guarantees are documented.
- Jobs can be paused, resumed, cancelled, and recovered.
- Memory usage and worker concurrency are bounded.
- Millions of small files can be processed without loading the entire tree into memory.
- The UI remains responsive during heavy operations.
- Windows tray behavior and the supported Explorer workflow work as implemented.
- Filesystem capability detection and safe fallbacks are tested.
- Supported target builds compile and launch.
- The Windows NSIS installer installs, upgrades, and uninstalls safely.
- Linux, macOS, and FreeBSD artifacts are provided for validated targets.
- Tests and platform-specific checks pass.
- Benchmarks are reproducible.
- Known limitations are documented honestly.

## 24. Final Instructions to the CLI Agent

Work directly in the current repository. Inspect before changing. Make defensible engineering decisions independently.

Prioritize:

1. User data safety.
2. Correct recovery and integrity.
3. Memory safety and concurrency correctness.
4. Real-world small-file throughput.
5. Large-file throughput.
6. Cross-platform behavior.
7. Smooth and minimal UX.
8. Reliable packaging and maintainability.

Do not overengineer. Do not introduce dependencies without a clear reason. Do not substitute mock implementations for core functionality. Do not claim completion based on compilation alone.

Implement, test, benchmark, document, and package the real application incrementally.

**Build Tufan as a reliable, high-performance, native-feeling file transfer utility that users can trust with important data.**
