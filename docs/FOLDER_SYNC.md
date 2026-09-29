# One-way folder sync

Choose **Tools → Sync Folder** (or run `Sync Folder` from the command palette). The active pane is the source. Enter the destination folder; when dual pane is open, Pathfinder suggests the other pane. Both folders must already exist and neither may contain the other.

The first step is a read-only preview. It lists copies, updates, new folders, destination-only items, and conflicts with their reasons. Enter comma-separated exclusions such as `*.tmp, node_modules/**, cache` and preview again. Patterns without a slash match any path component; `folder/**` excludes that folder and its contents. **Delete destination-only items** starts off. Changing exclusions or the deletion toggle invalidates the preview until you rebuild it.

Choose **Apply preview** to run the reviewed one-way plan. Pathfinder checks the source and destination again before each change. A destination file newer than its source, a type mismatch, a linked path, or a case-only name mismatch is a conflict and is skipped. An item changed since preview is skipped with an error. Stop sync to finish the current checkpoint and leave completed items intact.

Before updating or deleting a destination item, Pathfinder moves its previous version to a per-run folder beside the destination, under `.pathfinder-sync-history`. An empty deleted folder is preserved there too. The **Open recovery folder** button opens that run's folder, containing `report.json`, `events.jsonl`, and `versions/`. If a run stops before its report can be completed, the button opens the history folder so you can inspect its run folders. To recover an item, inspect the report and copy its version back to the desired location. Check the current destination before restoring so a later edit is not overwritten.

After a successful run, fingerprints are cached beside the reports. A second preview of unchanged files avoids repeat hashing and an unchanged Apply transfers no files. The first preview may hash matching files to establish that baseline. Each source or destination tree is limited to 200,000 entries; narrow larger jobs with exclusions. History is retained until you remove it manually.

The cache trusts file kind, size, creation time, and modification time. An external tool that changes bytes while preserving all of those values may make a later preview say “same.” For a strict check after such a tool runs, wait for any sync to finish, then remove the destination history folder's `fingerprints.json` before previewing again; Pathfinder will hash common files again. The dialog shows at most 500 preview rows, with conflicts and changes first; it does not yet offer a full pre-apply export.

This release supports reviewed **one-way** sync only. It does not schedule jobs, detect renames, automatically restore versions, or perform two-way merging. A crash can leave a staged or versioned file; inspect the run folder and report before retrying. The report and version files are deliberately kept for recovery.

Copies preserve the default file contents and modification time, but this version does not promise full Windows metadata fidelity: alternate data streams, ACLs, creation times, hard-link identity, and every file attribute are not explicitly copied. Links and reparse points are conflicts for manual review. Use a dedicated metadata-preserving backup tool when those properties matter.
