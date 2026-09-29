# Audit implementation status — 2026-09-28

This tracks `AUDIT_2026-09-28.md` against the current working tree. "Addressed" means the identified defect has a targeted code change; it does not imply the full validation matrix in the audit has run. "Partial" identifies a meaningful fix with remaining requirements. The first proposed feature now has a reviewed one-way implementation; the other two remain plans.

| Finding | Status | Remaining work |
| --- | --- | --- |
| 1 Selection after sort | Partial | Store stable file identity independent of row indices; exercise all sort/filter/pane combinations. |
| 2 Wrong pane command targets | Addressed | UI exercise with two panes. |
| 3 Redo delete recovery | Addressed | Fault-injection cycle through Recycle Bin. |
| 4 Undo/redo transaction integrity | Partial | Atomic batch journal, partial-failure recovery, external edit preconditions. |
| 5 Redo copy parity | Partial | Reuse full validated operation engine and durable decisions. |
| 6 Safe replacement | Partial | Directory merge transaction and fault injection at every stage. |
| 7 Descendant paste | Addressed | Reparse/UNC and cross-volume runtime matrix. |
| 8 Archive containment | Partial | Adversarial Windows junction/reparse and race testing; report rejected entries. |
| 9 Crash-safe persistence | Partial | Cross-process revision/conflict handling and crash-injection tests. |
| 10 Registry ownership | Open | Preserve previous registrations and remove only still-owned values. |
| 11 Shared search semantics | Partial | Content/tag equivalence and provider pagination after predicates. |
| 12 SQL path scope | Addressed | Broader Windows/UNC fixture coverage. |
| 13 Partial index reconciliation | Addressed | Deep/wide integration fixture across flushes. |
| 14 Search/filter source composition | Partial | Unify directory, collection, and search source models. |
| 15 Scoped composable tags | Open | Typed tag predicate and explicit global collection behavior. |
| 16 Watcher eviction/resume | Partial | Burst coalescing and all watched tabs reconciliation. |
| 17 UI-blocking duplicates | Partial | Cancellation, bounded scheduling, and progressive results. |
| 18 Implicit storage duplicate pass | Addressed | Optional reusable size buckets would improve an explicit duplicate scan. |
| 19 Quadratic image similarity | Addressed | Corpus benchmarks against brute-force oracle. |
| 20 Unchanged-file AI rework | Open | Fingerprinted dirty queue and bounded embedding batches. |
| 21 Redundant search providers | Open | Coverage-aware planner with cancellation. |
| 22 Semantic sample cap | Open | Exact candidate vectors and defined scoped retrieval baseline. |
| 23 Thumbnail DB amplification | Open | Batch worker, touch batching, measured cache stats. |
| 24 Stale thumbnail memory cache | Open | Fingerprinted invalidation and explicit recency. |
| 25 Full UI model rebuilds | Open | Stable row identity and bounded delta updates. |
| 26 Move prewalk | Addressed | Benchmark same-volume rename path. |
| 27 Cancellation/progress/terminal state | Partial | Shared operation context and cancellable external child process. |
| 28 Paste batch conflict resume | Partial | Durable plan and complete decision matrix. |
| 29 Text diff | Open | Bounded worker read, error states, aligned diff. |
| 30 Allocation budgets | Partial | Broader malformed-input and time budgets. |
| 31 Classifier model contract | Addressed | Real model inference fixture on supported hardware. |
| 32 Accelerator truthfulness | Open | Require provider registration, probe execution, qualify mixed assignment. |
| 33 Atomic AI update | Open | Stage coherent version, verify and smoke-test, activate with rollback. |
| 34 Runtime validation | Open | Verify runtime bundle/version/hash and support locked DLL activation. |
| 35 Catalog/locking | Partial | Verify stale owner liveness and harden trusted origins. |
| 36 Offline profile policy | Addressed | Offline profile-switch UI exercise. |
| 37 AI polling/preferences | Partial | Make expensive status checks asynchronous. |
| 38 Tab close targeting | Addressed | UI tab/history/selection matrix. |
| 39 Shortcut conflicts | Open | Structured chords, collision checks, reserved-key capture. |
| 40 Accessibility | Open | Control semantics, keyboard, Narrator/high-contrast/DPI testing. |
| 41 Search help usefulness | Partial | Keyboard-operable examples, responsive layout, translations, accurate coverage text. |
| 42 Shell menu identity | Open | Keep shell context alive through invocation and forward menu messages. |
| 43 Update trust/outcome | Open | Explicit asset contract, mandatory digest, durable install receipt. |
| 44 Architecture/invariant tests | Partial | Operation/query/index interfaces and integration suites. |

## Proposed features

1. Reviewable folder synchronization and versioned backup: **First one-way scope implemented**. See `FOLDER_SYNC.md`; two-way merging, scheduled jobs, automatic restoration, and broader fault-injection remain open.
2. Incremental document-content search and optional OCR: **Open**. Requires unified query semantics and source fingerprinting.
3. Durable transfer and recovery center: **Open**. Requires a journaled operation engine and restart/fault-injection tests.

No performance speedup or end-to-end safety claim is made without the runtime benchmark and fault-injection suites described in the audit.

## Verification

On this Windows checkout, `cargo test --lib` passed all 76 tests, including twelve folder-sync tests. `cargo check` and `cargo clippy --all-targets -- -D warnings` also passed on the final tree. The Slint compiler reports a pre-existing callback-name warning for `close`. The detailed scope and remaining validation are in [REVIEW_2026-09-28.md](REVIEW_2026-09-28.md).
