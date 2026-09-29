"""Read-only Pathfinder index probe. Prints aggregate timings, never file names."""

from __future__ import annotations

import os
import sqlite3
import statistics
import time
import tracemalloc
from pathlib import Path


def median_query(conn: sqlite3.Connection, sql: str, args: tuple[object, ...]) -> tuple[float, int]:
    samples = []
    rows = 0
    for _ in range(7):
        start = time.perf_counter()
        rows = len(conn.execute(sql, args).fetchall())
        samples.append((time.perf_counter() - start) * 1000)
    return statistics.median(samples), rows


def main() -> None:
    db = Path(os.environ["APPDATA"]) / "Pathfinder" / ".pathfinder-index.sqlite3"
    if not db.is_file():
        print("No local Pathfinder index to benchmark.")
        return
    conn = sqlite3.connect(db.as_uri() + "?mode=ro", uri=True)
    conn.execute("PRAGMA query_only = ON")
    count = conn.execute("SELECT count(*) FROM files").fetchone()[0]
    size_mib = db.stat().st_size / (1024 * 1024)
    sample = conn.execute("SELECT path, parent FROM files LIMIT 1").fetchone()
    print(f"Index: {count:,} paths, {size_mib:.1f} MiB")
    if not sample:
        return
    sample_path, sample_parent = sample
    root = Path(sample_path).anchor or sample_parent
    cases = [
        ("exact path", "SELECT path FROM files WHERE path = ? LIMIT 1", (sample_path,)),
        ("exact parent", "SELECT path FROM files WHERE parent = ? LIMIT 1200", (sample_parent,)),
        ("name prefix", "SELECT path FROM files WHERE name LIKE ? LIMIT 1200", ("report%",)),
        ("name contains", "SELECT path FROM files WHERE name LIKE ? LIMIT 1200", ("%report%",)),
        (
            "current scoped FTS",
            "SELECT f.path FROM files f JOIN files_fts ON files_fts.path = f.path "
            "WHERE LOWER(f.path) LIKE LOWER(?) ESCAPE '\\' AND files_fts MATCH ? LIMIT 1200",
            (root.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%", "report*"),
        ),
    ]
    for label, sql, args in cases:
        try:
            median_ms, rows = median_query(conn, sql, args)
            plan = " | ".join(row[3] for row in conn.execute("EXPLAIN QUERY PLAN " + sql, args))
            print(f"{label}: {median_ms:.2f} ms median; {rows} rows; {plan}")
        except sqlite3.Error as error:
            print(f"{label}: unavailable ({error})")

    # A Python dict illustrates the up-front memory/load cost of a full map;
    # its per-key lookup is not a replacement for persisted SQL/FTS queries.
    tracemalloc.start()
    start = time.perf_counter()
    path_map = {path: (name, extension) for path, name, extension in conn.execute(
        "SELECT path, name, extension FROM files"
    )}
    load_ms = (time.perf_counter() - start) * 1000
    _, peak = tracemalloc.get_traced_memory()
    start = time.perf_counter()
    for _ in range(1000):
        _ = path_map.get(sample_path)
    map_us = (time.perf_counter() - start) * 1000
    print(f"Full path map: {load_ms:.1f} ms load, {peak / (1024 * 1024):.1f} MiB Python peak, {map_us:.2f} ms / 1000 exact lookups")

    # Test a candidate substring index without changing the live database.
    try:
        tri = sqlite3.connect(":memory:")
        tri.execute("CREATE VIRTUAL TABLE names_tri USING fts5(name, path, tokenize='trigram')")
        start = time.perf_counter()
        tri.executemany("INSERT INTO names_tri(name, path) VALUES(?, ?)", conn.execute("SELECT name, path FROM files"))
        tri.commit()
        build_ms = (time.perf_counter() - start) * 1000
        median_ms, rows = median_query(tri, "SELECT path FROM names_tri WHERE name LIKE ? LIMIT 1200", ("%report%",))
        tri_mib = tri.execute("PRAGMA page_count").fetchone()[0] * tri.execute("PRAGMA page_size").fetchone()[0] / (1024 * 1024)
        print(f"Trigram candidate: {build_ms:.1f} ms build, {tri_mib:.1f} MiB memory DB, {median_ms:.2f} ms median contains, {rows} rows")
        for label, sql in [
            ("trigram name or path", "SELECT path FROM names_tri WHERE name LIKE ? OR path LIKE ? LIMIT 1200"),
            ("trigram union", "SELECT path FROM names_tri WHERE name LIKE ? UNION SELECT path FROM names_tri WHERE path LIKE ? LIMIT 1200"),
        ]:
            ms, rows = median_query(tri, sql, ("%report%", "%report%"))
            plan = " | ".join(row[3] for row in tri.execute("EXPLAIN QUERY PLAN " + sql, ("%report%", "%report%")))
            print(f"{label}: {ms:.2f} ms median; {rows} rows; {plan}")
        tri.close()
    except sqlite3.Error as error:
        print(f"Trigram candidate: unavailable ({error})")
    conn.close()


if __name__ == "__main__":
    main()
