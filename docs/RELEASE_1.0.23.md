# Pathfinder 1.0.23

This release adds reviewed, one-way folder synchronization. Choose **Tools → Sync Folder**, select a destination, and inspect the proposed copies, updates, conflicts, and destination-only items before applying. Destination-only deletion starts off. Exclusions, cancellation, a per-run report, and previous versions under `.pathfinder-sync-history` help you review and recover changes. Conflict reasons and failures appear first in the dialog.

Search help now has examples you can click to run. This release also includes targeted fixes for path-search filtering, dual-pane refresh and command targeting, safer file replacement and redo behavior, search scope and index consistency, duplicate-scan targeting, and several malformed-input and AI catalog checks.

Folder sync is one-way and does not schedule jobs or merge edits from both sides. Recovery from history is manual. It mirrors ordinary file contents and modification time; it does not guarantee preservation of Windows ACLs, alternate data streams, creation times, or hard-link identity. See [the folder-sync guide](https://github.com/rorohecker/pathfinder/blob/v1.0.23/docs/FOLDER_SYNC.md) for the workflow and limits.

Validation on Windows: 76 library tests passed; `cargo check` and strict Clippy passed. The [review](https://github.com/rorohecker/pathfinder/blob/v1.0.23/docs/REVIEW_2026-09-28.md) records remaining runtime, accessibility, crash-recovery, and large-tree checks.
