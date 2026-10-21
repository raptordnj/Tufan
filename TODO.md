# Tufan TODO

## Milestone 1
- [x] Workspace + release profile
- [x] Core: errors, policies, control (pause/cancel)
- [x] Core: safe file copy/publish with verification + checkpoints
- [x] Core: incremental scanner, bounded worker pool
- [x] Core: move semantics (same-fs rename, cross-fs copy-verify-delete)
- [x] Core: SQLite journal, resume (checkpointed temps reused; stale temp sweep not wired yet)
- [x] Tests: integration + fault injection
- [x] CLI
- [x] GTK 4 UI
- [x] Packaging: NSIS script, PKGBUILD, .desktop + icon
- [ ] deb/rpm/AppImage/Flatpak build recipes
- [x] Benchmarks script
- [x] README with guarantees and limitations

## Later
- [x] Windows tray, run-at-login, clipboard import (CF_HDROP), Explorer paste takeover (keyboard hook, opt-in)
- [ ] ACL / xattr / ADS / sparse preservation
- [ ] macOS .app/.dmg, signing/notarization
- [ ] FreeBSD validation
- [ ] CI matrix for all OS/arch targets
- [x] Tray icon on Windows (Shell_NotifyIcon)
- [ ] Tray icon on Linux (StatusNotifierItem)
- [ ] Explorer right-drag "Copy/Move with Tufan" (needs a shell extension)
- [ ] Explorer paste on the desktop and in virtual folders
- [x] Run-at-login toggle in the UI (tray menu)
- [ ] Abandoned temp cleanup on resume across all destination dirs (`engine::clean_temps` exists, not wired)
- [ ] Settings persistence in the UI
- [ ] Cold-cache, HDD, USB and network benchmarks
- [ ] Fuzzing of journal decoding; Miri on pure modules
- [x] Linux/macOS file manager actions (Tufan Copy / Cut / Paste with Tufan)
- [ ] Thunar custom actions installed automatically
