//! Opt-in, incremental local document index. No folder is indexed until the
//! user adds it; removal deletes both extracted text and FTS postings.
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
#[cfg(windows)]
use std::future::IntoFuture;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, TryLockError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_INDEX_TEXT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SCAN_FILES: usize = 100_000;
static SCAN_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Debug)]
pub struct RootStatus {
    pub path: String,
    pub scanned_at: i64,
    pub indexed: u64,
    pub skipped: u64,
    pub complete: bool,
    pub ocr: bool,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub snippet: String,
}

fn open_at(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let conn = Connection::open(path).map_err(|error| error.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA secure_delete=ON; PRAGMA temp_store=MEMORY;
        CREATE TABLE IF NOT EXISTS roots (
            path TEXT PRIMARY KEY, scanned_at INTEGER NOT NULL DEFAULT 0,
            indexed INTEGER NOT NULL DEFAULT 0, skipped INTEGER NOT NULL DEFAULT 0,
            complete INTEGER NOT NULL DEFAULT 0, ocr INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS documents (
            id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, root TEXT NOT NULL,
            size INTEGER NOT NULL, modified_ns TEXT NOT NULL,
            body TEXT NOT NULL, generation INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_content_root ON documents(root);
        CREATE TABLE IF NOT EXISTS content_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(
            body, tokenize='trigram'
        );",
    )
    .map_err(|error| error.to_string())?;
    let secure: Option<String> = conn
        .query_row(
            "SELECT value FROM content_meta WHERE key='fts_secure_delete'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if secure.as_deref() != Some("1") {
        conn.execute(
            "INSERT INTO content_fts(content_fts, rank) VALUES('secure-delete',1)",
            [],
        )
        .map_err(|error| error.to_string())?;
        conn.execute(
            "INSERT INTO content_meta(key,value) VALUES('fts_secure_delete','1')
            ON CONFLICT(key) DO UPDATE SET value='1'",
            [],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(conn)
}

fn root_key(path: &Path) -> Result<String, String> {
    let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
    if !canonical.is_dir() {
        return Err("Content index root must be a folder".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    let value = if let Some(unc) = value.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        value.strip_prefix("\\\\?\\").unwrap_or(&value).to_string()
    };
    value.to_lowercase()
}

pub(crate) fn path_within(path: &Path, root: &Path) -> bool {
    let path = path_key(path);
    let root = path_key(root);
    path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|tail| root.ends_with('\\') || tail.starts_with('\\'))
}

pub fn add_root(db: &Path, path: &Path) -> Result<(), String> {
    let path = root_key(path)?;
    let conn = open_at(db)?;
    {
        let mut stmt = conn
            .prepare("SELECT path FROM roots")
            .map_err(|error| error.to_string())?;
        let existing = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        for old in existing {
            let old = old.map_err(|error| error.to_string())?;
            if path_key(Path::new(&old)) == path_key(Path::new(&path)) {
                return Ok(());
            }
            if path_within(Path::new(&old), Path::new(&path))
                || path_within(Path::new(&path), Path::new(&old))
            {
                return Err("This folder overlaps an existing content index root".into());
            }
        }
    }
    conn.execute(
        "INSERT OR IGNORE INTO roots(path) VALUES(?1)",
        params![path],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn set_ocr(db: &Path, path: &Path, enabled: bool) -> Result<(), String> {
    let _guard = SCAN_LOCK
        .try_lock()
        .map_err(|_| "Stop the content scan before changing OCR".to_string())?;
    let path = root_key(path)?;
    let mut conn = open_at(db)?;
    let tx = conn.transaction().map_err(|error| error.to_string())?;
    if !enabled {
        let ocr_candidate_ids = {
            let mut stmt = tx
                .prepare("SELECT id,path FROM documents WHERE root=?1")
                .map_err(|error| error.to_string())?;
            let rows = stmt
                .query_map(params![path], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for (id, item_path) in ocr_candidate_ids {
            if matches!(
                super::extension(Path::new(&item_path)).as_str(),
                "pdf" | "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "webp"
            ) {
                tx.execute("DELETE FROM content_fts WHERE rowid=?1", params![id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM documents WHERE id=?1", params![id])
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    tx.execute(
        "UPDATE roots SET ocr=?2, complete=0 WHERE path=?1",
        params![path, enabled as i64],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    if !enabled {
        conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn roots(db: &Path) -> Result<Vec<RootStatus>, String> {
    let conn = open_at(db)?;
    let mut stmt = conn
        .prepare(
            "SELECT path, scanned_at, indexed, skipped, complete, ocr FROM roots ORDER BY path",
        )
        .map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(RootStatus {
                path: row.get(0)?,
                scanned_at: row.get(1)?,
                indexed: row.get::<_, i64>(2)?.max(0) as u64,
                skipped: row.get::<_, i64>(3)?.max(0) as u64,
                complete: row.get::<_, i64>(4)? != 0,
                ocr: row.get::<_, i64>(5)? != 0,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

pub fn remove_root(db: &Path, path: &Path) -> Result<(), String> {
    let _guard = SCAN_LOCK
        .try_lock()
        .map_err(|_| "Stop the content scan before removing a folder".to_string())?;
    let path = root_key(path).unwrap_or_else(|_| path.to_string_lossy().into_owned());
    let mut conn = open_at(db)?;
    let tx = conn.transaction().map_err(|error| error.to_string())?;
    let ids = {
        let mut stmt = tx
            .prepare("SELECT id FROM documents WHERE root=?1")
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map(params![path], |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
    };
    for id in ids {
        tx.execute("DELETE FROM content_fts WHERE rowid=?1", params![id])
            .map_err(|error| error.to_string())?;
    }
    tx.execute("DELETE FROM documents WHERE root=?1", params![path])
        .map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM roots WHERE path=?1", params![path])
        .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|error| error.to_string())
}

fn modified_ns(meta: &fs::Metadata) -> Option<String> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos().to_string())
}

fn is_efs_encrypted(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        meta.file_attributes() & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ENCRYPTED.0
            != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

fn supported(path: &Path) -> bool {
    let ext = super::extension(path);
    matches!(ext.as_str(), "pdf" | "docx" | "pptx")
        || super::is_text_ext(&ext)
        || matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "webp"
        )
}

fn extract_pdf_bounded(path: &Path, _source_len: u64) -> Option<String> {
    if let Some(pdfium) = super::load_pdfium()
        && let Ok(document) = pdfium.load_pdf_from_file(path, None)
    {
        let mut text = String::new();
        for page_index in 0..document.pages().len().min(100) {
            let page = document.pages().get(page_index).ok()?;
            let page_text = page.text().ok()?.all();
            let remaining = MAX_TEXT_BYTES.saturating_sub(text.len());
            if remaining == 0 {
                break;
            }
            let mut take = page_text.len().min(remaining);
            while !page_text.is_char_boundary(take) {
                take -= 1;
            }
            text.push_str(&page_text[..take]);
            text.push('\n');
        }
        return Some(text);
    }
    None
}

fn extract(path: &Path, meta: &fs::Metadata, ocr: bool) -> Option<String> {
    if is_efs_encrypted(meta)
        || meta.len() > MAX_SOURCE_BYTES
        || super::cloud_files::hydration_risk(path)
    {
        return None;
    }
    let ext = super::extension(path);
    let mut body = match ext.as_str() {
        "pdf" => {
            let text = extract_pdf_bounded(path, meta.len()).unwrap_or_default();
            if text.trim().is_empty() && ocr {
                ocr_pdf(path)?
            } else {
                text
            }
        }
        "docx" => super::extract_office_openxml_text(path, "word/")?,
        "pptx" => super::extract_office_openxml_text(path, "ppt/slides/")?,
        "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "webp" if ocr => ocr_image(path)?,
        _ if super::is_text_ext(&ext) && meta.len() <= MAX_TEXT_BYTES as u64 => {
            String::from_utf8_lossy(&fs::read(path).ok()?).into_owned()
        }
        _ => return None,
    };
    if body.len() > MAX_TEXT_BYTES {
        let mut boundary = MAX_TEXT_BYTES;
        while !body.is_char_boundary(boundary) {
            boundary -= 1;
        }
        body.truncate(boundary);
    }
    (!body.trim().is_empty()).then_some(body)
}

#[cfg(not(windows))]
fn ocr_image(_path: &Path) -> Option<String> {
    None
}

#[cfg(not(windows))]
fn ocr_pdf(_path: &Path) -> Option<String> {
    None
}

#[cfg(windows)]
fn recognize_bitmap(bitmap: &windows::Graphics::Imaging::SoftwareBitmap) -> Option<String> {
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    let bitmap = SoftwareBitmap::Convert(bitmap, BitmapPixelFormat::Bgra8).ok()?;
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().ok()?;
    let result =
        futures::executor::block_on(engine.RecognizeAsync(&bitmap).ok()?.into_future()).ok()?;
    result.Text().ok().map(|text| text.to_string())
}

#[cfg(windows)]
fn ocr_pdf(path: &Path) -> Option<String> {
    use pdfium_render::prelude::*;
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Storage::Streams::DataWriter;
    let pdfium = super::load_pdfium()?;
    let document = pdfium.load_pdf_from_file(path, None).ok()?;
    let mut text = String::new();
    // Separate low-priority OCR stage: at most three pages and about 2.3 MP
    // per page. The decoded pixels remain in memory, never in a temp file.
    for page_index in 0..document.pages().len().min(3) {
        let page = document.pages().get(page_index).ok()?;
        let config = PdfRenderConfig::new()
            .set_target_width(1200)
            .set_maximum_height(1920);
        let image = page
            .render_with_config(&config)
            .ok()?
            .as_image()
            .into_rgba8();
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 3_000_000 {
            continue;
        }
        let writer = DataWriter::new().ok()?;
        writer.WriteBytes(image.as_raw()).ok()?;
        let buffer = writer.DetachBuffer().ok()?;
        let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
            &buffer,
            BitmapPixelFormat::Rgba8,
            width as i32,
            height as i32,
        )
        .ok()?;
        if let Some(page_text) = recognize_bitmap(&bitmap)
            && !page_text.trim().is_empty()
        {
            text.push_str(&page_text);
            text.push('\n');
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(windows)]
fn ocr_image(path: &Path) -> Option<String> {
    use windows::Graphics::Imaging::BitmapDecoder;
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::StorageFile;
    use windows::core::HSTRING;
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_SOURCE_BYTES {
        return None;
    }
    // WinRT OCR uses installed local language packs. Work stays on the scanner
    // worker and never sends a document to a service.
    let file = futures::executor::block_on(
        StorageFile::GetFileFromPathAsync(&HSTRING::from(path.to_string_lossy().as_ref()))
            .ok()?
            .into_future(),
    )
    .ok()?;
    let stream = futures::executor::block_on(file.OpenReadAsync().ok()?.into_future()).ok()?;
    let decoder =
        futures::executor::block_on(BitmapDecoder::CreateAsync(&stream).ok()?.into_future())
            .ok()?;
    let max_dimension = OcrEngine::MaxImageDimension().ok()?;
    if decoder.PixelWidth().ok()? > max_dimension
        || decoder.PixelHeight().ok()? > max_dimension
        || u64::from(decoder.PixelWidth().ok()?) * u64::from(decoder.PixelHeight().ok()?)
            > 16_000_000
    {
        return None;
    }
    let bitmap =
        futures::executor::block_on(decoder.GetSoftwareBitmapAsync().ok()?.into_future()).ok()?;
    recognize_bitmap(&bitmap)
}

pub fn scan(db: &Path, root: &Path, cancel: &AtomicBool) -> Result<RootStatus, String> {
    scan_with_cancel(db, root, || cancel.load(Ordering::Relaxed))
}

pub fn scan_with_cancel(
    db: &Path,
    root: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<RootStatus, String> {
    let _guard = loop {
        if cancelled() {
            return Err("Content scan cancelled".into());
        }
        match SCAN_LOCK.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let key = root_key(root)?;
    let mut conn = open_at(db)?;
    let ocr: bool = conn
        .query_row("SELECT ocr FROM roots WHERE path=?1", params![key], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .map_err(|error| error.to_string())?
        .ok_or("Folder is not opted in for content indexing")?
        != 0;
    let generation = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos() as i64;
    conn.execute("UPDATE roots SET complete=0 WHERE path=?1", params![key])
        .map_err(|error| error.to_string())?;
    let mut indexed = 0u64;
    let mut skipped = 0u64;
    let mut seen = 0usize;
    let mut stored_bytes = conn
        .query_row(
            "SELECT COALESCE(SUM(length(CAST(body AS BLOB))),0) FROM documents",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())?
        .max(0) as u64;
    for entry in WalkDir::new(&key).follow_links(false).into_iter() {
        if cancelled() {
            return Err("Content scan cancelled".into());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() || !supported(entry.path()) {
            continue;
        }
        seen += 1;
        if seen > MAX_SCAN_FILES {
            return Err("Content scan reached its 100,000-file budget".into());
        }
        let path = entry.path().to_string_lossy().into_owned();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        // EFS contents must not be copied into the plaintext search database.
        // The stale-row pass also removes a document encrypted since last scan.
        if is_efs_encrypted(&meta) {
            skipped += 1;
            continue;
        }
        let stamp = match modified_ns(&meta) {
            Some(stamp) => stamp,
            None => {
                skipped += 1;
                continue;
            }
        };
        let existing: Option<(i64, i64, String, u64)> = conn.query_row(
            "SELECT id, size, modified_ns, length(CAST(body AS BLOB)) FROM documents WHERE path=?1", params![path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get::<_, i64>(3)?.max(0) as u64)),
        ).optional().map_err(|error| error.to_string())?;
        if let Some((_, size, old_stamp, _)) = &existing
            && *size == meta.len() as i64
            && *old_stamp == stamp
        {
            conn.execute(
                "UPDATE documents SET generation=?2 WHERE path=?1",
                params![path, generation],
            )
            .map_err(|error| error.to_string())?;
            indexed += 1;
            continue;
        }
        let Some(body) = extract(entry.path(), &meta, ocr) else {
            skipped += 1;
            continue;
        };
        let unchanged = fs::metadata(entry.path()).ok().is_some_and(|current| {
            current.len() == meta.len() && modified_ns(&current).as_deref() == Some(stamp.as_str())
        });
        if !unchanged {
            skipped += 1;
            continue;
        }
        let old_len = existing
            .as_ref()
            .map(|(_, _, _, old_len)| *old_len)
            .unwrap_or(0);
        if stored_bytes
            .saturating_sub(old_len)
            .saturating_add(body.len() as u64)
            > MAX_INDEX_TEXT_BYTES
        {
            skipped += 1;
            continue;
        }
        let tx = conn.transaction().map_err(|error| error.to_string())?;
        if let Some((id, _, _, _)) = existing {
            tx.execute("DELETE FROM content_fts WHERE rowid=?1", params![id])
                .map_err(|error| error.to_string())?;
            tx.execute("UPDATE documents SET root=?2, size=?3, modified_ns=?4, body=?5, generation=?6 WHERE id=?1",
                params![id, key, meta.len() as i64, stamp, body, generation]).map_err(|error| error.to_string())?;
            tx.execute(
                "INSERT INTO content_fts(rowid,body) VALUES(?1,?2)",
                params![id, body],
            )
            .map_err(|error| error.to_string())?;
        } else {
            tx.execute("INSERT INTO documents(path,root,size,modified_ns,body,generation) VALUES(?1,?2,?3,?4,?5,?6)",
                params![path, key, meta.len() as i64, stamp, body, generation]).map_err(|error| error.to_string())?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO content_fts(rowid,body) VALUES(?1,?2)",
                params![id, body],
            )
            .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        stored_bytes = stored_bytes
            .saturating_sub(old_len)
            .saturating_add(body.len() as u64);
        indexed += 1;
    }
    let stale = {
        let mut stmt = conn
            .prepare("SELECT id FROM documents WHERE root=?1 AND generation<>?2")
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map(params![key, generation], |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
    };
    let tx = conn.transaction().map_err(|error| error.to_string())?;
    for id in stale {
        tx.execute("DELETE FROM content_fts WHERE rowid=?1", params![id])
            .map_err(|error| error.to_string())?;
        tx.execute("DELETE FROM documents WHERE id=?1", params![id])
            .map_err(|error| error.to_string())?;
    }
    let scanned_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs() as i64;
    tx.execute(
        "UPDATE roots SET scanned_at=?2,indexed=?3,skipped=?4,complete=1 WHERE path=?1",
        params![key, scanned_at, indexed as i64, skipped as i64],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(RootStatus {
        path: key,
        scanned_at,
        indexed,
        skipped,
        complete: true,
        ocr,
    })
}

fn snippet(body: &str, needle: &str) -> Option<String> {
    let lower = body.to_lowercase();
    let start = lower.find(&needle.to_lowercase())?;
    // Lowercasing may change byte lengths for some Unicode characters. Find
    // the corresponding character position before slicing the original text.
    let char_index = lower[..start].chars().count();
    let original_start = body
        .char_indices()
        .nth(char_index)
        .map(|(at, _)| at)
        .unwrap_or(body.len());
    let begin = body[..original_start]
        .char_indices()
        .rev()
        .nth(35)
        .map(|(at, _)| at)
        .unwrap_or(0);
    let match_end = body[original_start..]
        .char_indices()
        .nth(needle.chars().count())
        .map(|(at, _)| original_start + at)
        .unwrap_or(body.len());
    let end = body[original_start..]
        .char_indices()
        .nth(needle.chars().count() + 55)
        .map(|(at, _)| original_start + at)
        .unwrap_or(body.len());
    let before = body[begin..original_start]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let found = &body[original_start..match_end];
    let after = body[match_end..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "{}{} [ {} ] {}{}",
        if begin > 0 { "…" } else { "" },
        before,
        found,
        after,
        if end < body.len() { "…" } else { "" }
    ))
}

#[cfg(test)]
pub fn search(db: &Path, scope: &Path, needle: &str, max: usize) -> Result<Vec<Hit>, String> {
    search_with_filter(db, scope, needle, max, |_, _| true)
}

pub fn search_with_filter(
    db: &Path,
    scope: &Path,
    needle: &str,
    max: usize,
    predicate: impl Fn(&Path, &fs::Metadata) -> bool,
) -> Result<Vec<Hit>, String> {
    if needle.is_empty() || max == 0 {
        return Ok(Vec::new());
    }
    let conn = open_at(db)?;
    let use_fts = needle.is_ascii() && needle.len() >= 3 && !needle.contains(['%', '_']);
    let sql = if use_fts {
        "SELECT d.path,d.body,d.size,d.modified_ns FROM content_fts f JOIN documents d ON d.id=f.rowid WHERE f.body LIKE ?1"
    } else {
        "SELECT path,body,size,modified_ns FROM documents"
    };
    let mut stmt = conn.prepare(sql).map_err(|error| error.to_string())?;
    let pattern = format!("%{needle}%");
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    };
    let mut rows = if use_fts {
        stmt.query(params![pattern])
    } else {
        stmt.query([])
    }
    .map_err(|error| error.to_string())?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        let (path, body, size, stamp) =
            map_row(row).map_err(|error: rusqlite::Error| error.to_string())?;
        if !path_within(Path::new(&path), scope) {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if is_efs_encrypted(&meta) {
            continue;
        }
        if meta.len() != size.max(0) as u64 || modified_ns(&meta).as_deref() != Some(stamp.as_str())
        {
            continue;
        }
        if !predicate(Path::new(&path), &meta) {
            continue;
        }
        if let Some(snippet) = snippet(&body, needle) {
            hits.push(Hit { path, snippet });
            if hits.len() >= max {
                break;
            }
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_comparison_handles_windows_extended_paths() {
        assert!(path_within(
            Path::new(r"\\?\C:\Docs\one.txt"),
            Path::new(r"C:\Docs")
        ));
        assert!(!path_within(
            Path::new(r"\\?\C:\Docs2\one.txt"),
            Path::new(r"C:\Docs")
        ));
        assert!(path_within(Path::new(r"\\?\C:\one.txt"), Path::new(r"C:\")));
        assert!(path_within(
            Path::new(r"\\?\UNC\server\share\one.txt"),
            Path::new(r"\\server\share")
        ));
    }

    #[test]
    fn opt_in_rescan_and_removal() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("pathfinder-content-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("index.sqlite3");
        let root = dir.join("documents");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("note.txt");
        fs::write(&file, "Alpha unique phrase Omega").unwrap();
        assert!(search(&db, &root, "unique", 10).unwrap().is_empty());
        add_root(&db, &root).unwrap();
        let first = scan(&db, &root, &AtomicBool::new(false)).unwrap();
        assert_eq!(first.indexed, 1);
        assert!(
            search(&db, &root, "unique", 10).unwrap()[0]
                .snippet
                .contains("unique")
        );
        let second = scan(&db, &root, &AtomicBool::new(false)).unwrap();
        assert_eq!(second.indexed, 1);
        fs::remove_file(file).unwrap();
        scan(&db, &root, &AtomicBool::new(false)).unwrap();
        assert!(search(&db, &root, "unique", 10).unwrap().is_empty());
        remove_root(&db, &root).unwrap();
        assert!(roots(&db).unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn content_filter_is_applied_before_result_limit() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("pathfinder-content-filter-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("index.sqlite3");
        let root = dir.join("documents");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "unique phrase").unwrap();
        fs::write(root.join("b.txt"), "unique phrase").unwrap();
        add_root(&db, &root).unwrap();
        scan(&db, &root, &AtomicBool::new(false)).unwrap();
        let hits = search_with_filter(&db, &root, "unique", 1, |path, _| {
            path.file_name().is_some_and(|name| name == "b.txt")
        })
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("b.txt"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn queued_scan_waits_for_active_scan() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("pathfinder-content-queued-{nonce}"));
        let root = dir.join("documents");
        let db = dir.join("index.sqlite3");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("note.txt"), "queue test").unwrap();
        add_root(&db, &root).unwrap();
        let active = SCAN_LOCK.lock().unwrap();
        let worker =
            std::thread::spawn(move || scan(&db, &root, &AtomicBool::new(false)).unwrap().indexed);
        std::thread::sleep(Duration::from_millis(50));
        drop(active);
        assert_eq!(worker.join().unwrap(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
