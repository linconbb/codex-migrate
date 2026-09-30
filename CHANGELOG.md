## 1.0.10 - 2026-10-01

- Added full on-demand session transcript preview before selective export, showing user, assistant, and tool messages with metadata.
- Guardian and other internal/subagent sessions are hidden from selective export by default, with an advanced toggle to reveal them.
- Preserved structured Codex session `source` metadata so subagent sessions such as `{"subagent":{"other":"guardian"}}` are classified correctly.
- Improved fallback session titles by ignoring injected `AGENTS.md` / `<environment_context>` blocks and extracting the real VS Code "My request for Codex" prompt.
- Added cross-platform regression tests for internal-session filtering, title extraction, and transcript parsing.

## 1.0.9 - 2026-10-01

- Added importable selective Codex backups: scan local projects/sessions and export only selected conversations while preserving `sessions/` and `archived_sessions/` layout.
- Added CLI support for repeated `export --thread <THREAD_ID>` selection.
- Kept full backup behavior unchanged.
- Updated GUI numeric stroke literals for Rust 1.98 Clippy compatibility.
- Linux x64 GitHub Actions builds now publish/update a dedicated release automatically when source or Cargo metadata changes.

# Changelog

All notable changes to this project will be documented here.

The project uses semantic versioning where practical.

## [Unreleased]

- Export backups as complete `.codex` directory copies instead of minimal
  session-only folders, preserving databases, settings, Skills, logs, caches,
  and other local contents while excluding root-level login credential files.
- Name exported folders `Codex_backup` so they remain visible on macOS and Linux.

## [1.0.7] - 2026-06-21

- Use a zero-pixel optical text offset on Windows while retaining the existing macOS/Linux alignment.
- Embed the project icon and version metadata into Windows executables.
- Hide the console window when launching the Windows release GUI.

## [1.0.6] - 2026-06-21

- Added multi-select deletion for rollback snapshots.
- Added a default-selected tri-state “Select all” control for rollback records.
- Embedded user images and tool screenshots directly in exported single-file HTML.
- Added completion dialogs for import, backup export, HTML export, and path repair.
- Added Chinese and English interfaces with system-language detection.
- Added path repair, parent-directory mapping, and HTML conversation export.
- Added selective project/session import, conflict preview, transactional rollback, and Codex Desktop project registration.
