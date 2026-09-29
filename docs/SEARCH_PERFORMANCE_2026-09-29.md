# Search and performance investigation — 2026-09-29

This is a warm-cache, read-only query benchmark of the local Pathfinder index on one Windows profile. `tools/bench_search_index.py` prints aggregate counts, times and query plans without exposing paths or file names. Seven runs per query use the median. It is a targeting baseline, not an end-to-end application speedup claim.

| Operation | Median on 30,001 paths | SQLite plan / observation |
| --- | ---: | --- |
| Exact path | 0.01 ms | Existing primary-key B-tree seek |
| Exact parent | 0.01 ms | Existing parent index seek |
| Name prefix | 0.01 ms | Existing name index range seek |
| Name contains | 11.47 ms | Full `files` scan |
| Existing scoped token FTS | 0.05 ms | FTS posting lookup plus path join; token semantics did not cover all path/substrings |
| In-memory trigram name contains prototype | 0.08 ms | FTS5 indexed `LIKE`; 664.9 ms build and 27.4 MiB memory DB |
| In-memory trigram name-or-path prototype | 0.14 ms | `MULTI-INDEX OR` over two FTS5 columns |
| In-memory full path map | 176.5 ms load, 11.2 MiB Python peak | 0.04 ms for 1,000 exact dict lookups after loading |

The application now uses a trigram FTS5 candidate index for literal name and path substrings of at least three ASCII characters. It falls back to a scan for shorter and special-character terms, preserving the reference predicate's behavior. The candidate is followed by one shared metadata/query predicate; the result cap is applied **after** that predicate. This fixes the case where many early candidates failed a compound filter and hid a later valid result. The legacy name-only FTS table has been removed from active writes. A test corpus checks compound filters after many decoys and path-only matches.

The SQLite file should remain an index. A hash map gives fast in-process exact lookup after an upfront load, but the existing on-disk B-tree already makes exact lookups cheap without duplicating every path in RAM or rebuilding a map on launch. A Dijkstra shortest-path search would traverse weighted graph edges; Pathfinder's filename/content queries need term-to-file postings and metadata predicates, not a shortest route between nodes. The folder hierarchy is useful for scope pruning, but turning it into a weighted graph does not improve substring retrieval.

The new opt-in content index stores only supported documents under explicitly added roots. It fingerprints size and high-resolution modified time, avoids re-extracting unchanged files, caps source and extracted text sizes, caps total stored text, and uses a separate trigram FTS5 table for substring candidates. Optional local OCR uses installed Windows languages, with pixel and PDF-page budgets. Content queries refresh opted-in roots in a worker and use a live scan where coverage is incomplete. Results include text snippets; the UI reports indexed roots and skipped files. This still needs cold-cache and larger-corpus timing, OCR-language fixtures, and UI latency measurements before calling it faster overall.

Remaining measured work from the audit is a coverage table for the general file index, so ordinary searches can safely skip redundant Windows Search/live providers when a scope is known complete; bounded scheduling for rapid successive queries; avoiding AI embeddings and thumbnail writes for unchanged files; and p95/p99 UI model-update timing. Record disk type, cold/warm state, directory shape, first-result latency, complete-result latency, filesystem bytes read, cancellation delay, SQLite write volume, and peak RAM on representative 1k/10k/50k folders and larger trees. The present 30k-path baseline does not answer those questions.

SQLite's [query planner](https://www.sqlite.org/queryplanner.html), [EXPLAIN QUERY PLAN](https://www.sqlite.org/eqp.html), and [FTS5 trigram tokenizer](https://www.sqlite.org/fts5.html) document the indexes and operator behavior used here. The trigram `LIKE` optimization applies to eligible patterns; short patterns and escaped patterns need a fallback.
