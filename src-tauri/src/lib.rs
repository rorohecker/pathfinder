#![allow(dead_code)]

use base64::{engine::general_purpose, Engine as _};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Manager, State, Window};
use walkdir::WalkDir;

slint::include_modules!();

const DIRECTORY_CACHE_TTL: Duration = Duration::from_secs(20);
const PREVIEW_CACHE_TTL: Duration = Duration::from_secs(180);
const MAX_DIRECTORY_CACHE_ENTRIES: usize = 64;
const MAX_PREVIEW_CACHE_ENTRIES: usize = 96;
const INDEX_DB_FILE: &str = ".pathfinder-index.sqlite3";
const THUMBNAIL_CACHE_LIMIT_BYTES: u64 = 50 * 1024 * 1024;
const INDEX_ESTIMATE_BYTES_PER_FILE: u64 = 420;
const MAX_OPERATION_QUEUE_ITEMS: usize = 200;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
    pub name_lower: String,
    pub kind: FileKind,
    pub size: u64,
    pub modified: u64,
    pub extension: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub modified: u64,
    pub created: u64,
    pub is_readonly: bool,
    pub extension: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DriveInfo {
    pub name: String,
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KnownFolder {
    pub id: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Bookmark {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PreviewContent {
    pub kind: String,
    pub mime: Option<String>,
    pub text: Option<String>,
    pub data_url: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AiCapabilities {
    pub npu_available: bool,
    pub semantic_search: bool,
    pub automatic_summaries: bool,
    pub image_classification: bool,
    pub local_embeddings: bool,
    pub reason: String,
}

#[derive(Clone)]
struct CachedDirectory {
    entries: Vec<FileEntry>,
    loaded_at: Instant,
}

#[derive(Clone)]
struct CachedPreview {
    content: PreviewContent,
    loaded_at: Instant,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileOp {
    pub kind: String,
    pub from: String,
    pub to: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RenameOp {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StorageNode {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub children: Vec<StorageNode>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SavedSearch {
    pub name: String,
    pub query: String,
    pub scope: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionTab {
    pub path: String,
    pub view: String,
    pub sort_by: String,
    pub sort_dir: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConflictInfo {
    pub incoming_path: String,
    pub existing_path: String,
    pub incoming_size: u64,
    pub existing_size: u64,
    pub incoming_modified: u64,
    pub existing_modified: u64,
    pub incoming_sha256: Option<String>,
    pub existing_sha256: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OperationQueueItem {
    pub id: u64,
    pub kind: String,
    pub source: String,
    pub destination: Option<String>,
    pub status: String,
    pub detail: String,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub conflict: Option<ConflictInfo>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LockedProcessInfo {
    pub pid: u32,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IndexStatus {
    pub mode: String,
    pub indexed_files: u64,
    pub index_bytes: u64,
    pub thumbnail_bytes: u64,
    pub thumbnail_limit: u64,
    pub estimated_storage: String,
    pub roots: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SmartFolder {
    pub id: String,
    pub name: String,
    pub query: String,
    pub scope: String,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FolderCompareEntry {
    pub path: String,
    pub left_exists: bool,
    pub right_exists: bool,
    pub left_size: u64,
    pub right_size: u64,
    pub left_modified: u64,
    pub right_modified: u64,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AutomationRule {
    pub name: String,
    pub folder: String,
    pub extension: String,
    pub tag: String,
    pub move_to: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileTemplate {
    pub name: String,
    pub extension: String,
    pub content: String,
}

type GitStatusMap = HashMap<String, String>;
type GitCacheMap = HashMap<String, (Arc<GitStatusMap>, Instant)>;

#[derive(Clone)]
struct AppState {
    directory_cache: Arc<Mutex<HashMap<String, CachedDirectory>>>,
    preview_cache: Arc<Mutex<HashMap<String, CachedPreview>>>,
    watchers: Arc<Mutex<HashMap<String, RecommendedWatcher>>>,
    search_generation: Arc<AtomicU64>,
    ai_capabilities: Arc<Mutex<Option<AiCapabilities>>>,
    operation_log: Arc<Mutex<Vec<FileOp>>>,
    operation_queue: Arc<Mutex<VecDeque<OperationQueueItem>>>,
    next_operation_id: Arc<AtomicU64>,
    queue_paused: Arc<Mutex<bool>>,
    git_cache: Arc<Mutex<GitCacheMap>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            directory_cache: Arc::new(Mutex::new(HashMap::new())),
            preview_cache: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            search_generation: Arc::new(AtomicU64::new(0)),
            ai_capabilities: Arc::new(Mutex::new(None)),
            operation_log: Arc::new(Mutex::new(Vec::new())),
            operation_queue: Arc::new(Mutex::new(VecDeque::new())),
            next_operation_id: Arc::new(AtomicU64::new(1)),
            queue_paused: Arc::new(Mutex::new(false)),
            git_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Default)]
struct ParsedQuery {
    terms: Vec<String>,
    ext: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    content: Option<String>,
    size: Option<SizeFilter>,
    modified_after: Option<SystemTime>,
}

#[derive(Clone, Copy)]
struct SizeFilter {
    op: SizeOp,
    value: u64,
}

#[derive(Clone, Copy)]
enum SizeOp {
    Greater,
    GreaterEq,
    Less,
    LessEq,
    Equal,
}

fn unix_secs(time: Result<SystemTime, std::io::Error>) -> u64 {
    time.ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_kind(path: &Path, metadata: &fs::Metadata) -> FileKind {
    if fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        FileKind::Symlink
    } else if metadata.is_dir() {
        FileKind::Directory
    } else if metadata.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

fn path_to_entry(entry_path: &Path, metadata: &fs::Metadata) -> FileEntry {
    let name = entry_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let extension = if metadata.is_file() {
        entry_path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
    } else {
        None
    };
    let name_lower = name.to_lowercase();

    FileEntry {
        path: entry_path.to_string_lossy().to_string(),
        name_lower,
        name,
        kind: file_kind(entry_path, metadata),
        size: metadata.len(),
        modified: unix_secs(metadata.modified()),
        extension,
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Err(format!("Destination already exists: {}", to.display()));
    }
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for entry in fs::read_dir(&src_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let src = entry.path();
            let dst = dst_dir.join(entry.file_name());
            if src.is_dir() && !src.is_symlink() {
                fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
                stack.push((src, dst));
            } else {
                fs::copy(&src, &dst).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

fn same_destination(left: &Path, right: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        left.to_string_lossy()
            .as_ref()
            .eq_ignore_ascii_case(right.to_string_lossy().as_ref())
    }

    #[cfg(not(target_os = "windows"))]
    {
        left == right
    }
}

fn quick_sha256(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut remaining = max_bytes;
    let mut buf = [0_u8; 64 * 1024];
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        let read = file.read(&mut buf[..take]).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        remaining -= read as u64;
    }
    Some(hex::encode(hasher.finalize()))
}

fn conflict_info(incoming: &Path, existing: &Path) -> ConflictInfo {
    let incoming_meta = fs::metadata(incoming).ok();
    let existing_meta = fs::metadata(existing).ok();
    let hash_limit = 8 * 1024 * 1024;
    ConflictInfo {
        incoming_path: incoming.to_string_lossy().to_string(),
        existing_path: existing.to_string_lossy().to_string(),
        incoming_size: incoming_meta.as_ref().map(|m| m.len()).unwrap_or(0),
        existing_size: existing_meta.as_ref().map(|m| m.len()).unwrap_or(0),
        incoming_modified: incoming_meta
            .as_ref()
            .map(|m| unix_secs(m.modified()))
            .unwrap_or(0),
        existing_modified: existing_meta
            .as_ref()
            .map(|m| unix_secs(m.modified()))
            .unwrap_or(0),
        incoming_sha256: incoming_meta
            .as_ref()
            .filter(|m| m.is_file() && m.len() <= hash_limit)
            .and_then(|_| quick_sha256(incoming, hash_limit)),
        existing_sha256: existing_meta
            .as_ref()
            .filter(|m| m.is_file() && m.len() <= hash_limit)
            .and_then(|_| quick_sha256(existing, hash_limit)),
    }
}

fn keep_both_destination(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "copy".to_string());
    let ext = path.extension().map(|s| s.to_string_lossy().to_string());
    for index in 1..10_000 {
        let name = if let Some(ext) = &ext {
            format!("{stem} ({index}).{ext}")
        } else {
            format!("{stem} ({index})")
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

fn folder_size_quick(path: &Path, max_entries: usize) -> u64 {
    if path.is_file() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .take(max_entries)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn cache_key(path: &Path) -> String {
    #[cfg(target_os = "windows")]
    return path.to_string_lossy().to_ascii_lowercase();
    #[cfg(not(target_os = "windows"))]
    return path.to_string_lossy().into_owned();
}

fn cache_key_str(path: &str) -> String {
    cache_key(Path::new(path))
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| match (&a.kind, &b.kind) {
        (FileKind::Directory, FileKind::Directory) => a.name_lower.cmp(&b.name_lower),
        (FileKind::Directory, _) => std::cmp::Ordering::Less,
        (_, FileKind::Directory) => std::cmp::Ordering::Greater,
        _ => a.name_lower.cmp(&b.name_lower),
    });
}

fn trim_dir_cache(cache: &mut HashMap<String, CachedDirectory>, max_entries: usize) {
    if cache.len() <= max_entries {
        return;
    }
    let excess = cache.len() - max_entries;
    let mut keys: Vec<(String, Instant)> = cache
        .iter()
        .map(|(k, v)| (k.clone(), v.loaded_at))
        .collect();
    keys.sort_unstable_by_key(|(_, t)| *t);
    for (k, _) in keys.iter().take(excess) {
        cache.remove(k);
    }
}

fn trim_preview_cache(cache: &mut HashMap<String, CachedPreview>, max_entries: usize) {
    if cache.len() <= max_entries {
        return;
    }
    let excess = cache.len() - max_entries;
    let mut keys: Vec<(String, Instant)> = cache
        .iter()
        .map(|(k, v)| (k.clone(), v.loaded_at))
        .collect();
    keys.sort_unstable_by_key(|(_, t)| *t);
    for (k, _) in keys.iter().take(excess) {
        cache.remove(k);
    }
}

impl AppState {
    fn cached_directory(&self, path: &str) -> Option<Vec<FileEntry>> {
        let key = cache_key_str(path);
        let mut cache = self.directory_cache.lock().ok()?;
        let cached = cache.get(&key)?;
        if cached.loaded_at.elapsed() <= DIRECTORY_CACHE_TTL {
            return Some(cached.entries.clone());
        }
        cache.remove(&key);
        None
    }

    fn store_directory(&self, path: &str, entries: Vec<FileEntry>) {
        if let Ok(mut cache) = self.directory_cache.lock() {
            cache.insert(
                cache_key_str(path),
                CachedDirectory {
                    entries,
                    loaded_at: Instant::now(),
                },
            );
            trim_dir_cache(&mut cache, MAX_DIRECTORY_CACHE_ENTRIES);
        }
    }

    fn log_op(&self, kind: &str, from: &str, to: Option<&str>) {
        if let Ok(mut log) = self.operation_log.lock() {
            log.push(FileOp {
                kind: kind.to_string(),
                from: from.to_string(),
                to: to.map(|s| s.to_string()),
            });
            if log.len() > 50 {
                log.remove(0);
            }
        }
    }

    fn queue_start(
        &self,
        kind: &str,
        source: &str,
        destination: Option<&str>,
        bytes_total: u64,
    ) -> u64 {
        let id = self.next_operation_id.fetch_add(1, Ordering::SeqCst);
        let item = OperationQueueItem {
            id,
            kind: kind.to_string(),
            source: source.to_string(),
            destination: destination.map(str::to_string),
            status: "running".to_string(),
            detail: "Queued".to_string(),
            bytes_total,
            bytes_done: 0,
            speed_bps: 0,
            eta_secs: None,
            conflict: None,
            started_at: now_unix_secs(),
            finished_at: None,
        };
        if let Ok(mut queue) = self.operation_queue.lock() {
            queue.push_back(item);
            while queue.len() > MAX_OPERATION_QUEUE_ITEMS {
                queue.pop_front();
            }
        }
        id
    }

    fn queue_finish(
        &self,
        id: u64,
        status: &str,
        detail: impl Into<String>,
        bytes_done: u64,
        elapsed: Duration,
    ) {
        if let Ok(mut queue) = self.operation_queue.lock() {
            if let Some(item) = queue.iter_mut().find(|item| item.id == id) {
                item.status = status.to_string();
                item.detail = detail.into();
                item.bytes_done = bytes_done;
                item.speed_bps = if elapsed.as_secs_f64() > 0.0 {
                    (bytes_done as f64 / elapsed.as_secs_f64()) as u64
                } else {
                    0
                };
                item.eta_secs = None;
                item.finished_at = Some(now_unix_secs());
            }
        }
    }

    fn queue_conflict(&self, id: u64, conflict: ConflictInfo) {
        if let Ok(mut queue) = self.operation_queue.lock() {
            if let Some(item) = queue.iter_mut().find(|item| item.id == id) {
                item.status = "conflict".to_string();
                item.detail = "Destination already exists".to_string();
                item.conflict = Some(conflict);
                item.finished_at = Some(now_unix_secs());
            }
        }
    }

    fn queue_is_paused(&self) -> bool {
        self.queue_paused
            .lock()
            .map(|paused| *paused)
            .unwrap_or(false)
    }

    fn invalidate_directory_path(&self, path: &Path) {
        if let Ok(mut cache) = self.directory_cache.lock() {
            cache.remove(&cache_key(path));
        }
    }

    fn invalidate_path(&self, path: &Path) {
        self.invalidate_directory_path(path);
        if let Some(parent) = path.parent() {
            self.invalidate_directory_path(parent);
        }
        let preview_prefix = cache_key(path);
        if let Ok(mut cache) = self.preview_cache.lock() {
            cache.retain(|key, _| !key.starts_with(&preview_prefix));
        }
    }

    fn preview(&self, key: &str) -> Option<PreviewContent> {
        let mut cache = self.preview_cache.lock().ok()?;
        let cached = cache.get(key)?;
        if cached.loaded_at.elapsed() <= PREVIEW_CACHE_TTL {
            return Some(cached.content.clone());
        }
        cache.remove(key);
        None
    }

    fn store_preview(&self, key: String, content: PreviewContent) {
        if let Ok(mut cache) = self.preview_cache.lock() {
            cache.insert(
                key,
                CachedPreview {
                    content,
                    loaded_at: Instant::now(),
                },
            );
            trim_preview_cache(&mut cache, MAX_PREVIEW_CACHE_ENTRIES);
        }
    }
}

fn bookmarks_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("bookmarks.json")
}

fn push_known(list: &mut Vec<KnownFolder>, id: &str, name: &str, path: Option<PathBuf>) {
    if let Some(path) = path {
        if path.exists() {
            list.push(KnownFolder {
                id: id.to_string(),
                name: name.to_string(),
                path: path.to_string_lossy().to_string(),
            });
        }
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "xml"
            | "csv"
            | "log"
            | "rs"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "css"
            | "html"
            | "htm"
            | "py"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "cs"
            | "ps1"
            | "bat"
            | "cmd"
            | "ini"
            | "sql"
    )
}

fn mime_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}

fn file_type_for_query(path: &Path, metadata: &fs::Metadata) -> &'static str {
    if metadata.is_dir() {
        return "folder";
    }

    match extension(path).as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "ico" | "heic" => "image",
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "wmv" => "video",
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" => "audio",
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" => "archive",
        "pdf" => "pdf",
        ext if is_text_ext(ext) => "text",
        _ => "file",
    }
}

fn is_archive_ext(ext: &str) -> bool {
    matches!(ext, "zip" | "7z" | "rar" | "tar" | "gz" | "xz")
}

fn is_font_ext(ext: &str) -> bool {
    matches!(ext, "ttf" | "otf" | "woff" | "woff2")
}

fn is_media_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp4"
            | "mov"
            | "mkv"
            | "avi"
            | "webm"
            | "wmv"
            | "mp3"
            | "wav"
            | "flac"
            | "aac"
            | "ogg"
            | "m4a"
    )
}

fn tokenize_query(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;

    for ch in query.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn parse_size_filter(raw: &str) -> Option<SizeFilter> {
    let raw = raw.trim().to_lowercase();
    let (op, rest) = if let Some(value) = raw.strip_prefix(">=") {
        (SizeOp::GreaterEq, value)
    } else if let Some(value) = raw.strip_prefix("<=") {
        (SizeOp::LessEq, value)
    } else if let Some(value) = raw.strip_prefix('>') {
        (SizeOp::Greater, value)
    } else if let Some(value) = raw.strip_prefix('<') {
        (SizeOp::Less, value)
    } else if let Some(value) = raw.strip_prefix('=') {
        (SizeOp::Equal, value)
    } else {
        (SizeOp::Equal, raw.as_str())
    };

    let split_at = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let (number, unit) = rest.split_at(split_at);
    let number = number.parse::<f64>().ok()?;
    let multiplier = match unit {
        "" | "b" => 1.0,
        "kb" | "k" => 1024.0,
        "mb" | "m" => 1024.0 * 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };

    Some(SizeFilter {
        op,
        value: (number * multiplier) as u64,
    })
}

fn matches_size(size: u64, filter: SizeFilter) -> bool {
    match filter.op {
        SizeOp::Greater => size > filter.value,
        SizeOp::GreaterEq => size >= filter.value,
        SizeOp::Less => size < filter.value,
        SizeOp::LessEq => size <= filter.value,
        SizeOp::Equal => size == filter.value,
    }
}

fn parse_modified_filter(raw: &str) -> Option<SystemTime> {
    let raw = raw.trim().to_lowercase();
    match raw.as_str() {
        "today" => return Some(SystemTime::now() - Duration::from_secs(24 * 60 * 60)),
        "week" => return Some(SystemTime::now() - Duration::from_secs(7 * 24 * 60 * 60)),
        "month" => return Some(SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60)),
        _ => {}
    }
    let split_at = raw.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = raw.split_at(split_at);
    let number = number.parse::<u64>().ok()?;
    let days = match unit {
        "h" => return Some(SystemTime::now() - Duration::from_secs(number * 60 * 60)),
        "d" => number,
        "w" => number * 7,
        "m" => number * 30,
        "y" => number * 365,
        _ => return None,
    };
    Some(SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60))
}

fn parse_query(query: &str) -> ParsedQuery {
    let mut parsed = ParsedQuery::default();

    for token in tokenize_query(query) {
        let token = token.trim_matches('"').to_string();
        if let Some((key, value)) = token.split_once(':') {
            let key = key.to_lowercase();
            let value = value.trim_matches('"').to_lowercase();
            match key.as_str() {
                "ext" => parsed.ext = Some(value.trim_start_matches('.').to_string()),
                "kind" => parsed.kind = Some(value),
                "name" => parsed.name = Some(value),
                "content" => parsed.content = Some(value),
                "size" => parsed.size = parse_size_filter(&value),
                "modified" => parsed.modified_after = parse_modified_filter(&value),
                // Tags are stored in the frontend because they are local app metadata.
                "tag" => {}
                _ => parsed.terms.push(token.to_lowercase()),
            }
        } else {
            parsed.terms.push(token.to_lowercase());
        }
    }

    parsed
}

fn read_text_for_search(path: &Path, metadata: &fs::Metadata) -> Option<Vec<u8>> {
    if metadata.len() > 1024 * 1024 {
        return None;
    }
    let ext = extension(path);
    if !is_text_ext(&ext) {
        return None;
    }
    // Read raw bytes; avoids UTF-8 validation scan.
    // make_ascii_lowercase is in-place and non-ASCII bytes pass through unchanged,
    // which is fine since the search patterns are ASCII.
    let mut bytes = fs::read(path).ok()?;
    bytes.make_ascii_lowercase();
    Some(bytes)
}

fn matches_query(path: &Path, metadata: &fs::Metadata, parsed: &ParsedQuery) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let ext = extension(path);
    let kind = file_type_for_query(path, metadata);

    if let Some(expected) = &parsed.ext {
        if ext != *expected {
            return false;
        }
    }

    if let Some(expected) = &parsed.kind {
        if kind != expected {
            return false;
        }
    }

    if let Some(expected) = &parsed.name {
        if !name.contains(expected) {
            return false;
        }
    }

    if let Some(filter) = parsed.size {
        if metadata.is_dir() || !matches_size(metadata.len(), filter) {
            return false;
        }
    }

    if let Some(after) = parsed.modified_after {
        if metadata.modified().map(|m| m < after).unwrap_or(true) {
            return false;
        }
    }

    let needs_content = parsed.content.is_some()
        || (!parsed.terms.is_empty() && metadata.is_file() && is_text_ext(&ext));
    let content = if needs_content {
        read_text_for_search(path, metadata)
    } else {
        None
    };

    if let Some(expected) = &parsed.content {
        let needle = expected.as_bytes();
        if !content
            .as_ref()
            .map(|bytes| memchr::memmem::find(bytes, needle).is_some())
            .unwrap_or(false)
        {
            return false;
        }
    }

    for term in &parsed.terms {
        let needle = term.as_bytes();
        let in_name = name.contains(term.as_str());
        let in_content = content
            .as_ref()
            .map(|bytes| memchr::memmem::find(bytes, needle).is_some())
            .unwrap_or(false);
        if !in_name && !in_content {
            return false;
        }
    }

    true
}

fn list_directory_uncached(dir: &Path) -> Result<Vec<FileEntry>, String> {
    if !dir.exists() {
        return Err(format!("Path does not exist: {}", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }

    let paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();

    let mut entries: Vec<FileEntry> = paths
        .par_iter()
        .filter_map(|path| fs::metadata(path).ok().map(|m| path_to_entry(path, &m)))
        .collect();

    sort_entries(&mut entries);
    Ok(entries)
}

#[tauri::command]
fn list_directory(state: State<'_, AppState>, path: String) -> Result<Vec<FileEntry>, String> {
    if let Some(entries) = state.cached_directory(&path) {
        return Ok(entries);
    }

    let dir = PathBuf::from(&path);
    let entries = list_directory_uncached(&dir)?;
    state.store_directory(&path, entries.clone());
    schedule_index_directory(path, entries.clone());
    Ok(entries)
}

#[tauri::command]
fn get_file_info(path: String) -> Result<FileInfo, String> {
    let path_buf = PathBuf::from(&path);
    if !path_buf.exists() {
        return Err(format!("Path does not exist: {path}"));
    }

    let metadata = fs::metadata(&path_buf).map_err(|e| e.to_string())?;
    let name = path_buf
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let extension = if metadata.is_file() {
        path_buf
            .extension()
            .map(|e| e.to_string_lossy().to_string())
    } else {
        None
    };

    Ok(FileInfo {
        path: path_buf.to_string_lossy().to_string(),
        name,
        kind: file_kind(&path_buf, &metadata),
        size: metadata.len(),
        modified: unix_secs(metadata.modified()),
        created: unix_secs(metadata.created()),
        is_readonly: metadata.permissions().readonly(),
        extension,
    })
}

#[tauri::command]
fn get_home_directory() -> Result<String, String> {
    dirs::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| "Could not determine home directory".to_string())
}

#[tauri::command]
fn get_known_folders() -> Vec<KnownFolder> {
    let mut folders = Vec::new();
    push_known(&mut folders, "home", "Home", dirs::home_dir());
    push_known(&mut folders, "desktop", "Desktop", dirs::desktop_dir());
    push_known(&mut folders, "documents", "Documents", dirs::document_dir());
    push_known(&mut folders, "downloads", "Downloads", dirs::download_dir());
    push_known(&mut folders, "pictures", "Pictures", dirs::picture_dir());
    push_known(&mut folders, "music", "Music", dirs::audio_dir());
    push_known(&mut folders, "videos", "Videos", dirs::video_dir());
    folders
}

#[tauri::command]
fn get_parent_path(path: String) -> Option<String> {
    PathBuf::from(&path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|p| !p.is_empty())
}

#[tauri::command]
fn join_path(parent: String, child: String) -> Result<String, String> {
    if child.contains('/') || child.contains('\\') {
        return Err("Name cannot contain path separators".to_string());
    }
    Ok(PathBuf::from(parent)
        .join(child)
        .to_string_lossy()
        .to_string())
}

#[tauri::command]
fn path_exists(path: String) -> bool {
    Path::new(&path).exists()
}

#[tauri::command]
fn get_drives() -> Vec<DriveInfo> {
    let mut drives = Vec::new();

    #[cfg(target_os = "windows")]
    {
        for byte in b'A'..=b'Z' {
            let path = format!("{}:\\", byte as char);
            if Path::new(&path).exists() {
                drives.push(DriveInfo {
                    name: format!("{}:", byte as char),
                    path,
                    kind: "local".to_string(),
                });
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        drives.push(DriveInfo {
            name: "Root".to_string(),
            path: "/".to_string(),
            kind: "local".to_string(),
        });

        if let Some(home) = dirs::home_dir() {
            drives.push(DriveInfo {
                name: "Home".to_string(),
                path: home.to_string_lossy().to_string(),
                kind: "local".to_string(),
            });
        }
    }

    drives
}

#[tauri::command]
fn open_file(path: String) -> Result<(), String> {
    open::that(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_in_folder(path: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);

    #[cfg(target_os = "windows")]
    {
        let target = if path_buf.is_dir() {
            path_buf.to_string_lossy().to_string()
        } else {
            format!("/select,{}", path_buf.to_string_lossy())
        };
        ProcessCommand::new("explorer")
            .arg(target)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let target = if path_buf.is_dir() {
            path_buf
        } else {
            path_buf
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        open::that(target).map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "windows")]
fn locked_file_processes(path: &str) -> Result<Vec<LockedProcessInfo>, String> {
    use std::mem::MaybeUninit;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY,
        RM_PROCESS_INFO,
    };

    let mut session = 0_u32;
    let mut key = vec![0_u16; CCH_RM_SESSION_KEY as usize + 1];
    let start = unsafe { RmStartSession(&mut session, 0, PWSTR(key.as_mut_ptr())) };
    if start.0 != 0 {
        return Err(format!("Restart Manager failed to start: {}", start.0));
    }

    let wide: Vec<u16> = Path::new(path)
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect();
    let files = [PCWSTR(wide.as_ptr())];
    let register = unsafe { RmRegisterResources(session, Some(&files), None, None) };
    if register.0 != 0 {
        unsafe {
            let _ = RmEndSession(session);
        }
        return Err(format!("Could not inspect file locks: {}", register.0));
    }

    let mut needed = 0_u32;
    let mut count = 0_u32;
    let mut reboot_reasons = 0_u32;
    let first = unsafe { RmGetList(session, &mut needed, &mut count, None, &mut reboot_reasons) };

    let mut result = Vec::new();
    if first.0 == 234 && needed > 0 {
        let mut processes: Vec<RM_PROCESS_INFO> = (0..needed)
            .map(|_| unsafe { MaybeUninit::<RM_PROCESS_INFO>::zeroed().assume_init() })
            .collect();
        count = needed;
        let second = unsafe {
            RmGetList(
                session,
                &mut needed,
                &mut count,
                Some(processes.as_mut_ptr()),
                &mut reboot_reasons,
            )
        };
        if second.0 == 0 {
            for info in processes.into_iter().take(count as usize) {
                let name = String::from_utf16_lossy(
                    &info
                        .strAppName
                        .iter()
                        .copied()
                        .take_while(|c| *c != 0)
                        .collect::<Vec<_>>(),
                );
                result.push(LockedProcessInfo {
                    pid: info.Process.dwProcessId,
                    name: if name.trim().is_empty() {
                        "Unknown process".to_string()
                    } else {
                        name
                    },
                    reason: format!("{:?}", info.ApplicationType),
                });
            }
        }
    }

    unsafe {
        let _ = RmEndSession(session);
    }
    Ok(result)
}

#[cfg(not(target_os = "windows"))]
fn locked_file_processes(_path: &str) -> Result<Vec<LockedProcessInfo>, String> {
    Ok(Vec::new())
}

fn open_windows_properties(path: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let escaped = path.replace('\'', "''");
        ProcessCommand::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "$p='{}'; $shell=New-Object -ComObject Shell.Application; \
                     $item=Get-Item -LiteralPath $p -ErrorAction Stop; \
                     $folder=$shell.Namespace($item.DirectoryName); \
                     if ($item.PSIsContainer) {{ $folder=$shell.Namespace($item.Parent.FullName) }}; \
                     $folder.ParseName($item.Name).InvokeVerb('properties')",
                    escaped
                ),
            ])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err("Native Properties is only available on Windows.".to_string())
    }
}

fn open_more_options(path: &str) -> Result<(), String> {
    reveal_in_folder(path.to_string())
}

fn cloud_state_label(path: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        let attrs = fs::metadata(path).map(|m| m.file_attributes()).unwrap_or(0);
        let mut states = Vec::new();
        if attrs & 0x0000_1000 != 0 {
            states.push("offline/cloud-only");
        }
        if attrs & 0x0008_0000 != 0 {
            states.push("pinned/always available");
        }
        if attrs & 0x0010_0000 != 0 {
            states.push("unpinned");
        }
        if attrs & 0x0040_0000 != 0 {
            states.push("recall on data access");
        }
        if states.is_empty() {
            "No cloud placeholder attributes reported.".to_string()
        } else {
            states.join(", ")
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        "Cloud file attributes are Windows-specific.".to_string()
    }
}

fn copy_text_to_clipboard(text: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        ProcessCommand::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Set-Clipboard"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(text.as_bytes())?;
                }
                let _ = child.wait();
                Ok(())
            })
            .map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = text;
        Err("Clipboard copy is only implemented on Windows.".to_string())
    }
}

#[tauri::command]
fn rename_file(
    state: State<'_, AppState>,
    path: String,
    new_name: String,
) -> Result<String, String> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if new_name.contains('/') || new_name.contains('\\') {
        return Err("Name cannot contain path separators".to_string());
    }

    let src = PathBuf::from(&path);
    let parent = src.parent().ok_or("No parent directory")?;
    let dst = parent.join(new_name);
    if dst.exists() && !same_destination(&src, &dst) {
        return Err(format!("'{new_name}' already exists"));
    }

    fs::rename(&src, &dst).map_err(|e| e.to_string())?;
    state.invalidate_path(&src);
    state.invalidate_path(&dst);
    state.log_op("rename", &path, Some(&dst.to_string_lossy()));
    Ok(dst.to_string_lossy().to_string())
}

#[tauri::command]
fn delete_file(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);
    if !path_buf.exists() {
        return Err(format!("Path does not exist: {path}"));
    }
    trash::delete(&path_buf).map_err(|e| e.to_string())?;
    state.invalidate_path(&path_buf);
    Ok(())
}

#[tauri::command]
fn create_directory(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);
    if path_buf.exists() {
        return Err(format!("Folder already exists: {}", path_buf.display()));
    }
    fs::create_dir_all(&path_buf).map_err(|e| e.to_string())?;
    state.invalidate_path(&path_buf);
    Ok(())
}

#[tauri::command]
fn copy_file(state: State<'_, AppState>, from: String, to: String) -> Result<(), String> {
    let src = PathBuf::from(&from);
    let dst = PathBuf::from(&to);
    if dst.exists() {
        return Err(format!("Destination already exists: {}", dst.display()));
    }

    let result = if src.is_dir() {
        copy_dir_recursive(&src, &dst)
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::copy(&src, &dst).map(|_| ()).map_err(|e| e.to_string())
    };
    if result.is_ok() {
        state.invalidate_path(&dst);
        state.log_op("copy", &from, Some(&to));
    }
    result
}

#[tauri::command]
fn move_file(state: State<'_, AppState>, from: String, to: String) -> Result<(), String> {
    let src = PathBuf::from(&from);
    let dst = PathBuf::from(&to);
    if dst.exists() {
        return Err(format!("Destination already exists: {}", dst.display()));
    }

    if fs::rename(&src, &dst).is_ok() {
        state.invalidate_path(&src);
        state.invalidate_path(&dst);
        return Ok(());
    }

    let result = if src.is_dir() {
        copy_dir_recursive(&src, &dst)?;
        fs::remove_dir_all(&src).map_err(|e| e.to_string())
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::copy(&src, &dst).map_err(|e| e.to_string())?;
        fs::remove_file(&src).map_err(|e| e.to_string())
    };
    if result.is_ok() {
        state.invalidate_path(&src);
        state.invalidate_path(&dst);
        state.log_op("move", &from, Some(&to));
    }
    result
}

#[tauri::command]
fn search_files(
    state: State<'_, AppState>,
    query: String,
    path: String,
    max_results: Option<usize>,
    use_indexed: Option<bool>,
) -> Result<Vec<FileEntry>, String> {
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {path}"));
    }

    let max = max_results.unwrap_or(400).min(2000);
    if use_indexed.unwrap_or(false) {
        if let Ok(mut indexed) = windows_index_search_impl(&query, &path, max) {
            if !indexed.is_empty() {
                sort_entries(&mut indexed);
                return Ok(indexed);
            }
        }
    }

    let token = state.search_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let parsed = Arc::new(parse_query(&query));
    let generation = state.search_generation.clone();

    // Split into top-level work units so each Rayon thread gets its own
    // WalkDir, issuing independent I/O requests to the NVMe queue in parallel.
    let work_units: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();

    let mut output: Vec<FileEntry> = work_units
        .into_par_iter()
        .flat_map_iter(|subtree| {
            let parsed = Arc::clone(&parsed);
            let generation = Arc::clone(&generation);
            WalkDir::new(subtree)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter_map(move |entry| {
                    if generation.load(Ordering::Relaxed) != token {
                        return None;
                    }
                    let entry_path = entry.path().to_path_buf();
                    let metadata = entry.metadata().ok()?;
                    if matches_query(&entry_path, &metadata, &parsed) {
                        Some(path_to_entry(&entry_path, &metadata))
                    } else {
                        None
                    }
                })
        })
        .take_any(max)
        .collect();

    if state.search_generation.load(Ordering::SeqCst) != token {
        return Ok(Vec::new());
    }

    sort_entries(&mut output);
    Ok(output)
}

#[cfg(target_os = "windows")]
fn windows_index_search_impl(
    query: &str,
    path: &str,
    max_results: usize,
) -> Result<Vec<FileEntry>, String> {
    let cleaned = tokenize_query(query)
        .into_iter()
        .filter(|token| !token.contains(':'))
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.trim().is_empty() {
        return Ok(Vec::new());
    }

    let script = r#"
$ErrorActionPreference = 'Stop'
$Query = $args[0]
$Scope = $args[1]
$Max = [int]$args[2]
$connection = New-Object -ComObject ADODB.Connection
$recordset = New-Object -ComObject ADODB.Recordset
$connection.Open("Provider=Search.CollatorDSO;Extended Properties='Application=Windows';")
$scopeItem = Get-Item -LiteralPath $Scope
$scopeUri = $scopeItem.FullName.Replace('\', '/')
if ($scopeUri -notmatch '/$') { $scopeUri += '/' }
$scopeUri = 'file:///' + $scopeUri
$like = $Query.Replace("'", "''").Replace("[", "[[]").Replace("%", "[%]").Replace("_", "[_]")
$sql = "SELECT TOP $Max System.ItemPathDisplay FROM SYSTEMINDEX WHERE SCOPE='$scopeUri' AND System.ItemNameDisplay LIKE '%$like%'"
$recordset.Open($sql, $connection)
$paths = New-Object System.Collections.Generic.List[string]
while (-not $recordset.EOF) {
  $value = $recordset.Fields.Item('System.ItemPathDisplay').Value
  if ($value) { $paths.Add([string]$value) }
  $recordset.MoveNext()
}
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
ConvertTo-Json -InputObject @($paths) -Compress
"#;

    let output = ProcessCommand::new("powershell")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(script)
        .arg(&cleaned)
        .arg(path)
        .arg(max_results.to_string())
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let paths: Vec<String> = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let entries = paths
        .into_iter()
        .filter_map(|path| {
            let path_buf = PathBuf::from(path);
            fs::metadata(&path_buf)
                .ok()
                .map(|metadata| path_to_entry(&path_buf, &metadata))
        })
        .collect();
    Ok(entries)
}

#[cfg(not(target_os = "windows"))]
fn windows_index_search_impl(
    _query: &str,
    _path: &str,
    _max_results: usize,
) -> Result<Vec<FileEntry>, String> {
    Err("Windows Search index is only available on Windows".to_string())
}

#[tauri::command]
fn windows_index_search(
    query: String,
    path: String,
    max_results: Option<usize>,
) -> Result<Vec<FileEntry>, String> {
    let mut entries =
        windows_index_search_impl(&query, &path, max_results.unwrap_or(400).min(2000))?;
    sort_entries(&mut entries);
    Ok(entries)
}

#[tauri::command]
fn read_preview(
    state: State<'_, AppState>,
    path: String,
    max_bytes: Option<usize>,
) -> Result<PreviewContent, String> {
    let path_buf = PathBuf::from(&path);
    let metadata = fs::metadata(&path_buf).map_err(|e| e.to_string())?;
    let key = format!(
        "{}|{}|{}",
        cache_key(&path_buf),
        unix_secs(metadata.modified()),
        max_bytes.unwrap_or(512 * 1024)
    );
    if let Some(content) = state.preview(&key) {
        return Ok(content);
    }

    let content = read_preview_uncached(&path_buf, &metadata, max_bytes)?;
    state.store_preview(key, content.clone());
    Ok(content)
}

fn archive_listing_preview(path: &Path, max_items: usize) -> Result<String, String> {
    let ext = extension(path);
    if ext != "zip" {
        return Ok(format!(
            "{} archive preview is available through Extract Here or Open in Explorer. Inline listing currently supports ZIP files.",
            ext.to_uppercase()
        ));
    }

    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    for i in 0..archive.len().min(max_items) {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        lines.push(format!(
            "{}  {}",
            format_size_short(entry.size()),
            entry.name()
        ));
    }
    if archive.len() > max_items {
        lines.push(format!("... {} more entries", archive.len() - max_items));
    }
    Ok(lines.join("\n"))
}

fn generic_metadata_preview(path: &Path, metadata: &fs::Metadata, kind: &str) -> String {
    let ext = extension(path).to_uppercase();
    format!(
        "Kind: {}\nExtension: {}\nSize: {}\nModified: {}\nPath: {}",
        kind,
        if ext.is_empty() {
            "none".to_string()
        } else {
            ext
        },
        format_size_short(metadata.len()),
        format_modified(unix_secs(metadata.modified())),
        path.display()
    )
}

fn read_preview_uncached(
    path_buf: &Path,
    metadata: &fs::Metadata,
    max_bytes: Option<usize>,
) -> Result<PreviewContent, String> {
    if metadata.is_dir() {
        return Ok(PreviewContent {
            kind: "folder".to_string(),
            mime: None,
            text: None,
            data_url: None,
            truncated: false,
        });
    }

    let ext = extension(path_buf);
    if ext == "svg" {
        let limit = max_bytes.unwrap_or(512 * 1024).min(2 * 1024 * 1024);
        let mut file = File::open(path_buf).map_err(|e| e.to_string())?;
        let mut bytes = Vec::with_capacity(limit + 1);
        std::io::Read::by_ref(&mut file)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        return Ok(PreviewContent {
            kind: "svg".to_string(),
            mime: Some("image/svg+xml".to_string()),
            text: Some(String::from_utf8_lossy(&bytes).to_string()),
            data_url: None,
            truncated,
        });
    }

    if is_archive_ext(&ext) {
        return Ok(PreviewContent {
            kind: "archive".to_string(),
            mime: None,
            text: Some(archive_listing_preview(path_buf, 120)?),
            data_url: None,
            truncated: false,
        });
    }

    if ext == "pdf" {
        return Ok(PreviewContent {
            kind: "pdf".to_string(),
            mime: Some("application/pdf".to_string()),
            text: Some(generic_metadata_preview(path_buf, metadata, "PDF document")),
            data_url: None,
            truncated: false,
        });
    }

    if is_font_ext(&ext) {
        return Ok(PreviewContent {
            kind: "font".to_string(),
            mime: None,
            text: Some(generic_metadata_preview(path_buf, metadata, "Font file")),
            data_url: None,
            truncated: false,
        });
    }

    if is_media_ext(&ext) {
        return Ok(PreviewContent {
            kind: "media".to_string(),
            mime: None,
            text: Some(generic_metadata_preview(path_buf, metadata, "Media file")),
            data_url: None,
            truncated: false,
        });
    }

    if let Some(mime) = mime_for_ext(&ext) {
        if metadata.len() > 20 * 1024 * 1024 {
            return Ok(PreviewContent {
                kind: "image-too-large".to_string(),
                mime: Some(mime.to_string()),
                text: None,
                data_url: None,
                truncated: true,
            });
        }

        let bytes = fs::read(path_buf).map_err(|e| e.to_string())?;
        return Ok(PreviewContent {
            kind: "image".to_string(),
            mime: Some(mime.to_string()),
            text: None,
            data_url: Some(format!(
                "data:{};base64,{}",
                mime,
                general_purpose::STANDARD.encode(bytes)
            )),
            truncated: false,
        });
    }

    if is_text_ext(&ext) {
        let limit = max_bytes.unwrap_or(512 * 1024).min(2 * 1024 * 1024);
        let mut file = File::open(path_buf).map_err(|e| e.to_string())?;
        let mut bytes = Vec::with_capacity(limit + 1);
        std::io::Read::by_ref(&mut file)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);

        return Ok(PreviewContent {
            kind: "text".to_string(),
            mime: Some("text/plain".to_string()),
            text: Some(String::from_utf8_lossy(&bytes).to_string()),
            data_url: None,
            truncated,
        });
    }

    Ok(PreviewContent {
        kind: file_type_for_query(path_buf, metadata).to_string(),
        mime: None,
        text: None,
        data_url: None,
        truncated: false,
    })
}

#[tauri::command]
fn warm_preview_cache(state: State<'_, AppState>, paths: Vec<String>, max_bytes: Option<usize>) {
    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        paths.into_par_iter().for_each(|path| {
            let path_buf = PathBuf::from(&path);
            let Ok(metadata) = fs::metadata(&path_buf) else {
                return;
            };
            let key = format!(
                "{}|{}|{}",
                cache_key(&path_buf),
                unix_secs(metadata.modified()),
                max_bytes.unwrap_or(256 * 1024)
            );
            if app_state.preview(&key).is_none() {
                if let Ok(content) = read_preview_uncached(&path_buf, &metadata, max_bytes) {
                    app_state.store_preview(key, content);
                }
            }
        });
    });
}

#[tauri::command]
fn prefetch_paths(state: State<'_, AppState>, paths: Vec<String>) {
    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        paths.into_par_iter().for_each(|path| {
            let dir = PathBuf::from(&path);
            if dir.is_dir() && app_state.cached_directory(&path).is_none() {
                if let Ok(entries) = list_directory_uncached(&dir) {
                    app_state.store_directory(&path, entries);
                }
            }
        });
    });
}

#[tauri::command]
fn watch_paths(state: State<'_, AppState>, paths: Vec<String>) -> Result<(), String> {
    let app_state = state.inner().clone();
    let mut watchers = state
        .watchers
        .lock()
        .map_err(|_| "Could not lock watcher registry")?;

    for path in paths {
        let path_buf = PathBuf::from(&path);
        if !path_buf.is_dir() || watchers.contains_key(&cache_key(&path_buf)) {
            continue;
        }

        let callback_state = app_state.clone();
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result {
                    let mut touched = HashSet::new();
                    for path in event.paths {
                        callback_state.invalidate_path(&path);
                        if let Some(parent) = path.parent() {
                            touched.insert(parent.to_path_buf());
                        }
                    }

                    for parent in touched {
                        let parent_string = parent.to_string_lossy().to_string();
                        if let Ok(entries) = list_directory_uncached(&parent) {
                            callback_state.store_directory(&parent_string, entries.clone());
                            schedule_index_directory(parent_string, entries);
                        }
                    }
                }
            },
            Config::default(),
        )
        .map_err(|e| e.to_string())?;

        watcher
            .watch(&path_buf, RecursiveMode::NonRecursive)
            .map_err(|e| e.to_string())?;
        watchers.insert(cache_key(&path_buf), watcher);

        // LRU pruning: keep at most 8 active watchers to avoid handle leaks
        const MAX_WATCHERS: usize = 8;
        if watchers.len() > MAX_WATCHERS {
            let evict_count = watchers.len() - MAX_WATCHERS;
            let keys_to_evict: Vec<String> = watchers.keys().take(evict_count).cloned().collect();
            for key in keys_to_evict {
                watchers.remove(&key);
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn detect_npu_names() -> Vec<String> {
    let script = r#"
$pattern = '(?i)(\bNPU\b|Neural Processing|Neural Processor|AI Boost|Ryzen AI|Hexagon|Hailo|Movidius|\bVPU\b)'
$devices = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
  Where-Object {
    ($_.Class -in @('ComputeAccelerator', 'System')) -and
    ($_.FriendlyName -match $pattern)
  } |
  Select-Object -ExpandProperty FriendlyName
ConvertTo-Json -InputObject @($devices) -Compress
"#;

    let output = ProcessCommand::new("powershell")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(script)
        .output();

    output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Vec<String>>(&output.stdout).ok())
        .unwrap_or_default()
}

#[cfg(not(target_os = "windows"))]
fn detect_npu_names() -> Vec<String> {
    Vec::new()
}

fn compute_ai_capabilities() -> AiCapabilities {
    let devices = detect_npu_names();
    let npu_hardware_found = !devices.is_empty();
    let runtime_configured = std::env::var("PATHFINDER_LOCAL_AI_RUNTIME")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let enabled = npu_hardware_found && runtime_configured;
    let reason = if enabled {
        format!(
            "NPU detected and local AI runtime configured: {}",
            devices.join(", ")
        )
    } else if npu_hardware_found {
        "No active NPU runtime was detected. Using CPU fallback.".to_string()
    } else {
        "No supported active NPU runtime was detected. Using CPU fallback.".to_string()
    };

    AiCapabilities {
        npu_available: enabled,
        semantic_search: enabled,
        automatic_summaries: enabled,
        image_classification: enabled,
        local_embeddings: enabled,
        reason,
    }
}

fn ai_status_label(capabilities: &AiCapabilities) -> &'static str {
    if capabilities.npu_available {
        "NPU Accelerated"
    } else {
        "CPU Fallback"
    }
}

#[tauri::command]
fn get_ai_capabilities(state: State<'_, AppState>) -> AiCapabilities {
    if let Ok(mut cached) = state.ai_capabilities.lock() {
        if let Some(capabilities) = cached.clone() {
            return capabilities;
        }
        let capabilities = compute_ai_capabilities();
        *cached = Some(capabilities.clone());
        capabilities
    } else {
        compute_ai_capabilities()
    }
}

#[tauri::command]
fn ai_semantic_search(
    state: State<'_, AppState>,
    query: String,
    path: String,
    max_results: Option<usize>,
) -> Result<Vec<FileEntry>, String> {
    let capabilities = get_ai_capabilities(state.clone());
    if !capabilities.semantic_search {
        return Err(capabilities.reason);
    }
    search_files(state, query, path, max_results, Some(true))
}

#[tauri::command]
fn ai_summarize_file(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let capabilities = get_ai_capabilities(state.clone());
    if !capabilities.automatic_summaries {
        return Err(capabilities.reason);
    }

    let preview = read_preview(state, path, Some(64 * 1024))?;
    if let Some(text) = preview.text {
        let first = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(4)
            .collect::<Vec<_>>()
            .join(" ");
        return Ok(if first.chars().count() > 500 {
            format!("{}...", first.chars().take(500).collect::<String>())
        } else {
            first
        });
    }
    Ok(format!("{} file preview is available.", preview.kind))
}

#[tauri::command]
fn get_bookmarks(app: AppHandle) -> Vec<Bookmark> {
    let path = bookmarks_path(&app);
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(bookmarks) = serde_json::from_str::<Vec<Bookmark>>(&data) {
            return bookmarks
                .into_iter()
                .filter(|bookmark| Path::new(&bookmark.path).exists())
                .collect();
        }
    }

    let mut bookmarks = Vec::new();
    for folder in get_known_folders() {
        if matches!(folder.id.as_str(), "documents" | "downloads" | "desktop") {
            bookmarks.push(Bookmark {
                name: folder.name,
                path: folder.path,
            });
        }
    }
    bookmarks
}

#[tauri::command]
fn save_bookmarks(app: AppHandle, bookmarks: Vec<Bookmark>) -> Result<(), String> {
    let path = bookmarks_path(&app);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(&bookmarks).map_err(|e| e.to_string())?;
    fs::write(&path, data).map_err(|e| e.to_string())
}

#[tauri::command]
fn minimize_window(window: Window) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_maximize_window(window: Window) -> Result<(), String> {
    if window.is_maximized().map_err(|e| e.to_string())? {
        window.unmaximize().map_err(|e| e.to_string())
    } else {
        window.maximize().map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn close_window(window: Window) -> Result<(), String> {
    window.close().map_err(|e| e.to_string())
}

// ───── helpers ─────

fn app_data_file(app: &AppHandle, name: &str) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(name)
}

fn read_json_file<T: serde::de::DeserializeOwned>(path: &Path, fallback: T) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(fallback)
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(path, data).map_err(|e| e.to_string())
}

// ───── checksum ─────

#[tauri::command]
fn get_checksum(path: String) -> Result<HashMap<String, String>, String> {
    let mut file = File::open(&path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let mut result = HashMap::new();
    result.insert("sha256".to_string(), hex::encode(hasher.finalize()));
    Ok(result)
}

// ───── terminal ─────

#[tauri::command]
fn open_terminal(path: String) -> Result<(), String> {
    let dir = if Path::new(&path).is_dir() {
        path.clone()
    } else {
        Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(path)
    };

    #[cfg(target_os = "windows")]
    {
        ProcessCommand::new("wt")
            .args(["-d", &dir])
            .spawn()
            .or_else(|_| {
                ProcessCommand::new("powershell")
                    .args([
                        "-NoExit",
                        "-Command",
                        &format!("Set-Location '{}'", dir.replace('\'', "''")),
                    ])
                    .spawn()
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        open::that(dir).map_err(|e| e.to_string())
    }
}

// ───── file notes ─────

#[tauri::command]
fn get_all_notes(app: AppHandle) -> HashMap<String, String> {
    read_json_file(&app_data_file(&app, "notes.json"), HashMap::new())
}

#[tauri::command]
fn save_file_note(app: AppHandle, path: String, note: String) -> Result<(), String> {
    let file = app_data_file(&app, "notes.json");
    let mut notes: HashMap<String, String> = read_json_file(&file, HashMap::new());
    if note.trim().is_empty() {
        notes.remove(&path);
    } else {
        notes.insert(path, note.trim().to_string());
    }
    write_json_file(&file, &notes)
}

// ───── batch rename ─────

#[tauri::command]
fn batch_rename(state: State<'_, AppState>, ops: Vec<RenameOp>) -> Result<Vec<String>, String> {
    let mut completed = Vec::new();
    for op in &ops {
        let src = Path::new(&op.from);
        let dst = Path::new(&op.to);
        if dst.exists() {
            return Err(format!("'{}' already exists", dst.display()));
        }
        fs::rename(src, dst).map_err(|e| format!("{}: {}", op.from, e))?;
        state.invalidate_path(src);
        state.invalidate_path(dst);
        state.log_op("rename", &op.from, Some(&op.to));
        completed.push(op.to.clone());
    }
    Ok(completed)
}

// ───── git status ─────

fn parse_git_porcelain(stdout: &[u8], base_path: &str) -> GitStatusMap {
    let mut statuses = GitStatusMap::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let name = line[3..].trim();
        let name = if name.contains(" -> ") {
            name.split(" -> ").last().unwrap_or(name)
        } else {
            name
        };
        let status = match xy.trim() {
            "M" | "MM" => "modified",
            "A" | "AM" => "added",
            "D" => "deleted",
            "R" | "RM" => "renamed",
            "??" => "untracked",
            _ if xy.contains('M') => "modified",
            _ => continue,
        };
        statuses.insert(
            PathBuf::from(base_path)
                .join(name)
                .to_string_lossy()
                .into_owned(),
            status.to_string(),
        );
    }
    statuses
}

#[tauri::command]
fn get_git_status(state: State<'_, AppState>, path: String) -> Result<GitStatusMap, String> {
    let key = cache_key_str(&path);
    if let Ok(cache) = state.git_cache.lock() {
        if let Some((arc, at)) = cache.get(&key) {
            if at.elapsed() < Duration::from_secs(10) {
                return Ok((**arc).clone());
            }
        }
    }

    let output = ProcessCommand::new("git")
        .args(["-C", &path, "status", "--porcelain", "-u"])
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err("Not a git repository".to_string());
    }

    let arc = Arc::new(parse_git_porcelain(&output.stdout, &path));
    if let Ok(mut cache) = state.git_cache.lock() {
        cache.insert(key, (Arc::clone(&arc), Instant::now()));
        if cache.len() > 32 {
            if let Some(k) = cache.keys().next().cloned() {
                cache.remove(&k);
            }
        }
    }
    Ok((*arc).clone())
}

// ───── image info ─────

#[tauri::command]
fn get_image_info(path: String) -> Result<ImageInfo, String> {
    let img = image::open(&path).map_err(|e| e.to_string())?;
    let ext = extension(Path::new(&path));
    Ok(ImageInfo {
        width: img.width(),
        height: img.height(),
        format: ext.to_uppercase(),
    })
}

// ───── duplicate finder ─────

#[tauri::command]
fn find_duplicates(path: String, min_size: Option<u64>) -> Result<Vec<Vec<FileEntry>>, String> {
    let dir = PathBuf::from(&path);
    let min = min_size.unwrap_or(4096);

    let items: Vec<(String, FileEntry)> = WalkDir::new(&dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .par_bridge()
        .filter_map(|entry| {
            let p = entry.path().to_path_buf();
            let meta = fs::metadata(&p).ok()?;
            if meta.len() < min {
                return None;
            }
            let mut file = File::open(&p).ok()?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf).ok()?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let hash = hex::encode(hasher.finalize());
            Some((hash, path_to_entry(&p, &meta)))
        })
        .collect();

    let mut map: HashMap<String, Vec<FileEntry>> = HashMap::new();
    for (hash, entry) in items {
        map.entry(hash).or_default().push(entry);
    }

    let mut groups: Vec<Vec<FileEntry>> = map.into_values().filter(|g| g.len() > 1).collect();
    groups.sort_by(|a, b| {
        let sa: u64 = a.iter().map(|e| e.size).sum();
        let sb: u64 = b.iter().map(|e| e.size).sum();
        sb.cmp(&sa)
    });
    Ok(groups)
}

// ───── storage tree ─────

fn build_storage_tree(root: &Path, max_depth: u32) -> StorageNode {
    struct Entry {
        path: PathBuf,
        depth: u32,
        file_size: u64,
        children: Vec<usize>,
    }

    let mut entries: Vec<Entry> = vec![Entry {
        path: root.to_path_buf(),
        depth: 0,
        file_size: 0,
        children: vec![],
    }];
    let mut queue: Vec<usize> = vec![0];

    while let Some(idx) = queue.pop() {
        let dir = entries[idx].path.clone();
        let depth = entries[idx].depth;
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        let mut file_size = 0u64;
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for e in read.filter_map(Result::ok) {
            let p = e.path();
            if let Ok(m) = fs::metadata(&p) {
                if m.is_file() {
                    file_size += m.len();
                } else if m.is_dir() && depth < max_depth && !p.is_symlink() {
                    subdirs.push(p);
                }
            }
        }
        entries[idx].file_size = file_size;

        for p in subdirs {
            let child_idx = entries.len();
            entries.push(Entry {
                path: p,
                depth: depth + 1,
                file_size: 0,
                children: vec![],
            });
            entries[idx].children.push(child_idx);
            queue.push(child_idx);
        }
    }

    let n = entries.len();
    let mut sizes = vec![0u64; n];
    for i in (0..n).rev() {
        let child_sum: u64 = entries[i].children.iter().map(|&c| sizes[c]).sum();
        sizes[i] = entries[i].file_size + child_sum;
    }

    fn build(entries: &[Entry], sizes: &[u64], idx: usize) -> StorageNode {
        let e = &entries[idx];
        let name = e
            .path
            .file_name()
            .unwrap_or(e.path.as_os_str())
            .to_string_lossy()
            .to_string();
        StorageNode {
            name,
            path: e.path.to_string_lossy().to_string(),
            size: sizes[idx],
            children: e
                .children
                .iter()
                .map(|&c| build(entries, sizes, c))
                .collect(),
        }
    }

    build(&entries, &sizes, 0)
}

#[tauri::command]
fn get_storage_tree(path: String, max_depth: Option<u32>) -> Result<StorageNode, String> {
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {path}"));
    }
    Ok(build_storage_tree(&dir, max_depth.unwrap_or(3)))
}

// ───── archives ─────

#[tauri::command]
fn extract_archive(state: State<'_, AppState>, path: String, dest: String) -> Result<(), String> {
    let src = PathBuf::from(&path);
    let dst = PathBuf::from(&dest);
    fs::create_dir_all(&dst).map_err(|e| e.to_string())?;

    let file = File::open(&src).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let out = dst.join(entry.name());
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(p) = out.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            let mut outfile = File::create(&out).map_err(|e| e.to_string())?;
            io::copy(&mut entry, &mut outfile).map_err(|e| e.to_string())?;
        }
    }
    state.invalidate_path(&dst);
    Ok(())
}

#[tauri::command]
fn create_archive(
    state: State<'_, AppState>,
    paths: Vec<String>,
    dest: String,
) -> Result<(), String> {
    let dst = PathBuf::from(&dest);
    if dst.exists() {
        return Err(format!("'{}' already exists", dst.display()));
    }
    let file = File::create(&dst).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for p in &paths {
        let src = PathBuf::from(p);
        let name = src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if src.is_dir() {
            for entry in WalkDir::new(&src).into_iter().filter_map(Result::ok) {
                let rel = entry.path().strip_prefix(&src).unwrap_or(entry.path());
                let entry_name = format!("{}/{}", name, rel.to_string_lossy().replace('\\', "/"));
                if entry.file_type().is_dir() {
                    zip.add_directory(&entry_name, opts)
                        .map_err(|e| e.to_string())?;
                } else {
                    zip.start_file(&entry_name, opts)
                        .map_err(|e| e.to_string())?;
                    let mut f = File::open(entry.path()).map_err(|e| e.to_string())?;
                    io::copy(&mut f, &mut zip).map_err(|e| e.to_string())?;
                }
            }
        } else {
            zip.start_file(&name, opts).map_err(|e| e.to_string())?;
            let mut f = File::open(&src).map_err(|e| e.to_string())?;
            io::copy(&mut f, &mut zip).map_err(|e| e.to_string())?;
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    state.invalidate_path(&dst);
    Ok(())
}

// ───── saved searches ─────

#[tauri::command]
fn get_saved_searches(app: AppHandle) -> Vec<SavedSearch> {
    read_json_file(&app_data_file(&app, "searches.json"), vec![])
}

#[tauri::command]
fn save_search(app: AppHandle, name: String, query: String, scope: String) -> Result<(), String> {
    let file = app_data_file(&app, "searches.json");
    let mut searches: Vec<SavedSearch> = read_json_file(&file, vec![]);
    searches.retain(|s| s.name != name);
    searches.insert(0, SavedSearch { name, query, scope });
    if searches.len() > 50 {
        searches.truncate(50);
    }
    write_json_file(&file, &searches)
}

#[tauri::command]
fn delete_saved_search(app: AppHandle, name: String) -> Result<(), String> {
    let file = app_data_file(&app, "searches.json");
    let mut searches: Vec<SavedSearch> = read_json_file(&file, vec![]);
    searches.retain(|s| s.name != name);
    write_json_file(&file, &searches)
}

// ───── session ─────

#[tauri::command]
fn save_session(app: AppHandle, tabs: Vec<SessionTab>) -> Result<(), String> {
    write_json_file(&app_data_file(&app, "session.json"), &tabs)
}

#[tauri::command]
fn load_session(app: AppHandle) -> Result<Vec<SessionTab>, String> {
    let path = app_data_file(&app, "session.json");
    if !path.exists() {
        return Ok(vec![]);
    }
    Ok(read_json_file(&path, vec![]))
}

// ───── operation log / undo ─────

#[tauri::command]
fn get_operation_log(state: State<'_, AppState>) -> Vec<FileOp> {
    state
        .operation_log
        .lock()
        .map(|l| l.clone())
        .unwrap_or_default()
}

#[tauri::command]
fn undo_last_operation(state: State<'_, AppState>) -> Result<String, String> {
    let op = state
        .operation_log
        .lock()
        .map_err(|_| "Lock failed")?
        .pop()
        .ok_or("Nothing to undo")?;

    match op.kind.as_str() {
        "rename" => {
            let from = op.to.as_deref().ok_or("Missing destination")?;
            let to = &op.from;
            let src = Path::new(from);
            let dst = Path::new(to);
            if dst.exists() {
                return Err(format!("'{}' already exists", dst.display()));
            }
            fs::rename(src, dst).map_err(|e| e.to_string())?;
            state.invalidate_path(src);
            state.invalidate_path(dst);
            Ok(format!("Renamed back to '{}'", dst.display()))
        }
        "copy" => {
            let copied = op.to.as_deref().ok_or("Missing destination")?;
            let p = Path::new(copied);
            if p.is_dir() {
                fs::remove_dir_all(p).map_err(|e| e.to_string())?;
            } else {
                fs::remove_file(p).map_err(|e| e.to_string())?;
            }
            state.invalidate_path(p);
            Ok(format!("Deleted copy '{}'", p.display()))
        }
        "move" => {
            let from = op.to.as_deref().ok_or("Missing destination")?;
            let to = &op.from;
            let src = Path::new(from);
            let dst = Path::new(to);
            if dst.exists() {
                return Err(format!("'{}' already exists", dst.display()));
            }
            fs::rename(src, dst).map_err(|e| e.to_string())?;
            state.invalidate_path(src);
            state.invalidate_path(dst);
            Ok(format!("Moved back to '{}'", dst.display()))
        }
        _ => Err(format!("Cannot undo '{}'", op.kind)),
    }
}

fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp")
}

fn thumbnail_cache_key(path: &Path, modified: u64, px: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cache_key(path));
    hasher.update(modified.to_le_bytes());
    hasher.update(px.to_le_bytes());
    hex::encode(hasher.finalize())
}

fn thumbnail_cache_size() -> u64 {
    fs::read_dir(thumbnail_cache_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok().map(|m| m.len()))
        .sum()
}

fn thumbnail_data_url(bytes: &[u8]) -> String {
    format!(
        "data:image/jpeg;base64,{}",
        general_purpose::STANDARD.encode(bytes)
    )
}

fn read_thumbnail_from_disk(cache_key: &str, source_mtime: u64, px: u32) -> Option<String> {
    let conn = open_index_connection().ok()?;
    let row = conn
        .query_row(
            "
            SELECT file_name FROM thumbnail_cache
            WHERE cache_key = ?1 AND source_modified = ?2 AND size_px = ?3
            ",
            params![cache_key, source_mtime as i64, px as i64],
            |row| row.get::<_, String>(0),
        )
        .ok()?;
    let path = thumbnail_cache_dir().join(row);
    let bytes = fs::read(path).ok()?;
    let _ = conn.execute(
        "UPDATE thumbnail_cache SET last_accessed = ?1 WHERE cache_key = ?2",
        params![now_unix_secs() as i64, cache_key],
    );
    Some(thumbnail_data_url(&bytes))
}

fn store_thumbnail_on_disk(
    source_path: &Path,
    source_mtime: u64,
    px: u32,
    bytes: &[u8],
    limit_bytes: u64,
) -> Option<String> {
    let cache_key = thumbnail_cache_key(source_path, source_mtime, px);
    let dir = thumbnail_cache_dir();
    fs::create_dir_all(&dir).ok()?;
    mark_hidden(&dir);

    let file_name = format!("{cache_key}.jpg");
    let path = dir.join(&file_name);
    fs::write(&path, bytes).ok()?;
    let conn = open_index_connection().ok()?;
    let now = now_unix_secs() as i64;
    conn.execute(
        "
        INSERT INTO thumbnail_cache(cache_key, source_path, source_modified, size_px, file_name, byte_len, last_accessed)
        VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(cache_key) DO UPDATE SET
            source_path = excluded.source_path,
            source_modified = excluded.source_modified,
            size_px = excluded.size_px,
            file_name = excluded.file_name,
            byte_len = excluded.byte_len,
            last_accessed = excluded.last_accessed
        ",
        params![
            cache_key,
            source_path.to_string_lossy(),
            source_mtime as i64,
            px as i64,
            file_name,
            bytes.len() as i64,
            now
        ],
    )
    .ok()?;
    let _ = prune_thumbnail_cache(limit_bytes);
    Some(thumbnail_data_url(bytes))
}

fn prune_thumbnail_cache(limit_bytes: u64) -> Result<(), String> {
    let conn = open_index_connection()?;
    let mut total = conn
        .query_row(
            "SELECT COALESCE(SUM(byte_len), 0) FROM thumbnail_cache",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u64;

    if total <= limit_bytes {
        return Ok(());
    }

    let mut stmt = conn
        .prepare(
            "SELECT cache_key, file_name, byte_len FROM thumbnail_cache ORDER BY last_accessed ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?.max(0) as u64,
            ))
        })
        .map_err(|e| e.to_string())?;

    for row in rows.filter_map(Result::ok) {
        if total <= limit_bytes {
            break;
        }
        let (cache_key, file_name, byte_len) = row;
        let _ = fs::remove_file(thumbnail_cache_dir().join(file_name));
        let _ = conn.execute(
            "DELETE FROM thumbnail_cache WHERE cache_key = ?1",
            params![cache_key],
        );
        total = total.saturating_sub(byte_len);
    }
    Ok(())
}

fn clear_thumbnail_cache() -> Result<u64, String> {
    let before = thumbnail_cache_size();
    let dir = thumbnail_cache_dir();
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    if let Ok(conn) = open_index_connection() {
        let _ = conn.execute("DELETE FROM thumbnail_cache", []);
    }
    Ok(before)
}

/// Generate or return a cached JPEG thumbnail data URL.
/// Runs per-path in parallel via Rayon; returns all available thumbnails in one IPC call.
#[tauri::command]
fn fetch_thumbnails(
    state: State<'_, AppState>,
    paths: Vec<String>,
    size: Option<u32>,
) -> HashMap<String, String> {
    let app_state = state.inner().clone();
    let px = size.unwrap_or(160).clamp(64, 512);

    paths
        .par_iter()
        .filter_map(|path| {
            let path_buf = PathBuf::from(path);
            if !is_image_ext(&extension(&path_buf)) {
                return None;
            }
            let metadata = fs::metadata(&path_buf).ok()?;
            if metadata.len() > 30 * 1024 * 1024 {
                return None;
            }
            let mtime = unix_secs(metadata.modified());
            let key = format!("thumb|{}|{}|{}", cache_key(&path_buf), mtime, px);
            let disk_key = thumbnail_cache_key(&path_buf, mtime, px);

            if let Some(cached) = app_state.preview(&key) {
                return cached.data_url.map(|url| (path.clone(), url));
            }

            if let Some(data_url) = read_thumbnail_from_disk(&disk_key, mtime, px) {
                app_state.store_preview(
                    key,
                    PreviewContent {
                        kind: "image".to_string(),
                        mime: Some("image/jpeg".to_string()),
                        text: None,
                        data_url: Some(data_url.clone()),
                        truncated: false,
                    },
                );
                return Some((path.clone(), data_url));
            }

            let img = image::open(&path_buf).ok()?;
            let thumb = img.thumbnail(px, px);
            let mut buf = Vec::new();
            thumb
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
                .ok()?;
            let data_url =
                store_thumbnail_on_disk(&path_buf, mtime, px, &buf, THUMBNAIL_CACHE_LIMIT_BYTES)
                    .unwrap_or_else(|| thumbnail_data_url(&buf));
            app_state.store_preview(
                key,
                PreviewContent {
                    kind: "image".to_string(),
                    mime: Some("image/jpeg".to_string()),
                    text: None,
                    data_url: Some(data_url.clone()),
                    truncated: false,
                },
            );
            Some((path.clone(), data_url))
        })
        .collect()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
struct NativeSettings {
    theme: String,
    accent: String,
    density: String,
    wallpaper: String,
    custom_theme: Option<String>,
    index_mode: String,
    index_roots: Vec<String>,
    thumbnail_cache_limit_mb: u64,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            theme: "mica-dark".to_string(),
            accent: "blue".to_string(),
            density: "cozy".to_string(),
            wallpaper: "none".to_string(),
            custom_theme: None,
            index_mode: "low".to_string(),
            index_roots: Vec::new(),
            thumbnail_cache_limit_mb: 50,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ThemeDefinition {
    name: String,
    bg: String,
    bg_soft: String,
    panel: String,
    border: String,
    border_strong: String,
    text: String,
    text_muted: String,
    text_faint: String,
    accent: String,
    danger: String,
    success: String,
    radius: f32,
    anim_speed: f32,
    border_width: f32,
    finish: String,
    ui_font: String,
    mono_font: String,
    font_size_delta: i32,
    icon_folder_hex: String,
}

impl Default for ThemeDefinition {
    fn default() -> Self {
        Self {
            name: "My Theme".to_string(),
            bg: "#101318".to_string(),
            bg_soft: "#171b22".to_string(),
            panel: "#1c212a".to_string(),
            border: "#1e2530".to_string(),
            border_strong: "#2a3140".to_string(),
            text: "#f2f6fb".to_string(),
            text_muted: "#b5c0cf".to_string(),
            text_faint: "#7f8b9d".to_string(),
            accent: "#4f9cff".to_string(),
            danger: "#e5484d".to_string(),
            success: "#37b26c".to_string(),
            radius: 8.0,
            anim_speed: 1.0,
            border_width: 0.0,
            finish: "mica-dark".to_string(),
            ui_font: "Segoe UI".to_string(),
            mono_font: "Cascadia Mono".to_string(),
            font_size_delta: 0,
            icon_folder_hex: "#e2a934".to_string(),
        }
    }
}

#[derive(Clone)]
struct NativeClipboard {
    paths: Vec<String>,
    cut: bool,
}

enum PendingPrompt {
    Rename(String),
    NewFolder,
    Note(String),
    Archive,
    NewTemplate(FileTemplate),
    CompareFolder(String),
    BatchRename(Vec<String>),
    ConflictPaste {
        src: String,
        dest: String,
        cut: bool,
    },
}

struct NativeController {
    app_state: AppState,
    current_path: String,
    files: Vec<FileEntry>,
    visible_files: Vec<FileEntry>,
    selected_index: i32,
    selected_set: std::collections::HashSet<usize>,
    select_anchor: i32,
    files_model: Option<ModelRc<FileItem>>,
    search_query: String,
    history: Vec<String>,
    history_index: usize,
    tabs: Vec<SessionTab>,
    active_tab: usize,
    known_folders: Vec<KnownFolder>,
    drives: Vec<DriveInfo>,
    bookmarks: Vec<Bookmark>,
    recent_locations: Vec<String>,
    folder_views: HashMap<String, String>,
    tags: HashMap<String, String>,
    notes: HashMap<String, String>,
    git_status: Arc<GitStatusMap>,
    git_dir_status: HashMap<String, String>,
    settings: NativeSettings,
    ai: AiCapabilities,
    clipboard: Option<NativeClipboard>,
    pending_prompt: Option<PendingPrompt>,
    sort_by: String,
    sort_dir: String,
    thumbnail_memory: HashMap<String, slint::Image>,
    thumbnail_ready: Arc<std::sync::atomic::AtomicBool>,
    thumbnail_timer: Option<slint::Timer>,
    toast_queue: std::collections::VecDeque<(String, String)>,
    toast_showing: bool,
    toast_last_shown: Option<std::time::Instant>,
    toast_timer: Option<slint::Timer>,
    git_status_ready: Arc<std::sync::atomic::AtomicBool>,
    pending_git_status: Arc<Mutex<Option<Arc<GitStatusMap>>>>,
    operation_ready: Arc<std::sync::atomic::AtomicBool>,
    pending_operation_result: Arc<Mutex<Option<NativeOperationResult>>>,
}

#[derive(Clone)]
struct NativeOperationResult {
    message: String,
    kind: String,
    refresh: bool,
    clear_clipboard: bool,
}

struct PaletteSpec {
    bg: Color,
    bg_soft: Color,
    panel: Color,
    panel_solid: Color,
    panel_alt: Color,
    titlebar: Color,
    sidebar: Color,
    border: Color,
    border_strong: Color,
    text: Color,
    text_muted: Color,
    text_faint: Color,
    accent: Color,
    accent_soft: Color,
    accent_strong: Color,
    radius: f32,
    radius_small: f32,
    ui_font: &'static str,
    mono_font: &'static str,
    light_controls: bool,
    outer_border: f32,
}

fn native_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Pathfinder")
}

fn native_data_file(name: &str) -> PathBuf {
    native_data_dir().join(name)
}

fn native_index_file() -> PathBuf {
    native_data_file(INDEX_DB_FILE)
}

fn native_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(native_data_dir)
        .join("Pathfinder")
}

fn thumbnail_cache_dir() -> PathBuf {
    native_cache_dir().join("thumbnails")
}

fn themes_dir() -> PathBuf {
    native_data_dir().join("themes")
}

fn lighten_color(c: Color, factor: f32) -> Color {
    let r = (c.red() as f32 + (255.0 - c.red() as f32) * factor).round() as u8;
    let g = (c.green() as f32 + (255.0 - c.green() as f32) * factor).round() as u8;
    let b = (c.blue() as f32 + (255.0 - c.blue() as f32) * factor).round() as u8;
    Color::from_rgb_u8(r, g, b)
}

fn icon_folder_colors(id: &str) -> (Color, Color) {
    match id {
        "terminal" => (color("#9cffd8"), color("#45c97a")),
        "retro" => (color("#ffe46a"), color("#ffc800")),
        "cyberpunk" => (color("#ff6ae0"), color("#d000a0")),
        "paper" | "warm" | "fantasy" => (color("#f5c26a"), color("#c88c28")),
        _ => (color("#ffd86a"), color("#e2a934")),
    }
}

fn list_custom_themes() -> Vec<String> {
    let dir = themes_dir();
    if !dir.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                p.file_stem().map(|s| s.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    names.sort_unstable();
    names
}

fn save_custom_theme_def(def: &ThemeDefinition) -> Result<(), String> {
    let dir = themes_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe_name: String = def
        .name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{}.json", safe_name.trim()));
    let data = serde_json::to_string_pretty(def).map_err(|e| e.to_string())?;
    fs::write(path, data).map_err(|e| e.to_string())
}

fn load_custom_theme_def(name: &str) -> Option<ThemeDefinition> {
    let safe_name: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = themes_dir().join(format!("{}.json", safe_name.trim()));
    fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
}

fn delete_custom_theme_def(name: &str) -> Result<(), String> {
    let safe_name: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = themes_dir().join(format!("{}.json", safe_name.trim()));
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

fn list_system_fonts() -> Vec<String> {
    let result = ProcessCommand::new("powershell")
        .args([
            "-NonInteractive",
            "-Command",
            "[System.Reflection.Assembly]::LoadWithPartialName('System.Drawing') | Out-Null; \
             [System.Drawing.Text.InstalledFontCollection]::new().Families | \
             ForEach-Object { $_.Name }",
        ])
        .output();
    match result {
        Ok(out) if out.status.success() => {
            let mut fonts: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            fonts.sort_unstable();
            fonts.dedup();
            fonts
        }
        _ => vec![
            "Segoe UI".to_string(),
            "Segoe UI Variable".to_string(),
            "Calibri".to_string(),
            "Arial".to_string(),
            "Consolas".to_string(),
            "Cascadia Mono".to_string(),
            "Courier New".to_string(),
            "Georgia".to_string(),
            "Tahoma".to_string(),
            "Verdana".to_string(),
        ],
    }
}

fn color_to_hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

fn mark_hidden(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = ProcessCommand::new("attrib").arg("+H").arg(path).output();
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
    }
}

fn open_index_connection() -> Result<Connection, String> {
    let path = native_index_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    // WAL lets concurrent readers proceed while the indexer writes.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .map_err(|e| e.to_string())?;
    // 5 s before returning SQLITE_BUSY so reader/writer don't collide.
    conn.pragma_update(None, "busy_timeout", 5000i64)
        .map_err(|e| e.to_string())?;
    // 64 MB in-process page cache keeps hot index pages off disk.
    conn.pragma_update(None, "cache_size", -65536i64)
        .map_err(|e| e.to_string())?;
    // Memory-map up to 256 MB; lets OS manage hot pages without read() calls.
    conn.pragma_update(None, "mmap_size", 268435456i64)
        .map_err(|e| e.to_string())?;
    // Checkpoint every 1000 WAL pages to keep WAL file small.
    conn.pragma_update(None, "wal_autocheckpoint", 1000i64)
        .map_err(|e| e.to_string())?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS files (
            path TEXT PRIMARY KEY,
            parent TEXT NOT NULL,
            name TEXT NOT NULL,
            extension TEXT NOT NULL,
            is_dir INTEGER NOT NULL,
            size INTEGER NOT NULL,
            modified INTEGER NOT NULL,
            indexed_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_files_parent ON files(parent);
        CREATE INDEX IF NOT EXISTS idx_files_name ON files(name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);

        CREATE TABLE IF NOT EXISTS thumbnail_cache (
            cache_key TEXT PRIMARY KEY,
            source_path TEXT NOT NULL,
            source_modified INTEGER NOT NULL,
            size_px INTEGER NOT NULL,
            file_name TEXT NOT NULL,
            byte_len INTEGER NOT NULL,
            last_accessed INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_thumbnail_accessed ON thumbnail_cache(last_accessed);
        ",
    )
    .map_err(|e| e.to_string())?;
    mark_hidden(&path);
    Ok(conn)
}

fn schedule_index_directory(parent: String, entries: Vec<FileEntry>) {
    std::thread::spawn(move || {
        let _ = index_directory_entries(&parent, &entries);
    });
}

fn index_directory_entries(parent: &str, entries: &[FileEntry]) -> Result<(), String> {
    let mut conn = open_index_connection()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let paths: HashSet<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    let tx = conn.transaction().map_err(|e| e.to_string())?;

    {
        let mut upsert = tx
            .prepare(
                "
                INSERT INTO files(path, parent, name, extension, is_dir, size, modified, indexed_at)
                VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(path) DO UPDATE SET
                    parent = excluded.parent,
                    name = excluded.name,
                    extension = excluded.extension,
                    is_dir = excluded.is_dir,
                    size = excluded.size,
                    modified = excluded.modified,
                    indexed_at = excluded.indexed_at
                ",
            )
            .map_err(|e| e.to_string())?;

        for entry in entries {
            upsert
                .execute(params![
                    entry.path,
                    parent,
                    entry.name,
                    entry.extension.as_deref().unwrap_or("").to_lowercase(),
                    i64::from(entry.kind == FileKind::Directory),
                    entry.size as i64,
                    entry.modified as i64,
                    now
                ])
                .map_err(|e| e.to_string())?;
        }
    }

    let stale_paths = {
        let mut select = tx
            .prepare("SELECT path FROM files WHERE parent = ?1")
            .map_err(|e| e.to_string())?;
        let rows = select
            .query_map(params![parent], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut stale = Vec::new();
        for path in rows.flatten() {
            if !paths.contains(path.as_str()) {
                stale.push(path);
            }
        }
        stale
    };

    for path in stale_paths {
        tx.execute("DELETE FROM files WHERE path = ?1", params![path])
            .map_err(|e| e.to_string())?;
    }

    tx.commit().map_err(|e| e.to_string())?;
    let _ = conn.execute_batch("PRAGMA incremental_vacuum(16); PRAGMA optimize;");
    Ok(())
}

fn like_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn index_search(root: &str, query: &str, max: usize) -> Result<Vec<FileEntry>, String> {
    let query = query.trim();
    if query.len() < 2 {
        return Ok(Vec::new());
    }

    let conn = open_index_connection()?;
    let root_prefix = format!("{}%", root.trim_end_matches(['\\', '/']));
    let (name_like, ext_exact) = if let Some(ext) = query.strip_prefix("ext:") {
        ("%".to_string(), ext.trim_start_matches('.').to_lowercase())
    } else if let Some(name) = query.strip_prefix("name:") {
        (format!("%{}%", like_escape(name)), String::new())
    } else {
        (format!("%{}%", like_escape(query)), query.to_lowercase())
    };

    let mut stmt = conn
        .prepare(
            "
            SELECT path, name, is_dir, size, modified, extension
            FROM files
            WHERE path LIKE ?1 ESCAPE '\\'
              AND (name LIKE ?2 ESCAPE '\\' OR extension = ?3)
            ORDER BY is_dir DESC, name COLLATE NOCASE ASC
            LIMIT ?4
            ",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map(
            params![root_prefix, name_like, ext_exact, max as i64],
            |row| {
                let is_dir = row.get::<_, i64>(2)? == 1;
                let ext = row.get::<_, String>(5)?;
                let name: String = row.get(1)?;
                Ok(FileEntry {
                    path: row.get(0)?,
                    name_lower: name.to_lowercase(),
                    name,
                    kind: if is_dir {
                        FileKind::Directory
                    } else {
                        FileKind::File
                    },
                    size: row.get::<_, i64>(3)?.max(0) as u64,
                    modified: row.get::<_, i64>(4)?.max(0) as u64,
                    extension: (!ext.is_empty()).then_some(ext),
                })
            },
        )
        .map_err(|e| e.to_string())?;

    Ok(rows.filter_map(Result::ok).collect())
}

fn suggest_paths(prefix: &str, max: usize) -> Vec<String> {
    let prefix = prefix.trim();
    if prefix.len() < 2 {
        return Vec::new();
    }
    let Ok(conn) = open_index_connection() else {
        return Vec::new();
    };
    // Match directories whose path starts with the typed prefix (case-insensitive)
    let pattern = format!("{}%", like_escape(prefix));
    let mut stmt = match conn.prepare(
        "SELECT path FROM files WHERE is_dir = 1 AND path LIKE ?1 ESCAPE '\\' ORDER BY path ASC LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(params![pattern, max as i64], |row| row.get::<_, String>(0))
        .ok()
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn index_stats() -> IndexStatus {
    let (indexed_files, index_bytes) = open_index_connection()
        .map(|conn| {
            let indexed_files = conn
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
                .unwrap_or(0)
                .max(0) as u64;
            let page_count = conn
                .query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0))
                .unwrap_or(0)
                .max(0) as u64;
            let page_size = conn
                .query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))
                .unwrap_or(4096)
                .max(0) as u64;
            (indexed_files, page_count.saturating_mul(page_size))
        })
        .unwrap_or((0, 0));

    IndexStatus {
        mode: "low".to_string(),
        indexed_files,
        index_bytes,
        thumbnail_bytes: thumbnail_cache_size(),
        thumbnail_limit: THUMBNAIL_CACHE_LIMIT_BYTES,
        estimated_storage: "Low uses only visited folders, usually under 50 MB.".to_string(),
        roots: Vec::new(),
    }
}

fn common_index_roots() -> Vec<String> {
    let mut roots = Vec::new();
    for folder in get_known_folders() {
        if matches!(
            folder.id.as_str(),
            "desktop" | "documents" | "downloads" | "pictures"
        ) {
            roots.push(folder.path);
        }
    }
    if let Some(home) = dirs::home_dir() {
        for candidate in ["Projects", "Dev", "Code", "source", "repos"] {
            let path = home.join(candidate);
            if path.is_dir() {
                roots.push(path.to_string_lossy().to_string());
            }
        }
    }
    roots.sort_unstable();
    roots.dedup();
    roots
}

fn index_roots_for_mode(settings: &NativeSettings) -> Vec<String> {
    match settings.index_mode.as_str() {
        "balanced" => common_index_roots(),
        "fast" => {
            if settings.index_roots.is_empty() {
                common_index_roots()
            } else {
                settings.index_roots.clone()
            }
        }
        "max" => get_drives()
            .into_iter()
            .filter(|drive| drive.kind == "local")
            .map(|drive| drive.path)
            .collect(),
        _ => Vec::new(),
    }
}

fn estimate_index_storage(roots: &[String], mode: &str) -> String {
    if mode == "low" {
        return "Low uses only folders you open, usually under 50 MB.".to_string();
    }

    let mut sampled = 0_u64;
    let mut capped = false;
    for root in roots {
        for entry in WalkDir::new(root)
            .max_depth(3)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.path().is_file() || entry.path().is_dir() {
                sampled += 1;
                if sampled >= 20_000 {
                    capped = true;
                    break;
                }
            }
        }
        if capped {
            break;
        }
    }

    let estimated = if capped {
        sampled
            .saturating_mul(INDEX_ESTIMATE_BYTES_PER_FILE)
            .saturating_mul(3)
    } else {
        sampled.saturating_mul(INDEX_ESTIMATE_BYTES_PER_FILE)
    };
    if capped {
        format!(
            "{}+ sampled. Estimated index storage starts around {} and can grow as more files are indexed.",
            sampled,
            format_size_short(estimated)
        )
    } else {
        format!(
            "{} items found in a quick scan. Estimated index storage: {}.",
            sampled,
            format_size_short(estimated)
        )
    }
}

fn index_status_for_settings(settings: &NativeSettings) -> IndexStatus {
    let roots = index_roots_for_mode(settings);
    let mut status = index_stats();
    status.mode = settings.index_mode.clone();
    status.estimated_storage = estimate_index_storage(&roots, &settings.index_mode);
    status.roots = roots;
    status.thumbnail_limit = settings
        .thumbnail_cache_limit_mb
        .max(1)
        .saturating_mul(1024 * 1024);
    status
}

fn schedule_index_roots(roots: Vec<String>) {
    if roots.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for root in roots {
            let root_path = PathBuf::from(&root);
            if !root_path.is_dir() {
                continue;
            }
            let mut by_parent: HashMap<String, Vec<FileEntry>> = HashMap::new();
            for entry in WalkDir::new(&root_path)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .map(|name| !matches!(name, "$Recycle.Bin" | "System Volume Information"))
                        .unwrap_or(true)
                })
                .filter_map(Result::ok)
            {
                let path = entry.path();
                let Ok(metadata) = fs::metadata(path) else {
                    continue;
                };
                let Some(parent) = path.parent() else {
                    continue;
                };
                by_parent
                    .entry(parent.to_string_lossy().to_string())
                    .or_default()
                    .push(path_to_entry(path, &metadata));

                if by_parent.len() > 128 {
                    let batch = std::mem::take(&mut by_parent);
                    for (parent, entries) in batch {
                        let _ = index_directory_entries(&parent, &entries);
                    }
                }
            }
            for (parent, entries) in by_parent {
                let _ = index_directory_entries(&parent, &entries);
            }
        }
    });
}

fn read_native_json<T: serde::de::DeserializeOwned>(name: &str, fallback: T) -> T {
    fs::read_to_string(native_data_file(name))
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or(fallback)
}

fn write_native_json<T: Serialize>(name: &str, value: &T) -> Result<(), String> {
    let path = native_data_file(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(path, data).map_err(|e| e.to_string())
}

fn default_smart_folders(current_path: &str) -> Vec<SmartFolder> {
    let scope = current_path.to_string();
    vec![
        SmartFolder {
            id: "large".to_string(),
            name: "Large files".to_string(),
            query: "size:>100mb".to_string(),
            scope: scope.clone(),
            description: "Files larger than 100 MB in this location".to_string(),
        },
        SmartFolder {
            id: "recent".to_string(),
            name: "Recently modified".to_string(),
            query: "modified:week".to_string(),
            scope: scope.clone(),
            description: "Items changed this week".to_string(),
        },
        SmartFolder {
            id: "old-downloads".to_string(),
            name: "Downloads over 30 days old".to_string(),
            query: "smart:old-downloads".to_string(),
            scope: dirs::download_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(scope.clone()),
            description: "Older files in Downloads that may be cleanup candidates".to_string(),
        },
        SmartFolder {
            id: "screenshots".to_string(),
            name: "Screenshots".to_string(),
            query: "kind:image name:screenshot".to_string(),
            scope: scope.clone(),
            description: "Image files with screenshot in the name".to_string(),
        },
        SmartFolder {
            id: "git-untracked".to_string(),
            name: "Untracked git files".to_string(),
            query: "smart:git-untracked".to_string(),
            scope,
            description: "Files marked untracked by git status".to_string(),
        },
    ]
}

fn default_file_templates() -> Vec<FileTemplate> {
    vec![
        FileTemplate {
            name: "Text note".to_string(),
            extension: "txt".to_string(),
            content: String::new(),
        },
        FileTemplate {
            name: "Markdown note".to_string(),
            extension: "md".to_string(),
            content: "# New Note\n".to_string(),
        },
        FileTemplate {
            name: "JSON file".to_string(),
            extension: "json".to_string(),
            content: "{\n  \n}\n".to_string(),
        },
        FileTemplate {
            name: "PowerShell script".to_string(),
            extension: "ps1".to_string(),
            content: "# New script\n".to_string(),
        },
    ]
}

fn default_automation_rules() -> Vec<AutomationRule> {
    vec![AutomationRule {
        name: "Tag review PDFs in Downloads".to_string(),
        folder: dirs::download_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        extension: "pdf".to_string(),
        tag: "yellow".to_string(),
        move_to: None,
        enabled: false,
    }]
}

fn compare_folders(
    left: &Path,
    right: &Path,
    max: usize,
) -> Result<Vec<FolderCompareEntry>, String> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();

    for base in [left, right] {
        for entry in WalkDir::new(base)
            .into_iter()
            .filter_map(Result::ok)
            .take(max)
        {
            let rel = entry
                .path()
                .strip_prefix(base)
                .ok()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if rel.is_empty() || !seen.insert(rel.clone()) {
                continue;
            }
            let l = left.join(&rel);
            let r = right.join(&rel);
            let lm = fs::metadata(&l).ok();
            let rm = fs::metadata(&r).ok();
            let left_size = lm.as_ref().map(|m| m.len()).unwrap_or(0);
            let right_size = rm.as_ref().map(|m| m.len()).unwrap_or(0);
            let left_modified = lm.as_ref().map(|m| unix_secs(m.modified())).unwrap_or(0);
            let right_modified = rm.as_ref().map(|m| unix_secs(m.modified())).unwrap_or(0);
            let status = match (lm.is_some(), rm.is_some()) {
                (true, false) => "left-only",
                (false, true) => "right-only",
                (true, true) if left_size != right_size => "size-diff",
                (true, true) if left_modified != right_modified => "date-diff",
                (true, true) => "same",
                _ => "missing",
            }
            .to_string();
            rows.push(FolderCompareEntry {
                path: rel,
                left_exists: lm.is_some(),
                right_exists: rm.is_some(),
                left_size,
                right_size,
                left_modified,
                right_modified,
                status,
            });
        }
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows.truncate(max);
    Ok(rows)
}

fn color(hex: &str) -> Color {
    let value = hex.trim_start_matches('#');
    let parsed = u32::from_str_radix(value, 16).unwrap_or(0);
    let r = ((parsed >> 16) & 0xff) as u8;
    let g = ((parsed >> 8) & 0xff) as u8;
    let b = (parsed & 0xff) as u8;
    Color::from_rgb_u8(r, g, b)
}

fn rgba_u8(r: u8, g: u8, b: u8, alpha: f32) -> Color {
    Color::from_argb_u8((alpha.clamp(0.0, 1.0) * 255.0).round() as u8, r, g, b)
}

fn model_from_vec<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

fn build_breadcrumbs(path: &str) -> Vec<ChoiceItem> {
    let mut crumbs = Vec::new();
    let mut accumulated = String::with_capacity(path.len() + 1);
    for part in path.split(['/', '\\']) {
        if part.is_empty() {
            continue;
        }
        accumulated.push_str(part);
        accumulated.push('\\');
        crumbs.push(ChoiceItem {
            id: ss(accumulated.trim_end_matches('\\')),
            label: ss(part),
            description: ss(""),
            color: slint::Color::from_argb_u8(0, 0, 0, 0),
        });
    }
    crumbs
}

fn ss(value: impl Into<String>) -> SharedString {
    SharedString::from(value.into())
}

fn same_path_string(left: &str, right: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        left.replace('/', "\\")
            .eq_ignore_ascii_case(&right.replace('/', "\\"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        left == right
    }
}

fn native_bookmarks() -> Vec<Bookmark> {
    let saved: Vec<Bookmark> = read_native_json("bookmarks.json", Vec::new());
    if !saved.is_empty() {
        return saved
            .into_iter()
            .filter(|bookmark| Path::new(&bookmark.path).exists())
            .collect();
    }

    get_known_folders()
        .into_iter()
        .filter(|folder| matches!(folder.id.as_str(), "documents" | "downloads" | "desktop"))
        .map(|folder| Bookmark {
            name: folder.name,
            path: folder.path,
        })
        .collect()
}

fn native_list_directory(state: &AppState, path: &str) -> Result<Vec<FileEntry>, String> {
    if let Some(entries) = state.cached_directory(path) {
        return Ok(entries);
    }
    let entries = list_directory_uncached(&PathBuf::from(path))?;
    state.store_directory(path, entries.clone());
    schedule_index_directory(path.to_string(), entries.clone());
    Ok(entries)
}

fn native_read_preview(
    state: &AppState,
    path: &str,
    max_bytes: Option<usize>,
) -> Result<PreviewContent, String> {
    let path_buf = PathBuf::from(path);
    let metadata = fs::metadata(&path_buf).map_err(|e| e.to_string())?;
    let key = format!(
        "{}|{}|{}",
        cache_key(&path_buf),
        unix_secs(metadata.modified()),
        max_bytes.unwrap_or(4 * 1024)
    );
    if let Some(content) = state.preview(&key) {
        return Ok(content);
    }
    let content = read_preview_uncached(&path_buf, &metadata, max_bytes)?;
    state.store_preview(key, content.clone());
    Ok(content)
}

fn native_git_status(state: &AppState, path: &str) -> Arc<GitStatusMap> {
    if !is_inside_git_worktree(Path::new(path)) {
        return Arc::new(GitStatusMap::new());
    }

    let key = cache_key_str(path);
    if let Ok(cache) = state.git_cache.lock() {
        if let Some((arc, loaded_at)) = cache.get(&key) {
            if loaded_at.elapsed() < Duration::from_secs(10) {
                return Arc::clone(arc);
            }
        }
    }

    let output = ProcessCommand::new("git")
        .args([
            "-C",
            path,
            "status",
            "--porcelain",
            "--untracked-files=normal",
        ])
        .output();

    let Ok(output) = output else {
        return Arc::new(GitStatusMap::new());
    };
    if !output.status.success() {
        return Arc::new(GitStatusMap::new());
    }

    let arc = Arc::new(parse_git_porcelain(&output.stdout, path));
    if let Ok(mut cache) = state.git_cache.lock() {
        cache.insert(key, (Arc::clone(&arc), Instant::now()));
        if cache.len() > 32 {
            if let Some(k) = cache.keys().next().cloned() {
                cache.remove(&k);
            }
        }
    }

    arc
}

fn is_inside_git_worktree(path: &Path) -> bool {
    let mut current = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };

    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent();
    }

    false
}

fn native_rename(state: &AppState, path: &str, new_name: &str) -> Result<String, String> {
    if state.queue_is_paused() {
        return Err("Operation queue is paused.".to_string());
    }
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if new_name.contains('/') || new_name.contains('\\') {
        return Err("Name cannot contain path separators".to_string());
    }

    let src = PathBuf::from(path);
    let parent = src.parent().ok_or("No parent directory")?;
    let dst = parent.join(new_name);
    if dst.exists() && !same_destination(&src, &dst) {
        return Err(format!("'{new_name}' already exists"));
    }

    let op_id = state.queue_start("rename", path, Some(&dst.to_string_lossy()), 0);
    let started = Instant::now();
    fs::rename(&src, &dst).map_err(|e| e.to_string())?;
    state.invalidate_path(&src);
    state.invalidate_path(&dst);
    state.log_op("rename", path, Some(&dst.to_string_lossy()));
    state.queue_finish(op_id, "done", "Renamed", 0, started.elapsed());
    Ok(dst.to_string_lossy().to_string())
}

fn native_delete(state: &AppState, path: &str) -> Result<(), String> {
    if state.queue_is_paused() {
        return Err("Operation queue is paused.".to_string());
    }
    let path_buf = PathBuf::from(path);
    if !path_buf.exists() {
        return Err(format!("Path does not exist: {path}"));
    }
    let total = folder_size_quick(&path_buf, 25_000);
    let op_id = state.queue_start("delete", path, None, total);
    let started = Instant::now();
    trash::delete(&path_buf).map_err(|e| e.to_string())?;
    state.invalidate_path(&path_buf);
    state.log_op("delete", path, None);
    state.queue_finish(
        op_id,
        "done",
        "Moved to Recycle Bin",
        total,
        started.elapsed(),
    );
    Ok(())
}

fn native_create_directory(state: &AppState, path: &str) -> Result<(), String> {
    if state.queue_is_paused() {
        return Err("Operation queue is paused.".to_string());
    }
    let path_buf = PathBuf::from(path);
    if path_buf.exists() {
        return Err(format!("Folder already exists: {}", path_buf.display()));
    }
    fs::create_dir_all(&path_buf).map_err(|e| e.to_string())?;
    state.invalidate_path(&path_buf);
    Ok(())
}

fn native_copy(state: &AppState, from: &str, to: &str) -> Result<(), String> {
    if state.queue_is_paused() {
        return Err("Operation queue is paused.".to_string());
    }
    let src = PathBuf::from(from);
    let mut dst = PathBuf::from(to);
    if dst.exists() {
        let op_id = state.queue_start("copy", from, Some(&dst.to_string_lossy()), 0);
        state.queue_conflict(op_id, conflict_info(&src, &dst));
        dst = keep_both_destination(&dst);
    }
    let total = folder_size_quick(&src, 25_000);
    let op_id = state.queue_start("copy", from, Some(&dst.to_string_lossy()), total);
    let started = Instant::now();
    let result = if src.is_dir() {
        copy_dir_recursive(&src, &dst)
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::copy(&src, &dst).map(|_| ()).map_err(|e| e.to_string())
    };
    if result.is_ok() {
        state.invalidate_path(&dst);
        state.log_op("copy", from, Some(&dst.to_string_lossy()));
        state.queue_finish(op_id, "done", "Copied", total, started.elapsed());
    } else if let Err(error) = &result {
        state.queue_finish(op_id, "failed", error.clone(), 0, started.elapsed());
    }
    result
}

fn native_move(state: &AppState, from: &str, to: &str) -> Result<(), String> {
    if state.queue_is_paused() {
        return Err("Operation queue is paused.".to_string());
    }
    let src = PathBuf::from(from);
    let mut dst = PathBuf::from(to);
    if dst.exists() {
        let op_id = state.queue_start("move", from, Some(&dst.to_string_lossy()), 0);
        state.queue_conflict(op_id, conflict_info(&src, &dst));
        dst = keep_both_destination(&dst);
    }
    let total = folder_size_quick(&src, 25_000);
    let op_id = state.queue_start("move", from, Some(&dst.to_string_lossy()), total);
    let started = Instant::now();
    if fs::rename(&src, &dst).is_err() {
        if src.is_dir() {
            copy_dir_recursive(&src, &dst)?;
            fs::remove_dir_all(&src).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::copy(&src, &dst).map_err(|e| e.to_string())?;
            fs::remove_file(&src).map_err(|e| e.to_string())?;
        }
    }
    state.invalidate_path(&src);
    state.invalidate_path(&dst);
    state.log_op("move", from, Some(&dst.to_string_lossy()));
    state.queue_finish(op_id, "done", "Moved", total, started.elapsed());
    Ok(())
}

fn format_size_short(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut index = 0;
    while size >= 1024.0 && index < units.len() - 1 {
        size /= 1024.0;
        index += 1;
    }
    if index > 0 && size < 10.0 {
        format!("{size:.1} {}", units[index])
    } else {
        format!("{} {}", size.round() as u64, units[index])
    }
}

fn format_modified(secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(secs);
    if secs == 0 {
        String::new()
    } else if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{} min ago", diff / 60)
    } else if diff < 86_400 {
        format!("{} hr ago", diff / 3600)
    } else if diff < 604_800 {
        format!("{} day ago", diff / 86_400)
    } else {
        format!("{} days ago", diff / 86_400)
    }
}

fn entry_type(entry: &FileEntry) -> String {
    if entry.kind == FileKind::Directory {
        return "Folder".to_string();
    }
    match entry
        .extension
        .as_deref()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "" => "File".to_string(),
        "jpg" | "jpeg" => "JPEG image".to_string(),
        "png" => "PNG image".to_string(),
        "gif" => "GIF image".to_string(),
        "webp" => "WebP image".to_string(),
        "pdf" => "PDF document".to_string(),
        "md" => "Markdown".to_string(),
        "txt" => "Text document".to_string(),
        "zip" => "ZIP archive".to_string(),
        "rs" => "Rust source".to_string(),
        "js" => "JavaScript".to_string(),
        "ts" => "TypeScript".to_string(),
        "html" => "HTML document".to_string(),
        "css" => "CSS stylesheet".to_string(),
        ext => format!("{} file", ext.to_uppercase()),
    }
}

fn tag_color(id: &str) -> Color {
    match id {
        "red" => color("#e5484d"),
        "orange" => color("#e3862a"),
        "yellow" => color("#d7b125"),
        "green" => color("#2aa96b"),
        "blue" => color("#4f9cff"),
        "violet" => color("#8b6cff"),
        _ => rgba_u8(0, 0, 0, 0.0),
    }
}

fn git_color(status: &str) -> Color {
    match status {
        "modified" => color("#d98a24"),
        "added" => color("#2aa96b"),
        "deleted" => color("#e5484d"),
        "renamed" => color("#8b6cff"),
        "untracked" => color("#7f8b9d"),
        _ => rgba_u8(0, 0, 0, 0.0),
    }
}

fn git_label(status: &str) -> &'static str {
    match status {
        "modified" => "M",
        "added" => "A",
        "deleted" => "D",
        "renamed" => "R",
        "untracked" => "U",
        _ => "",
    }
}

fn accent_override(id: &str) -> (Color, Color, Color) {
    match id {
        "amber" => (
            color("#d98a24"),
            rgba_u8(217, 138, 36, 0.17),
            color("#f0ae4d"),
        ),
        "green" => (
            color("#2aa96b"),
            rgba_u8(42, 169, 107, 0.17),
            color("#45c985"),
        ),
        "violet" => (
            color("#8b6cff"),
            rgba_u8(139, 108, 255, 0.18),
            color("#aa93ff"),
        ),
        "rose" => (
            color("#e45578"),
            rgba_u8(228, 85, 120, 0.17),
            color("#ff7a99"),
        ),
        "teal" => (
            color("#1aa6a6"),
            rgba_u8(26, 166, 166, 0.17),
            color("#31caca"),
        ),
        _ => (
            color("#4f9cff"),
            rgba_u8(79, 156, 255, 0.16),
            color("#78b6ff"),
        ),
    }
}

fn theme_palette(id: &str) -> PaletteSpec {
    let mut p = match id {
        "mica-light" => PaletteSpec {
            bg: color("#eef2f7"),
            bg_soft: color("#f6f8fb"),
            panel: rgba_u8(255, 255, 255, 0.78),
            panel_solid: color("#ffffff"),
            panel_alt: color("#edf1f6"),
            titlebar: rgba_u8(242, 246, 251, 0.92),
            sidebar: rgba_u8(236, 241, 247, 0.86),
            border: rgba_u8(33, 52, 78, 0.13),
            border_strong: rgba_u8(33, 52, 78, 0.23),
            text: color("#19202a"),
            text_muted: color("#526173"),
            text_faint: color("#7f8c9d"),
            accent: color("#4f9cff"),
            accent_soft: rgba_u8(79, 156, 255, 0.16),
            accent_strong: color("#72b3ff"),
            radius: 8.0,
            radius_small: 5.0,
            ui_font: "Segoe UI",
            mono_font: "Cascadia Mono",
            light_controls: true,
            outer_border: 0.0,
        },
        "warm" => PaletteSpec {
            bg: color("#f2eee6"),
            bg_soft: color("#faf6ee"),
            panel: rgba_u8(255, 250, 241, 0.84),
            panel_solid: color("#fffaf1"),
            panel_alt: color("#eee4d3"),
            titlebar: rgba_u8(240, 231, 216, 0.92),
            sidebar: rgba_u8(235, 225, 210, 0.88),
            border: rgba_u8(95, 75, 46, 0.15),
            border_strong: rgba_u8(95, 75, 46, 0.28),
            text: color("#2a241d"),
            text_muted: color("#65594d"),
            text_faint: color("#928576"),
            accent: color("#d07920"),
            accent_soft: rgba_u8(208, 121, 32, 0.16),
            accent_strong: color("#b96218"),
            radius: 8.0,
            radius_small: 5.0,
            ui_font: "Segoe UI",
            mono_font: "Cascadia Mono",
            light_controls: true,
            outer_border: 0.0,
        },
        "flat" => PaletteSpec {
            bg: color("#f7f8fa"),
            bg_soft: color("#ffffff"),
            panel: color("#ffffff"),
            panel_solid: color("#ffffff"),
            panel_alt: color("#f0f2f5"),
            titlebar: color("#ffffff"),
            sidebar: color("#f3f5f8"),
            border: color("#e3e7ee"),
            border_strong: color("#cdd5e0"),
            text: color("#161a20"),
            text_muted: color("#526071"),
            text_faint: color("#7d8795"),
            accent: color("#4f6fdc"),
            accent_soft: rgba_u8(79, 111, 220, 0.15),
            accent_strong: color("#3d5ac8"),
            radius: 4.0,
            radius_small: 3.0,
            ui_font: "Segoe UI",
            mono_font: "Cascadia Mono",
            light_controls: true,
            outer_border: 0.0,
        },
        "terminal" => PaletteSpec {
            bg: color("#07110d"),
            bg_soft: color("#0d1b14"),
            panel: color("#0b1812"),
            panel_solid: color("#0b1812"),
            panel_alt: color("#10261b"),
            titlebar: color("#06100b"),
            sidebar: color("#08130d"),
            border: rgba_u8(68, 255, 153, 0.22),
            border_strong: rgba_u8(68, 255, 153, 0.42),
            text: color("#c8ffd8"),
            text_muted: color("#7de59f"),
            text_faint: color("#4f996b"),
            accent: color("#7cff9d"),
            accent_soft: rgba_u8(124, 255, 157, 0.12),
            accent_strong: color("#c8ffd8"),
            radius: 0.0,
            radius_small: 0.0,
            ui_font: "Cascadia Mono",
            mono_font: "Cascadia Mono",
            light_controls: false,
            outer_border: 0.0,
        },
        "paper" => PaletteSpec {
            bg: color("#eadfc9"),
            bg_soft: color("#f8efd9"),
            panel: color("#f5ead1"),
            panel_solid: color("#f5ead1"),
            panel_alt: color("#dfcfad"),
            titlebar: color("#e4d4b3"),
            sidebar: color("#deceb0"),
            border: rgba_u8(88, 63, 34, 0.20),
            border_strong: rgba_u8(88, 63, 34, 0.34),
            text: color("#332617"),
            text_muted: color("#6b573d"),
            text_faint: color("#957f5e"),
            accent: color("#9f3f2c"),
            accent_soft: rgba_u8(159, 63, 44, 0.14),
            accent_strong: color("#7f2f21"),
            radius: 5.0,
            radius_small: 3.0,
            ui_font: "Georgia",
            mono_font: "Cascadia Mono",
            light_controls: true,
            outer_border: 0.0,
        },
        "retro" => PaletteSpec {
            bg: color("#24205e"),
            bg_soft: color("#342c86"),
            panel: color("#302878"),
            panel_solid: color("#302878"),
            panel_alt: color("#4539a5"),
            titlebar: color("#171446"),
            sidebar: color("#211b62"),
            border: color("#f8e76d"),
            border_strong: color("#fff4a8"),
            text: color("#fff7b0"),
            text_muted: color("#aeeaff"),
            text_faint: color("#f59bd1"),
            accent: color("#ffcf3f"),
            accent_soft: rgba_u8(255, 207, 63, 0.18),
            accent_strong: color("#ffef7a"),
            radius: 0.0,
            radius_small: 0.0,
            ui_font: "Consolas",
            mono_font: "Consolas",
            light_controls: false,
            outer_border: 4.0,
        },
        "fantasy" => PaletteSpec {
            bg: color("#d9c38f"),
            bg_soft: color("#f0dfaf"),
            panel: rgba_u8(244, 226, 179, 0.90),
            panel_solid: color("#f4e2b3"),
            panel_alt: color("#ceb16f"),
            titlebar: color("#c6a562"),
            sidebar: color("#d3b976"),
            border: rgba_u8(80, 50, 22, 0.28),
            border_strong: rgba_u8(80, 50, 22, 0.48),
            text: color("#2f2113"),
            text_muted: color("#604323"),
            text_faint: color("#876b3c"),
            accent: color("#9b6716"),
            accent_soft: rgba_u8(155, 103, 22, 0.16),
            accent_strong: color("#74450e"),
            radius: 6.0,
            radius_small: 5.0,
            ui_font: "Georgia",
            mono_font: "Cascadia Mono",
            light_controls: true,
            outer_border: 0.0,
        },
        "cyberpunk" => PaletteSpec {
            bg: color("#100727"),
            bg_soft: color("#190b3d"),
            panel: rgba_u8(30, 13, 68, 0.88),
            panel_solid: color("#1e0d44"),
            panel_alt: color("#28105f"),
            titlebar: color("#0b051f"),
            sidebar: color("#13082f"),
            border: rgba_u8(0, 236, 255, 0.24),
            border_strong: rgba_u8(255, 57, 188, 0.54),
            text: color("#f6f2ff"),
            text_muted: color("#9cecff"),
            text_faint: color("#ce6cff"),
            accent: color("#ff39bc"),
            accent_soft: rgba_u8(255, 57, 188, 0.18),
            accent_strong: color("#00ecff"),
            radius: 3.0,
            radius_small: 2.0,
            ui_font: "Segoe UI",
            mono_font: "Cascadia Mono",
            light_controls: false,
            outer_border: 0.0,
        },
        _ => PaletteSpec {
            bg: color("#101318"),
            bg_soft: color("#171b22"),
            panel: rgba_u8(28, 33, 42, 0.82),
            panel_solid: color("#1c212a"),
            panel_alt: color("#232936"),
            titlebar: rgba_u8(18, 22, 29, 0.92),
            sidebar: rgba_u8(22, 26, 34, 0.84),
            border: rgba_u8(157, 172, 196, 0.16),
            border_strong: rgba_u8(185, 198, 220, 0.26),
            text: color("#f2f6fb"),
            text_muted: color("#b5c0cf"),
            text_faint: color("#7f8b9d"),
            accent: color("#4f9cff"),
            accent_soft: rgba_u8(79, 156, 255, 0.16),
            accent_strong: color("#72b3ff"),
            radius: 8.0,
            radius_small: 5.0,
            ui_font: "Segoe UI",
            mono_font: "Cascadia Mono",
            light_controls: false,
            outer_border: 0.0,
        },
    };

    let (accent, accent_soft, accent_strong) = accent_override(id);
    if !matches!(
        id,
        "warm" | "terminal" | "paper" | "retro" | "fantasy" | "cyberpunk"
    ) {
        p.accent = accent;
        p.accent_soft = accent_soft;
        p.accent_strong = accent_strong;
    }
    p
}

fn apply_theme(ui: &MainWindow, settings: &NativeSettings) {
    if let Some(custom_name) = &settings.custom_theme {
        if let Some(def) = load_custom_theme_def(custom_name) {
            apply_custom_theme_to_ui(ui, &def);
            let metrics = ui.global::<AppMetrics>();
            apply_density_metrics(&metrics, &settings.density);
            ui.set_active_theme(ss("custom"));
            ui.set_active_accent(ss(&settings.accent));
            ui.set_active_density(ss(&settings.density));
            return;
        }
    }

    let mut palette = theme_palette(&settings.theme);
    let (accent, accent_soft, accent_strong) = accent_override(&settings.accent);
    palette.accent = accent;
    palette.accent_soft = accent_soft;
    palette.accent_strong = accent_strong;

    let global = ui.global::<ThemePalette>();
    global.set_bg(palette.bg);
    global.set_bg_soft(palette.bg_soft);
    global.set_panel(palette.panel);
    global.set_panel_solid(palette.panel_solid);
    global.set_panel_alt(palette.panel_alt);
    global.set_titlebar(palette.titlebar);
    global.set_sidebar(palette.sidebar);
    global.set_border(palette.border);
    global.set_border_strong(palette.border_strong);
    global.set_text(palette.text);
    global.set_text_muted(palette.text_muted);
    global.set_text_faint(palette.text_faint);
    global.set_accent(palette.accent);
    global.set_accent_soft(palette.accent_soft);
    global.set_accent_strong(palette.accent_strong);
    global.set_danger(color("#e5484d"));
    global.set_success(color("#37b26c"));
    global.set_warning(color("#e3a524"));

    let (folder_1, folder_2) = icon_folder_colors(&settings.theme);
    global.set_icon_folder_1(folder_1);
    global.set_icon_folder_2(folder_2);

    let metrics = ui.global::<AppMetrics>();
    metrics.set_radius(palette.radius);
    metrics.set_radius_small(palette.radius_small);
    metrics.set_outer_border(palette.outer_border);
    metrics.set_ui_font(ss(palette.ui_font));
    metrics.set_mono_font(ss(palette.mono_font));
    metrics.set_light_controls(palette.light_controls);
    apply_density_metrics(&metrics, &settings.density);

    ui.set_active_theme(ss(&settings.theme));
    ui.set_active_accent(ss(&settings.accent));
    ui.set_active_density(ss(&settings.density));
}

fn apply_density_metrics(metrics: &AppMetrics, density: &str) {
    match density {
        "compact" => {
            metrics.set_row_h(26.0);
            metrics.set_grid_w(104.0);
            metrics.set_grid_h(94.0);
            metrics.set_pad(8.0);
        }
        "comfortable" => {
            metrics.set_row_h(32.0);
            metrics.set_grid_w(120.0);
            metrics.set_grid_h(108.0);
            metrics.set_pad(12.0);
        }
        _ => {
            metrics.set_row_h(38.0);
            metrics.set_grid_w(136.0);
            metrics.set_grid_h(122.0);
            metrics.set_pad(16.0);
        }
    }
}

fn apply_custom_theme_to_ui(ui: &MainWindow, def: &ThemeDefinition) {
    let global = ui.global::<ThemePalette>();

    let bg = color(&def.bg);
    let bg_soft = color(&def.bg_soft);
    let panel_c = color(&def.panel);
    let border_c = color(&def.border);
    let border_strong_c = color(&def.border_strong);
    let text_c = color(&def.text);
    let text_muted_c = color(&def.text_muted);
    let text_faint_c = color(&def.text_faint);
    let accent_c = color(&def.accent);
    let danger_c = color(&def.danger);
    let success_c = color(&def.success);

    let is_light = {
        let r = bg.red() as u32;
        let g = bg.green() as u32;
        let b = bg.blue() as u32;
        (r * 299 + g * 587 + b * 114) / 1000 > 128
    };

    let panel_alpha: u8 = if is_light { 200 } else { 210 };
    let sidebar_alpha: u8 = if is_light { 220 } else { 214 };

    global.set_bg(bg);
    global.set_bg_soft(bg_soft);
    global.set_panel(Color::from_argb_u8(
        panel_alpha,
        panel_c.red(),
        panel_c.green(),
        panel_c.blue(),
    ));
    global.set_panel_solid(panel_c);
    global.set_panel_alt(color(&def.panel));
    global.set_titlebar(Color::from_argb_u8(235, bg.red(), bg.green(), bg.blue()));
    global.set_sidebar(Color::from_argb_u8(
        sidebar_alpha,
        panel_c.red(),
        panel_c.green(),
        panel_c.blue(),
    ));
    global.set_border(Color::from_argb_u8(
        41,
        border_c.red(),
        border_c.green(),
        border_c.blue(),
    ));
    global.set_border_strong(Color::from_argb_u8(
        66,
        border_strong_c.red(),
        border_strong_c.green(),
        border_strong_c.blue(),
    ));
    global.set_text(text_c);
    global.set_text_muted(text_muted_c);
    global.set_text_faint(text_faint_c);
    global.set_accent(accent_c);
    global.set_accent_soft(Color::from_argb_u8(
        41,
        accent_c.red(),
        accent_c.green(),
        accent_c.blue(),
    ));
    global.set_accent_strong(lighten_color(accent_c, 0.2));
    global.set_danger(danger_c);
    global.set_success(success_c);
    global.set_warning(color("#e3a524"));

    let folder_base = if def.icon_folder_hex.is_empty() {
        color("#e2a934")
    } else {
        color(&def.icon_folder_hex)
    };
    global.set_icon_folder_1(lighten_color(folder_base, 0.28));
    global.set_icon_folder_2(folder_base);

    let metrics = ui.global::<AppMetrics>();
    metrics.set_radius(def.radius);
    metrics.set_radius_small(def.radius * 0.625);
    metrics.set_outer_border(def.border_width);
    metrics.set_ui_font(ss(&def.ui_font));
    metrics.set_mono_font(ss(&def.mono_font));
    metrics.set_light_controls(is_light);

    let base_row_h = 38.0_f32;
    let size_delta = def.font_size_delta as f32 * 4.0;
    metrics.set_row_h(base_row_h + size_delta);
    metrics.set_grid_w(136.0 + size_delta * 3.0);
    metrics.set_grid_h(122.0 + size_delta * 3.0);
}

fn sync_editor_state(ui: &MainWindow, def: &ThemeDefinition) {
    ui.set_ce_name(ss(&def.name));
    ui.set_ce_finish(ss(&def.finish));
    ui.set_ce_radius(def.radius);
    ui.set_ce_anim_speed(def.anim_speed);
    ui.set_ce_ui_font(ss(&def.ui_font));
    ui.set_ce_mono_font(ss(&def.mono_font));
    ui.set_ce_font_size_delta(def.font_size_delta);
    ui.set_ce_icon_folder_hex(ss(&def.icon_folder_hex));
    ui.set_ce_icon_set(ss(""));
    ui.set_ce_selected_token(-1);
    ui.set_ce_token_hex(ss(""));
    ui.set_ce_token_label(ss(""));

    let tokens = [
        &def.bg,
        &def.bg_soft,
        &def.panel,
        &def.border,
        &def.border_strong,
        &def.text,
        &def.text_muted,
        &def.text_faint,
        &def.accent,
        &def.danger,
        &def.success,
    ];
    let colors: Vec<Color> = tokens.iter().map(|h| color(h)).collect();
    let hexes: Vec<SharedString> = tokens.iter().map(|h| ss(*h)).collect();
    ui.set_ce_token_colors(model_from_vec(colors));
    ui.set_ce_token_hexes(model_from_vec(hexes));
}

fn editor_def_from_ui(ui: &MainWindow) -> ThemeDefinition {
    use slint::Model;
    let hexes = ui.get_ce_token_hexes();
    let get_hex = |i: usize| hexes.row_data(i).map(|s| s.to_string()).unwrap_or_default();
    ThemeDefinition {
        name: ui.get_ce_name().to_string(),
        bg: get_hex(0),
        bg_soft: get_hex(1),
        panel: get_hex(2),
        border: get_hex(3),
        border_strong: get_hex(4),
        text: get_hex(5),
        text_muted: get_hex(6),
        text_faint: get_hex(7),
        accent: get_hex(8),
        danger: get_hex(9),
        success: get_hex(10),
        radius: ui.get_ce_radius(),
        anim_speed: ui.get_ce_anim_speed(),
        border_width: 0.0,
        finish: ui.get_ce_finish().to_string(),
        ui_font: ui.get_ce_ui_font().to_string(),
        mono_font: ui.get_ce_mono_font().to_string(),
        font_size_delta: ui.get_ce_font_size_delta(),
        icon_folder_hex: ui.get_ce_icon_folder_hex().to_string(),
    }
}

#[cfg(target_os = "windows")]
fn apply_window_finish(ui: &MainWindow, finish: &str) {
    use i_slint_backend_winit::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use i_slint_backend_winit::WinitWindowAccessor;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};

    const DWMWA_SYSTEMBACKDROP_TYPE_ID: i32 = 38;
    const DWMSBT_MAINWINDOW: i32 = 2;
    const DWMSBT_NONE: i32 = 1;

    let backdrop = match finish {
        "mica-dark" | "mica-light" => DWMSBT_MAINWINDOW,
        _ => DWMSBT_NONE,
    };

    ui.window().with_winit_window(|window| {
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(DWMWA_SYSTEMBACKDROP_TYPE_ID),
                &backdrop as *const _ as *const _,
                std::mem::size_of_val(&backdrop) as u32,
            );
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn apply_window_finish(_ui: &MainWindow, _finish: &str) {}

fn choice_items(items: &[(&str, &str, &str, &str)]) -> ModelRc<ChoiceItem> {
    model_from_vec(
        items
            .iter()
            .map(|(id, label, description, color_value)| ChoiceItem {
                id: ss(*id),
                label: ss(*label),
                description: ss(*description),
                color: color(color_value),
            })
            .collect(),
    )
}

const ALL_COMMANDS: &[(&str, &str, &str, &str)] = &[
    ("Navigation", "New Tab", "Ctrl+T", "new-tab"),
    ("Navigation", "Close Tab", "Ctrl+W", "close-tab"),
    ("Navigation", "Refresh", "F5", "refresh"),
    ("Files", "New Folder", "Ctrl+Shift+N", "new-folder"),
    ("Files", "Rename", "F2", "rename"),
    ("Files", "Delete", "Del", "delete"),
    ("Files", "Copy", "Ctrl+C", "copy"),
    ("Files", "Cut", "Ctrl+X", "cut"),
    ("Files", "Paste", "Ctrl+V", "paste"),
    ("Files", "Select All", "Ctrl+A", "select-all"),
    ("Files", "Batch Rename", "", "batch-rename"),
    ("Tools", "Checksum", "", "checksum"),
    ("Tools", "Storage Treemap", "", "storage"),
    ("Tools", "Find Duplicates", "", "duplicates"),
    ("Tools", "Operation Log", "", "operation-log"),
    ("Tools", "Operation Queue", "", "operation-queue"),
    ("Tools", "Pause Operation Queue", "", "queue-pause"),
    ("Tools", "Resume Operation Queue", "", "queue-resume"),
    ("Tools", "Cancel Queued Operations", "", "queue-cancel"),
    ("Tools", "Locked File Inspector", "", "locked-file"),
    ("Tools", "Native Properties", "Alt+Enter", "properties"),
    ("Tools", "Show More Options", "", "show-more-options"),
    ("Tools", "Cloud State", "", "cloud-state"),
    ("Tools", "New From Template", "", "new-template"),
    ("Tools", "Power Rename Presets", "", "rename-presets"),
    ("Tools", "Image Tools", "", "image-tools"),
    ("Tools", "Archive Browser", "", "archive-browser"),
    ("Tools", "Compare Folder", "", "compare-folder"),
    ("Tools", "Rules and Automation", "", "rules"),
    ("Tools", "Smart Folders", "", "smart-folders"),
    ("Tools", "Home Page", "", "home-page"),
    ("Tools", "Libraries", "", "libraries"),
    ("Tools", "Recent Locations", "", "recent-locations"),
    ("Tools", "Copy As Path", "", "copy-as-path"),
    ("Tools", "Copy As PowerShell Path", "", "copy-as-powershell"),
    ("Tools", "Copy As URI", "", "copy-as-uri"),
    ("Tools", "Breadcrumb Siblings", "", "breadcrumb-siblings"),
    ("Tools", "Performance Debug Panel", "", "performance-debug"),
    (
        "Tools",
        "Clear Thumbnail Cache",
        "",
        "clear-thumbnail-cache",
    ),
    ("Tools", "Rebuild Search Index", "", "rebuild-index"),
    (
        "Settings",
        "Search Performance Settings",
        "",
        "performance-settings",
    ),
    ("Settings", "Shortcut Editor", "", "shortcut-editor"),
    ("Tools", "Undo Last Operation", "Ctrl+Z", "undo"),
    ("View", "Icon View", "Ctrl+1", "view-grid"),
    ("View", "Details View", "Ctrl+2", "view-list"),
    ("View", "Gallery View", "Ctrl+3", "view-gallery"),
    ("View", "Toggle Preview", "Ctrl+I", "toggle-preview"),
    ("View", "Toggle Dual Pane", "F3", "toggle-dual"),
    ("Settings", "Open Settings", "Ctrl+,", "settings"),
];

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = needle.chars();
    let mut current = chars.next();
    for ch in haystack.chars() {
        if current
            .map(|expected| expected.eq_ignore_ascii_case(&ch))
            .unwrap_or(false)
        {
            current = chars.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

fn command_items_filtered(query: &str) -> ModelRc<CommandItem> {
    let q = query.to_lowercase();
    model_from_vec(
        ALL_COMMANDS
            .iter()
            .filter(|(group, label, _, command)| {
                q.is_empty()
                    || label.to_lowercase().contains(&q)
                    || group.to_lowercase().contains(&q)
                    || command.to_lowercase().contains(&q)
                    || fuzzy_match(label, &q)
                    || fuzzy_match(command, &q)
            })
            .map(|(group, label, hint, command)| CommandItem {
                group: ss(*group),
                label: ss(*label),
                hint: ss(*hint),
                command: ss(*command),
            })
            .collect(),
    )
}

fn command_items() -> ModelRc<CommandItem> {
    command_items_filtered("")
}

impl NativeController {
    fn new() -> Self {
        let app_state = AppState::default();
        let settings = read_native_json("settings.json", NativeSettings::default());
        let tabs: Vec<SessionTab> = read_native_json("session.json", Vec::new());
        let known_folders = get_known_folders();
        let home = get_home_directory()
            .ok()
            .or_else(|| known_folders.first().map(|folder| folder.path.clone()))
            .unwrap_or_else(|| ".".to_string());
        let current_path = tabs
            .first()
            .map(|tab| tab.path.clone())
            .filter(|path| Path::new(path).is_dir())
            .unwrap_or(home);

        Self {
            app_state,
            current_path: current_path.clone(),
            files: Vec::new(),
            visible_files: Vec::new(),
            selected_index: -1,
            selected_set: std::collections::HashSet::new(),
            select_anchor: -1,
            files_model: None,
            search_query: String::new(),
            history: vec![current_path],
            history_index: 0,
            tabs: if tabs.is_empty() {
                vec![SessionTab {
                    path: String::new(),
                    view: "grid".to_string(),
                    sort_by: "name".to_string(),
                    sort_dir: "asc".to_string(),
                }]
            } else {
                tabs
            },
            active_tab: 0,
            known_folders,
            drives: get_drives(),
            bookmarks: native_bookmarks(),
            recent_locations: read_native_json("recent_locations.json", Vec::new()),
            folder_views: read_native_json("folder_views.json", HashMap::new()),
            tags: read_native_json("tags.json", HashMap::new()),
            notes: read_native_json("notes.json", HashMap::new()),
            git_status: Arc::new(HashMap::new()),
            git_dir_status: HashMap::new(),
            settings,
            ai: AiCapabilities {
                npu_available: false,
                semantic_search: false,
                automatic_summaries: false,
                image_classification: false,
                local_embeddings: false,
                reason: "Detecting...".to_string(),
            },
            clipboard: None,
            pending_prompt: None,
            sort_by: "name".to_string(),
            sort_dir: "asc".to_string(),
            thumbnail_memory: HashMap::new(),
            thumbnail_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thumbnail_timer: None,
            toast_queue: std::collections::VecDeque::new(),
            toast_showing: false,
            toast_last_shown: None,
            toast_timer: None,
            git_status_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_git_status: Arc::new(Mutex::new(None)),
            operation_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_operation_result: Arc::new(Mutex::new(None)),
        }
    }

    fn initialize_ui(&mut self, ui: &MainWindow) {
        ui.set_theme_choices(choice_items(&[
            (
                "mica-dark",
                "Mica Dark",
                "Windows-style dark Fluent",
                "#101318",
            ),
            (
                "mica-light",
                "Mica Light",
                "Windows-style light Mica",
                "#eef2f7",
            ),
            ("warm", "Warm Neutral", "Soft desktop workspace", "#d07920"),
            ("flat", "Flat White", "Quiet and minimal", "#ffffff"),
            (
                "terminal",
                "Terminal",
                "Phosphor green command room",
                "#7cff9d",
            ),
            ("paper", "Paper", "Letterpress and ink", "#eadfc9"),
            ("retro", "Retro Arcade", "16-bit desktop energy", "#ffcf3f"),
            (
                "fantasy",
                "High Fantasy",
                "Parchment and gilded UI",
                "#d9c38f",
            ),
            ("cyberpunk", "Cyberpunk", "Neon city file grid", "#ff39bc"),
        ]));
        ui.set_accent_choices(choice_items(&[
            ("blue", "Blue", "Default Pathfinder blue", "#4f9cff"),
            ("amber", "Amber", "Warm amber controls", "#d98a24"),
            ("green", "Green", "Quiet success green", "#2aa96b"),
            ("violet", "Violet", "Soft violet accents", "#8b6cff"),
            ("rose", "Rose", "Pink-red highlight", "#e45578"),
            ("teal", "Teal", "Cool teal accent", "#1aa6a6"),
        ]));
        ui.set_density_choices(choice_items(&[
            ("cozy", "Cozy", "38px rows and larger icons", "#4f9cff"),
            (
                "comfortable",
                "Comfortable",
                "Balanced row and grid sizing",
                "#8b6cff",
            ),
            (
                "compact",
                "Compact",
                "Dense rows and tight spacing",
                "#2aa96b",
            ),
        ]));
        ui.set_index_choices(choice_items(&[
            (
                "low",
                "Low",
                "Visited folders only, lowest storage",
                "#2aa96b",
            ),
            (
                "balanced",
                "Balanced",
                "Desktop, Documents, Downloads, Pictures, and projects",
                "#4f9cff",
            ),
            (
                "fast",
                "Fast",
                "Selected roots, with common folders as fallback",
                "#8b6cff",
            ),
            ("max", "Max", "All fixed drives, highest storage", "#d98a24"),
        ]));
        ui.set_command_items(command_items());
        ui.set_ai_device(ss(&self.ai.reason));
        ui.set_ai_label(ss(ai_status_label(&self.ai)));
        self.sync_performance_status(ui);
        apply_theme(ui, &self.settings);

        // Initialize custom theme editor with defaults or active custom theme
        let init_def = if let Some(name) = &self.settings.custom_theme {
            load_custom_theme_def(name).unwrap_or_default()
        } else {
            ThemeDefinition::default()
        };
        sync_editor_state(ui, &init_def);
        let saved = list_custom_themes();
        ui.set_ce_saved_themes(model_from_vec(
            saved.into_iter().map(SharedString::from).collect(),
        ));

        let path = self.current_path.clone();
        self.navigate(ui, path, false);
    }

    fn show_toast(&mut self, ui: &MainWindow, message: impl Into<String>) {
        self.show_toast_kind(ui, message, "info");
    }

    fn show_toast_kind(&mut self, ui: &MainWindow, message: impl Into<String>, kind: &str) {
        self.toast_queue
            .push_back((message.into(), kind.to_string()));
        if !self.toast_showing {
            self.advance_toast_display(ui);
        }
    }

    fn advance_toast_display(&mut self, ui: &MainWindow) {
        if let Some((msg, kind)) = self.toast_queue.pop_front() {
            ui.set_toast_text(ss(&msg));
            ui.set_toast_kind(ss(&kind));
            self.toast_showing = true;
            self.toast_last_shown = Some(std::time::Instant::now());
        } else {
            ui.set_toast_text(ss(""));
            self.toast_showing = false;
            self.toast_last_shown = None;
        }
    }

    fn save_settings(&self) {
        let _ = write_native_json("settings.json", &self.settings);
    }

    fn sync_performance_status(&self, ui: &MainWindow) {
        let status = index_status_for_settings(&self.settings);
        ui.set_active_index_mode(ss(&self.settings.index_mode));
        ui.set_index_status(ss(format!(
            "{} indexed | index {} | thumbnails {} of {} | {}",
            status.indexed_files,
            format_size_short(status.index_bytes),
            format_size_short(status.thumbnail_bytes),
            format_size_short(status.thumbnail_limit),
            status.estimated_storage
        )));
        ui.set_thumbnail_status(ss(format!(
            "Thumbnail cache is capped at {} and old thumbnails are removed automatically.",
            format_size_short(status.thumbnail_limit)
        )));
    }

    fn set_index_mode(&mut self, ui: &MainWindow, mode: &str) {
        self.settings.index_mode = mode.to_string();
        self.save_settings();
        self.sync_performance_status(ui);
        let roots = index_roots_for_mode(&self.settings);
        if roots.is_empty() {
            self.show_toast(
                ui,
                "Low indexing enabled. Pathfinder will index folders as you open them.",
            );
        } else {
            schedule_index_roots(roots);
            self.show_toast(ui, "Background indexing started.");
        }
    }

    fn save_session(&self) {
        let _ = write_native_json("session.json", &self.tabs);
    }

    fn selected_entry(&self) -> Option<FileEntry> {
        self.visible_files
            .get(self.selected_index as usize)
            .cloned()
    }

    fn selected_paths(&self) -> Vec<String> {
        if self.selected_set.is_empty() {
            return self
                .selected_entry()
                .map(|entry| vec![entry.path])
                .unwrap_or_default();
        }

        let mut sorted: Vec<usize> = self.selected_set.iter().copied().collect();
        sorted.sort_unstable();
        sorted
            .into_iter()
            .filter_map(|i| self.visible_files.get(i))
            .map(|entry| entry.path.clone())
            .collect()
    }

    fn apply_sort(&mut self) {
        let sort_by = self.sort_by.clone();
        let sort_dir = self.sort_dir.clone();
        self.visible_files.sort_by(|a, b| {
            // Directories always first when sorting by name
            if sort_by == "name" {
                match (&a.kind, &b.kind) {
                    (FileKind::Directory, FileKind::Directory) => {}
                    (FileKind::Directory, _) => return std::cmp::Ordering::Less,
                    (_, FileKind::Directory) => return std::cmp::Ordering::Greater,
                    _ => {}
                }
            }
            let ord = match sort_by.as_str() {
                "size" => a.size.cmp(&b.size),
                "modified" => a.modified.cmp(&b.modified),
                "type" => {
                    let ta = a.extension.as_deref().unwrap_or("").to_lowercase();
                    let tb = b.extension.as_deref().unwrap_or("").to_lowercase();
                    ta.cmp(&tb)
                }
                _ => a.name_lower.cmp(&b.name_lower),
            };
            if sort_dir == "desc" {
                ord.reverse()
            } else {
                ord
            }
        });
    }

    fn apply_filter(&mut self) {
        let query = self.search_query.trim().to_lowercase();
        self.visible_files.clear();
        if query.is_empty() {
            self.visible_files.extend_from_slice(&self.files);
            self.apply_sort();
            return;
        }
        if let Some(id) = query.strip_prefix("smart:") {
            let now = now_unix_secs();
            for entry in &self.files {
                let ext = entry.extension.as_deref().unwrap_or("").to_lowercase();
                let matched = match id {
                    "large" => entry.kind != FileKind::Directory && entry.size > 100 * 1024 * 1024,
                    "recent" => entry.modified >= now.saturating_sub(7 * 24 * 60 * 60),
                    "old-downloads" => {
                        entry.kind != FileKind::Directory
                            && self.current_path
                                == dirs::download_dir()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .unwrap_or_default()
                            && entry.modified < now.saturating_sub(30 * 24 * 60 * 60)
                    }
                    "screenshots" => {
                        matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp")
                            && entry.name_lower.contains("screenshot")
                    }
                    "git-untracked" => self.git_for_entry(entry) == "untracked",
                    _ => false,
                };
                if matched {
                    self.visible_files.push(entry.clone());
                }
            }
            self.apply_sort();
            return;
        }
        for entry in &self.files {
            let ext = entry.extension.as_deref().unwrap_or("").to_lowercase();
            let matched = if let Some(expected) = query.strip_prefix("ext:") {
                ext == expected.trim_start_matches('.')
            } else if let Some(expected) = query.strip_prefix("name:") {
                entry.name_lower.contains(expected)
            } else if let Some(expected) = query.strip_prefix("tag:") {
                self.tags
                    .get(&entry.path)
                    .map(|tag| tag == expected)
                    .unwrap_or(false)
            } else if let Some(expected) = query.strip_prefix("kind:") {
                let kind = if entry.kind == FileKind::Directory {
                    "folder"
                } else {
                    match ext.as_str() {
                        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "ico" => "image",
                        "mp4" | "mov" | "mkv" | "avi" | "webm" | "wmv" => "video",
                        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" => "audio",
                        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" => {
                            "doc"
                        }
                        _ => "file",
                    }
                };
                kind == expected
            } else {
                entry.name_lower.contains(&query) || ext.contains(&query)
            };
            if matched {
                self.visible_files.push(entry.clone());
            }
        }
        self.apply_sort();
    }

    fn update_status(&self, ui: &MainWindow) {
        let total = self.visible_files.len();
        let sel = &self.selected_set;
        let sel_count = sel.len();

        let left = if sel_count == 0 {
            let dirs = self
                .visible_files
                .iter()
                .filter(|e| e.kind == FileKind::Directory)
                .count();
            let files = total - dirs;
            match (dirs, files) {
                (0, f) => format!("{f} file{}", if f == 1 { "" } else { "s" }),
                (d, 0) => format!("{d} folder{}", if d == 1 { "" } else { "s" }),
                (d, f) => format!(
                    "{d} folder{} · {f} file{}",
                    if d == 1 { "" } else { "s" },
                    if f == 1 { "" } else { "s" }
                ),
            }
        } else {
            let sel_size: u64 = sel
                .iter()
                .filter_map(|&i| self.visible_files.get(i))
                .map(|e| e.size)
                .sum();
            format!("{sel_count} selected · {}", format_size_short(sel_size))
        };

        ui.set_status_left(ss(left));
        ui.set_status_right(ss(&self.current_path));
    }

    fn update_models(&mut self, ui: &MainWindow) {
        // Pre-load any thumbnails that are cached on disk but not yet in memory
        if ui.get_view_mode() != "list" {
            for entry in self.visible_files.iter().take(96) {
                let ext = entry.extension.as_deref().unwrap_or("").to_lowercase();
                if !is_image_ext(&ext) || self.thumbnail_memory.contains_key(&entry.path) {
                    continue;
                }
                let disk_key = thumbnail_cache_key(Path::new(&entry.path), entry.modified, 160);
                let thumb_path = thumbnail_cache_dir().join(format!("{disk_key}.jpg"));
                if !thumb_path.exists() {
                    continue;
                }
                if let Ok(img) = image::open(&thumb_path).map(|i| i.into_rgba8()) {
                    let (w, h) = img.dimensions();
                    let raw = img.into_raw();
                    let buf =
                        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&raw, w, h);
                    self.thumbnail_memory
                        .insert(entry.path.clone(), slint::Image::from_rgba8(buf));
                    // Evict oldest entries when memory cache exceeds 256 thumbnails (~40 MB at 160px)
                    const MAX_THUMB_CACHE: usize = 256;
                    if self.thumbnail_memory.len() > MAX_THUMB_CACHE {
                        let remove_count = self.thumbnail_memory.len() - MAX_THUMB_CACHE;
                        let keys: Vec<String> = self
                            .thumbnail_memory
                            .keys()
                            .take(remove_count)
                            .cloned()
                            .collect();
                        for k in keys {
                            self.thumbnail_memory.remove(&k);
                        }
                    }
                }
            }
        }

        ui.set_sort_by(ss(&self.sort_by));
        ui.set_sort_dir(ss(&self.sort_dir));
        let items: Vec<FileItem> = self
            .visible_files
            .iter()
            .enumerate()
            .map(|(i, entry)| self.file_item(entry, self.selected_set.contains(&i)))
            .collect();
        let model = model_from_vec(items);
        ui.set_files(model.clone());
        self.files_model = Some(model);
        ui.set_side_items(model_from_vec(self.side_items()));
        ui.set_tabs(model_from_vec(self.tab_items()));
        ui.set_selected_index(self.selected_index);
        ui.set_current_path(ss(&self.current_path));
        ui.set_address_text(ss(&self.current_path));
        ui.set_search_text(ss(&self.search_query));
        ui.set_breadcrumbs(model_from_vec(build_breadcrumbs(&self.current_path)));
        self.update_status(ui);
    }

    fn update_selection_in_model(&mut self, ui: &MainWindow, changed: &[usize]) {
        if let Some(model) = &self.files_model {
            use slint::Model;
            for &i in changed {
                if let Some(entry) = self.visible_files.get(i) {
                    let item = self.file_item(entry, self.selected_set.contains(&i));
                    if let Some(m) = model.as_any().downcast_ref::<VecModel<FileItem>>() {
                        m.set_row_data(i, item);
                    }
                }
            }
        }
        ui.set_selected_index(self.selected_index);
        self.update_status(ui);
    }

    fn file_item(&self, entry: &FileEntry, selected: bool) -> FileItem {
        let tag_id = self.tags.get(&entry.path).cloned().unwrap_or_default();
        let git_status = self.git_for_entry(entry);
        let (has_thumbnail, thumbnail) = self
            .thumbnail_memory
            .get(&entry.path)
            .map(|img| (true, img.clone()))
            .unwrap_or((false, slint::Image::default()));
        FileItem {
            name: ss(&entry.name),
            file_path: ss(&entry.path),
            is_dir: entry.kind == FileKind::Directory,
            size_text: ss(if entry.kind == FileKind::Directory {
                String::new()
            } else {
                format_size_short(entry.size)
            }),
            modified_text: ss(format_modified(entry.modified)),
            type_text: ss(entry_type(entry)),
            extension: ss(entry
                .extension
                .clone()
                .unwrap_or_else(|| "file".to_string())
                .to_uppercase()),
            has_tag: !tag_id.is_empty(),
            tag_color: tag_color(&tag_id),
            git_badge: ss(git_label(&git_status)),
            git_color: git_color(&git_status),
            has_note: self.notes.contains_key(&entry.path),
            is_selected: selected,
            has_thumbnail,
            thumbnail,
        }
    }

    fn rebuild_git_dir_status(&mut self) {
        self.git_dir_status.clear();
        for (file_path, status) in self.git_status.iter() {
            let mut p = Path::new(file_path);
            while let Some(parent) = p.parent() {
                let key = parent.to_string_lossy().into_owned();
                if key.is_empty() {
                    break;
                }
                self.git_dir_status
                    .entry(key)
                    .or_insert_with(|| status.clone());
                p = parent;
            }
        }
    }

    fn git_for_entry(&self, entry: &FileEntry) -> String {
        if let Some(status) = self.git_status.get(&entry.path) {
            return status.clone();
        }
        if entry.kind == FileKind::Directory {
            return self
                .git_dir_status
                .get(&entry.path)
                .cloned()
                .unwrap_or_default();
        }
        String::new()
    }

    fn side_items(&self) -> Vec<SideItem> {
        let mut items = Vec::new();
        items.push(SideItem {
            label: ss("QUICK ACCESS"),
            path: ss(""),
            icon: ss(""),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: true,
            active: false,
        });
        for folder in &self.known_folders {
            items.push(SideItem {
                label: ss(&folder.name),
                path: ss(&folder.path),
                icon: ss(match folder.id.as_str() {
                    "home" => "home",
                    "downloads" => "download",
                    "pictures" => "image",
                    _ => "folder",
                }),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: false,
                active: same_path_string(&self.current_path, &folder.path),
            });
        }

        items.push(SideItem {
            label: ss("DRIVES"),
            path: ss(""),
            icon: ss(""),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: true,
            active: false,
        });
        for drive in &self.drives {
            items.push(SideItem {
                label: ss(if drive.name.is_empty() {
                    &drive.path
                } else {
                    &drive.name
                }),
                path: ss(&drive.path),
                icon: ss("drive"),
                count: ss(&drive.kind),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: false,
                active: self.current_path.starts_with(&drive.path),
            });
        }

        items.push(SideItem {
            label: ss("BOOKMARKS"),
            path: ss(""),
            icon: ss(""),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: true,
            active: false,
        });
        for bookmark in &self.bookmarks {
            items.push(SideItem {
                label: ss(&bookmark.name),
                path: ss(&bookmark.path),
                icon: ss("folder"),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: false,
                active: same_path_string(&self.current_path, &bookmark.path),
            });
        }

        items.push(SideItem {
            label: ss("SMART FOLDERS"),
            path: ss(""),
            icon: ss(""),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: true,
            active: false,
        });
        for smart in default_smart_folders(&self.current_path) {
            items.push(SideItem {
                label: ss(smart.name),
                path: ss(format!("smart:{}", smart.id)),
                icon: ss("search"),
                count: ss(""),
                color: color("#4f9cff"),
                is_header: false,
                active: self.search_query == format!("smart:{}", smart.id),
            });
        }

        if !self.recent_locations.is_empty() {
            items.push(SideItem {
                label: ss("RECENT"),
                path: ss(""),
                icon: ss(""),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: true,
                active: false,
            });
            for path in self.recent_locations.iter().take(5) {
                items.push(SideItem {
                    label: ss(Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.clone())),
                    path: ss(path),
                    icon: ss("folder"),
                    count: ss(""),
                    color: rgba_u8(0, 0, 0, 0.0),
                    is_header: false,
                    active: same_path_string(&self.current_path, path),
                });
            }
        }

        items.push(SideItem {
            label: ss("TAGS"),
            path: ss(""),
            icon: ss(""),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: true,
            active: false,
        });
        for (id, label) in [
            ("red", "Urgent"),
            ("orange", "Important"),
            ("yellow", "Review"),
            ("green", "Done"),
            ("blue", "Personal"),
            ("violet", "Code"),
        ] {
            let count = self.tags.values().filter(|tag| tag.as_str() == id).count();
            items.push(SideItem {
                label: ss(label),
                path: ss(format!("tag:{id}")),
                icon: ss("tag"),
                count: ss(count.to_string()),
                color: tag_color(id),
                is_header: false,
                active: self.search_query == format!("tag:{id}"),
            });
        }
        items
    }

    fn tab_items(&self) -> Vec<TabItem> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabItem {
                title: ss(Path::new(&tab.path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| tab.path.clone())),
                path: ss(&tab.path),
                active: index == self.active_tab,
            })
            .collect()
    }

    fn navigate(&mut self, ui: &MainWindow, path: String, push_history: bool) {
        let path = if path.trim().is_empty() {
            self.current_path.clone()
        } else {
            path
        };
        let is_accessible = Path::new(&path).is_dir();
        if !is_accessible && !path.is_empty() {
            ui.set_empty_state(ss(format!("Cannot open \"{}\"", path)));
            return;
        }
        match native_list_directory(&self.app_state, &path) {
            Ok(files) => {
                ui.set_nav_opacity(0.0);

                // Save view+sort for the folder we are leaving
                let prev_path = self.current_path.clone();
                if !prev_path.is_empty() {
                    let view_key = format!("{}|{}|{}", prev_path, ui.get_view_mode(), "");
                    let sort_key = format!("{}|sort_by|{}", prev_path, self.sort_by);
                    let sort_dir_key = format!("{}|sort_dir|{}", prev_path, self.sort_dir);
                    self.folder_views
                        .insert(format!("{prev_path}:view"), ui.get_view_mode().to_string());
                    self.folder_views
                        .insert(format!("{prev_path}:sort_by"), self.sort_by.clone());
                    self.folder_views
                        .insert(format!("{prev_path}:sort_dir"), self.sort_dir.clone());
                    let _ = (view_key, sort_key, sort_dir_key); // suppress unused warning
                }

                self.current_path = path.clone();
                // Clear thumbnails from the previous folder to free memory
                self.thumbnail_memory.retain(|k, _| k.starts_with(&path));
                self.recent_locations
                    .retain(|p| !same_path_string(p, &path));
                self.recent_locations.insert(0, path.clone());
                self.recent_locations.truncate(20);
                let _ = write_native_json("recent_locations.json", &self.recent_locations);

                // Restore view+sort for the new folder
                if let Some(view) = self.folder_views.get(&format!("{path}:view")).cloned() {
                    ui.set_view_mode(ss(&view));
                }
                if let Some(sb) = self.folder_views.get(&format!("{path}:sort_by")).cloned() {
                    self.sort_by = sb;
                }
                if let Some(sd) = self.folder_views.get(&format!("{path}:sort_dir")).cloned() {
                    self.sort_dir = sd;
                }

                self.files = files;
                self.search_query.clear();
                self.selected_index = -1;
                self.selected_set.clear();
                self.select_anchor = -1;
                self.files_model = None;
                // Fetch git status in the background to avoid blocking UI
                if is_inside_git_worktree(Path::new(&path)) {
                    let ready = self.git_status_ready.clone();
                    let pending = self.pending_git_status.clone();
                    let state = self.app_state.clone();
                    let p = path.clone();
                    std::thread::spawn(move || {
                        let status = native_git_status(&state, &p);
                        if let Ok(mut lock) = pending.lock() {
                            *lock = Some(status);
                        }
                        ready.store(true, Ordering::Release);
                    });
                } else {
                    self.git_status = Arc::new(GitStatusMap::new());
                    self.rebuild_git_dir_status();
                }
                if push_history {
                    self.history.truncate(self.history_index + 1);
                    self.history.push(path.clone());
                    self.history_index = self.history.len().saturating_sub(1);
                }
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.path = path.clone();
                    tab.view = ui.get_view_mode().to_string();
                }

                // Set empty state message
                ui.set_empty_state(ss(""));

                self.apply_filter();
                self.update_models(ui);
                self.update_preview(ui);
                self.save_session();

                // Background thumbnail generation for image files
                let image_entries: Vec<(String, u64)> = self
                    .visible_files
                    .iter()
                    .filter(|e| is_image_ext(e.extension.as_deref().unwrap_or("")))
                    .take(64)
                    .map(|e| (e.path.clone(), e.modified))
                    .collect();
                if !image_entries.is_empty() {
                    let ready_flag = self.thumbnail_ready.clone();
                    std::thread::spawn(move || {
                        for (path, mtime) in image_entries {
                            let pb = PathBuf::from(&path);
                            let ck = thumbnail_cache_key(&pb, mtime, 160);
                            let thumb = thumbnail_cache_dir().join(format!("{ck}.jpg"));
                            if thumb.exists() {
                                continue;
                            }
                            if let Ok(img) = image::open(&pb) {
                                if img.width() <= 8192 && img.height() <= 8192 {
                                    let t = img.thumbnail(160, 160);
                                    let mut buf = Vec::new();
                                    if t.write_to(
                                        &mut Cursor::new(&mut buf),
                                        image::ImageFormat::Jpeg,
                                    )
                                    .is_ok()
                                    {
                                        let _ = store_thumbnail_on_disk(
                                            &pb,
                                            mtime,
                                            160,
                                            &buf,
                                            THUMBNAIL_CACHE_LIMIT_BYTES,
                                        );
                                    }
                                }
                            }
                        }
                        ready_flag.store(true, Ordering::Release);
                    });
                }

                // Preload subdirectories into cache in the background
                let subdir_paths: Vec<String> = self
                    .visible_files
                    .iter()
                    .filter(|e| e.kind == FileKind::Directory)
                    .take(12)
                    .map(|e| e.path.clone())
                    .collect();
                if !subdir_paths.is_empty() {
                    let preload_state = self.app_state.clone();
                    std::thread::spawn(move || {
                        for dir_path in subdir_paths {
                            if preload_state.cached_directory(&dir_path).is_some() {
                                continue;
                            }
                            let pb = PathBuf::from(&dir_path);
                            if let Ok(entries) = list_directory_uncached(&pb) {
                                preload_state.store_directory(&dir_path, entries.clone());
                                let _ = index_directory_entries(&dir_path, &entries);
                            }
                        }
                    });
                }

                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::from_millis(40), move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_nav_opacity(1.0);
                    }
                });
            }
            Err(error) => {
                ui.set_empty_state(ss(format!("Cannot read folder: {error}")));
                self.show_toast(ui, error);
            }
        }
    }

    fn refresh(&mut self, ui: &MainWindow) {
        self.app_state
            .invalidate_directory_path(Path::new(&self.current_path));
        let path = self.current_path.clone();
        self.navigate(ui, path, false);
    }

    fn select(&mut self, ui: &MainWindow, index: i32) {
        self.select_with_modifiers(ui, index, false, false);
    }

    fn select_with_modifiers(&mut self, ui: &MainWindow, index: i32, ctrl: bool, shift: bool) {
        let n = self.visible_files.len();
        let mut changed: Vec<usize> = Vec::new();

        if shift && self.select_anchor >= 0 && index >= 0 {
            // Range select from anchor to index
            let lo = (self.select_anchor as usize).min(index as usize);
            let hi = (self.select_anchor as usize).max(index as usize);
            // Clear current non-anchor selection, keep anchor, add range
            let old = std::mem::take(&mut self.selected_set);
            for i in 0..n {
                if (i >= lo && i <= hi) || i == self.select_anchor as usize {
                    self.selected_set.insert(i);
                    if !old.contains(&i) {
                        changed.push(i);
                    }
                } else if old.contains(&i) {
                    changed.push(i);
                }
            }
        } else if ctrl && index >= 0 {
            // Toggle this item
            let i = index as usize;
            if self.selected_set.contains(&i) {
                self.selected_set.remove(&i);
            } else {
                self.selected_set.insert(i);
                self.select_anchor = index;
            }
            changed.push(i);
        } else {
            // Plain click: clear all, select one
            let old = std::mem::take(&mut self.selected_set);
            for i in old {
                changed.push(i);
            }
            if index >= 0 && (index as usize) < n {
                self.selected_set.insert(index as usize);
                changed.push(index as usize);
                self.select_anchor = index;
            }
        }

        self.selected_index = index;
        if let Some(entry) = self.selected_entry() {
            ui.set_selected_name(ss(&entry.name));
        } else {
            ui.set_selected_name(ss(""));
        }
        self.update_selection_in_model(ui, &changed);
        // Preview update is debounced at the callback level
    }

    fn select_all(&mut self, ui: &MainWindow) {
        let n = self.visible_files.len();
        if n == 0 {
            return;
        }
        self.selected_set = (0..n).collect();
        self.selected_index = 0;
        self.select_anchor = 0;
        if let Some(entry) = self.selected_entry() {
            ui.set_selected_name(ss(&entry.name));
        }
        let changed: Vec<usize> = (0..n).collect();
        self.update_selection_in_model(ui, &changed);
    }

    fn open_index(&mut self, ui: &MainWindow, index: i32) {
        if index < 0 {
            return;
        }
        let Some(entry) = self.visible_files.get(index as usize).cloned() else {
            return;
        };
        if entry.kind == FileKind::Directory {
            self.navigate(ui, entry.path, true);
        } else if let Err(error) = open_file(entry.path) {
            self.show_toast(ui, error);
        }
    }

    fn update_preview(&self, ui: &MainWindow) {
        if !ui.get_preview_visible() {
            return;
        }

        let Some(entry) = self.selected_entry() else {
            ui.set_preview_title(ss(""));
            ui.set_preview_body(ss(""));
            ui.set_preview_meta(ss(""));
            ui.set_preview_is_image(false);
            return;
        };

        ui.set_preview_title(ss(&entry.name));

        // Try to load image preview from thumbnail cache
        let ext = entry.extension.as_deref().unwrap_or("").to_lowercase();
        let is_image = is_image_ext(&ext);
        if is_image {
            let disk_key = thumbnail_cache_key(Path::new(&entry.path), entry.modified, 160);
            let thumb_path = thumbnail_cache_dir().join(format!("{disk_key}.jpg"));
            if let Ok(img) = image::open(&thumb_path).map(|i| i.into_rgba8()) {
                let (w, h) = img.dimensions();
                let raw = img.into_raw();
                let buf =
                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&raw, w, h);
                ui.set_preview_image(slint::Image::from_rgba8(buf));
                ui.set_preview_is_image(true);
            } else {
                ui.set_preview_is_image(false);
            }
        } else {
            ui.set_preview_is_image(false);
        }

        match native_read_preview(&self.app_state, &entry.path, Some(4 * 1024)) {
            Ok(preview) => {
                let body = match preview.kind.as_str() {
                    "image" => String::new(),
                    "text" | "svg" | "archive" | "pdf" | "font" | "media" => {
                        preview.text.unwrap_or_default()
                    }
                    "folder" => String::new(),
                    other => format!("{other} file"),
                };
                let truncated_note = if preview.truncated {
                    " · truncated"
                } else {
                    ""
                };
                let meta = format!(
                    "Path:     {}\nType:     {}\nSize:     {}\nModified: {}{}",
                    entry.path,
                    entry_type(&entry),
                    format_size_short(entry.size),
                    format_modified(entry.modified),
                    truncated_note,
                );
                ui.set_preview_body(ss(body));
                ui.set_preview_meta(ss(meta));
            }
            Err(error) => {
                ui.set_preview_body(ss("Preview unavailable"));
                ui.set_preview_meta(ss(error));
            }
        }
    }

    fn side_activated(&mut self, ui: &MainWindow, index: i32) {
        let items = self.side_items();
        if let Some(item) = items.get(index as usize) {
            let path = item.path.to_string();
            if let Some(tag) = path.strip_prefix("tag:") {
                self.search_query = format!("tag:{tag}");
                self.apply_filter();
                self.selected_index = -1;
                self.update_models(ui);
            } else if let Some(smart) = path.strip_prefix("smart:") {
                if smart == "old-downloads" {
                    if let Some(downloads) = dirs::download_dir() {
                        let target = downloads.to_string_lossy().to_string();
                        if !same_path_string(&self.current_path, &target) {
                            self.navigate(ui, target, true);
                        }
                    }
                }
                self.search_query = format!("smart:{smart}");
                self.apply_filter();
                self.selected_index = -1;
                self.update_models(ui);
            } else if !path.is_empty() {
                self.navigate(ui, path, true);
            }
        }
    }

    fn go_up(&mut self, ui: &MainWindow) {
        if let Some(parent) = Path::new(&self.current_path).parent() {
            self.navigate(ui, parent.to_string_lossy().to_string(), true);
        }
    }

    fn go_back(&mut self, ui: &MainWindow) {
        if self.history_index > 0 {
            self.history_index -= 1;
            if let Some(path) = self.history.get(self.history_index).cloned() {
                self.navigate(ui, path, false);
            }
        }
    }

    fn go_forward(&mut self, ui: &MainWindow) {
        if self.history_index + 1 < self.history.len() {
            self.history_index += 1;
            if let Some(path) = self.history.get(self.history_index).cloned() {
                self.navigate(ui, path, false);
            }
        }
    }

    fn set_view(&mut self, ui: &MainWindow, mode: &str) {
        ui.set_view_mode(ss(mode));
        let path = self.current_path.clone();
        self.folder_views.insert(path.clone(), mode.to_string());
        self.folder_views
            .insert(format!("{path}:view"), mode.to_string());
        let _ = write_native_json("folder_views.json", &self.folder_views);
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.view = mode.to_string();
        }
        self.save_session();
        // If switching to grid/gallery, trigger thumbnail loading
        if mode != "list" {
            self.update_models(ui);
        }
    }

    fn set_preview_visible(&self, ui: &MainWindow, visible: bool) {
        ui.set_preview_visible(visible);
        if visible {
            self.update_preview(ui);
        }
    }

    fn search(&mut self, ui: &MainWindow, query: String) {
        self.search_query = query;
        self.selected_index = -1;
        self.selected_set.clear();
        self.select_anchor = -1;
        self.files_model = None;
        let indexed = if self.search_query.trim().starts_with("tag:") {
            Vec::new()
        } else {
            index_search(&self.current_path, &self.search_query, 500).unwrap_or_default()
        };
        if indexed.is_empty() {
            self.apply_filter();
        } else {
            self.visible_files = indexed;
        }
        self.update_models(ui);
    }

    fn new_tab(&mut self, ui: &MainWindow) {
        self.tabs.push(SessionTab {
            path: self.current_path.clone(),
            view: ui.get_view_mode().to_string(),
            sort_by: "name".to_string(),
            sort_dir: "asc".to_string(),
        });
        self.active_tab = self.tabs.len() - 1;
        self.update_models(ui);
        self.save_session();
    }

    fn close_tab(&mut self, ui: &MainWindow, index: i32) {
        if self.tabs.len() <= 1 {
            return;
        }
        let idx = if index < 0 {
            self.active_tab
        } else {
            index as usize
        };
        if idx < self.tabs.len() {
            self.tabs.remove(idx);
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
            if let Some(tab) = self.tabs.get(self.active_tab).cloned() {
                self.navigate(ui, tab.path, false);
            }
        }
    }

    fn activate_tab(&mut self, ui: &MainWindow, index: i32) {
        let idx = index as usize;
        if let Some(tab) = self.tabs.get(idx).cloned() {
            self.active_tab = idx;
            ui.set_view_mode(ss(&tab.view));
            self.navigate(ui, tab.path, false);
        }
    }

    fn sort_column(&mut self, ui: &MainWindow, col: &str) {
        if self.sort_by == col {
            self.sort_dir = if self.sort_dir == "asc" {
                "desc".to_string()
            } else {
                "asc".to_string()
            };
        } else {
            self.sort_by = col.to_string();
            self.sort_dir = "asc".to_string();
        }
        let path = self.current_path.clone();
        self.folder_views
            .insert(format!("{path}:sort_by"), self.sort_by.clone());
        self.folder_views
            .insert(format!("{path}:sort_dir"), self.sort_dir.clone());
        self.apply_filter();
        self.update_models(ui);
    }

    fn command(&mut self, ui: &MainWindow, command: &str) {
        match command {
            "new-tab" => self.new_tab(ui),
            "close-tab" => self.close_tab(ui, -1),
            "refresh" => self.refresh(ui),
            "settings" => ui.set_settings_visible(true),
            "command-palette" => ui.set_command_visible(true),
            "view-grid" => self.set_view(ui, "grid"),
            "view-list" => self.set_view(ui, "list"),
            "view-gallery" => self.set_view(ui, "gallery"),
            "toggle-preview" => self.set_preview_visible(ui, !ui.get_preview_visible()),
            "toggle-dual" => ui.set_dual_pane(!ui.get_dual_pane()),
            "open" => self.open_index(ui, self.selected_index),
            "rename" => self.prompt_rename(ui),
            "delete" => self.prompt_delete(ui),
            "new-folder" => self.prompt_new_folder(ui),
            "copy" => self.copy_selected(false, ui),
            "cut" => self.copy_selected(true, ui),
            "paste" => self.paste_async(ui),
            "select-all" => self.select_all(ui),
            "batch-rename" => self.prompt_batch_rename(ui),
            "checksum" => self.show_checksum(ui),
            "note" => self.prompt_note(ui),
            "storage" => self.show_storage(ui),
            "open-terminal" => {
                let path = self
                    .selected_entry()
                    .map(|e| {
                        if e.kind == FileKind::Directory {
                            e.path
                        } else {
                            self.current_path.clone()
                        }
                    })
                    .unwrap_or_else(|| self.current_path.clone());
                if let Err(e) = open_terminal(path) {
                    self.show_toast_kind(ui, e, "error");
                }
            }
            "duplicates" => self.show_duplicates(ui),
            "operation-log" => self.show_operation_log(ui),
            "operation-queue" => self.show_operation_queue(ui),
            "queue-pause" => {
                if let Ok(mut paused) = self.app_state.queue_paused.lock() {
                    *paused = true;
                }
                self.show_toast(ui, "Queue paused for new operations.");
            }
            "queue-resume" => {
                if let Ok(mut paused) = self.app_state.queue_paused.lock() {
                    *paused = false;
                }
                self.show_toast(ui, "Queue resumed.");
            }
            "queue-cancel" => {
                if let Ok(mut queue) = self.app_state.operation_queue.lock() {
                    for item in queue.iter_mut().filter(|i| i.status == "running") {
                        item.status = "cancelled".to_string();
                        item.detail = "Cancelled by user".to_string();
                        item.finished_at = Some(now_unix_secs());
                    }
                }
                self.show_toast(ui, "Running operations marked for cancellation.");
            }
            "locked-file" => self.show_locked_file(ui),
            "properties" => {
                if let Some(entry) = self.selected_entry() {
                    match open_windows_properties(&entry.path) {
                        Ok(()) => self.show_toast(ui, "Opening Windows Properties"),
                        Err(error) => self.show_toast(ui, error),
                    }
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "show-more-options" => {
                if let Some(entry) = self.selected_entry() {
                    match open_more_options(&entry.path) {
                        Ok(()) => self.show_toast(ui, "Opened in Explorer for shell options"),
                        Err(error) => self.show_toast(ui, error),
                    }
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "cloud-state" => {
                if let Some(entry) = self.selected_entry() {
                    ui.set_preview_title(ss("Cloud State"));
                    ui.set_preview_body(ss(cloud_state_label(&entry.path)));
                    ui.set_preview_meta(ss(entry.path));
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "new-template" => self.show_templates(ui),
            "rename-presets" => self.show_rename_presets(ui),
            "image-tools" => self.show_image_tools(ui),
            "archive-browser" => self.show_archive_browser(ui),
            "compare-folder" => self.prompt_compare_folder(ui),
            "rules" => self.show_rules(ui),
            "smart-folders" => self.show_smart_folders(ui),
            "home-page" => self.show_home_page(ui),
            "libraries" => self.show_libraries(ui),
            "recent-locations" => self.show_recent_locations(ui),
            "copy-as-path" => {
                if let Some(entry) = self.selected_entry() {
                    match copy_text_to_clipboard(&entry.path) {
                        Ok(()) => self.show_toast(ui, "Path copied"),
                        Err(error) => self.show_toast(ui, error),
                    }
                }
            }
            "copy-as-powershell" => {
                if let Some(entry) = self.selected_entry() {
                    let text = format!("'{}'", entry.path.replace('\'', "''"));
                    match copy_text_to_clipboard(&text) {
                        Ok(()) => self.show_toast(ui, "PowerShell path copied"),
                        Err(error) => self.show_toast(ui, error),
                    }
                }
            }
            "copy-as-uri" => {
                if let Some(entry) = self.selected_entry() {
                    let uri = format!(
                        "file:///{}",
                        entry.path.replace('\\', "/").replace(' ', "%20")
                    );
                    match copy_text_to_clipboard(&uri) {
                        Ok(()) => self.show_toast(ui, "URI copied"),
                        Err(error) => self.show_toast(ui, error),
                    }
                }
            }
            "breadcrumb-siblings" => self.show_breadcrumb_siblings(ui),
            "performance-debug" => self.show_performance_debug(ui),
            "clear-thumbnail-cache" => match clear_thumbnail_cache() {
                Ok(bytes) => {
                    self.sync_performance_status(ui);
                    self.show_toast(ui, format!("Cleared {}", format_size_short(bytes)));
                }
                Err(error) => self.show_toast(ui, error),
            },
            "rebuild-index" => {
                let roots = index_roots_for_mode(&self.settings);
                if roots.is_empty() {
                    self.show_toast(ui, "Low mode indexes folders as you open them.");
                } else {
                    schedule_index_roots(roots);
                    self.show_toast(ui, "Index rebuild started in the background.");
                }
            }
            "performance-settings" => {
                ui.set_settings_tab(ss("performance"));
                ui.set_settings_visible(true);
            }
            "shortcut-editor" => self.show_shortcuts(ui),
            "undo" => self.undo(ui),
            "focus-search" => self.show_toast(ui, "Search is ready in the toolbar."),
            _ => self.show_toast(ui, format!("Command not implemented: {command}")),
        }
    }

    fn prompt_rename(&mut self, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            self.show_toast(ui, "Select a file first.");
            return;
        };
        self.pending_prompt = Some(PendingPrompt::Rename(entry.path));
        ui.set_prompt_title(ss("Rename"));
        ui.set_prompt_value(ss(entry.name));
        ui.set_prompt_visible(true);
    }

    fn prompt_new_folder(&mut self, ui: &MainWindow) {
        self.pending_prompt = Some(PendingPrompt::NewFolder);
        ui.set_prompt_title(ss("New folder"));
        ui.set_prompt_value(ss("New Folder"));
        ui.set_prompt_visible(true);
    }

    fn prompt_note(&mut self, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            self.show_toast(ui, "Select a file first.");
            return;
        };
        self.pending_prompt = Some(PendingPrompt::Note(entry.path.clone()));
        ui.set_prompt_title(ss("File note"));
        ui.set_prompt_value(ss(self.notes.get(&entry.path).cloned().unwrap_or_default()));
        ui.set_prompt_visible(true);
    }

    fn prompt_batch_rename(&mut self, ui: &MainWindow) {
        let paths = self.selected_paths();
        if paths.len() < 2 {
            self.show_toast(ui, "Select at least two items for batch rename.");
            return;
        }
        let preview = paths
            .iter()
            .take(8)
            .enumerate()
            .map(|(i, path)| {
                let original = Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                format!("{original} -> Renamed {:03}", i + 1)
            })
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("Batch Rename"));
        ui.set_preview_body(ss(format!(
            "{preview}\n\nEnter a base name. Extensions are preserved."
        )));
        ui.set_preview_meta(ss(format!("{} selected", paths.len())));
        self.pending_prompt = Some(PendingPrompt::BatchRename(paths));
        ui.set_prompt_title(ss("Batch rename base name"));
        ui.set_prompt_value(ss("Renamed"));
        ui.set_prompt_visible(true);
    }

    fn prompt_delete(&mut self, ui: &MainWindow) {
        let n = self.selected_set.len();
        if n > 1 {
            ui.set_confirm_text(ss(format!("Send {n} items to the Recycle Bin?")));
            ui.set_confirm_visible(true);
        } else if let Some(entry) = self.selected_entry() {
            ui.set_confirm_text(ss(format!("Send '{}' to the Recycle Bin?", entry.name)));
            ui.set_confirm_visible(true);
        } else {
            self.show_toast(ui, "Select a file first.");
        }
    }

    fn accept_prompt(&mut self, ui: &MainWindow, value: String) {
        match self.pending_prompt.take() {
            Some(PendingPrompt::Rename(path)) => {
                match native_rename(&self.app_state, &path, &value) {
                    Ok(_) => {
                        self.refresh(ui);
                        self.show_toast_kind(ui, "Renamed", "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::NewFolder) => {
                let path = PathBuf::from(&self.current_path).join(value.trim());
                match native_create_directory(&self.app_state, &path.to_string_lossy()) {
                    Ok(()) => {
                        self.refresh(ui);
                        self.show_toast_kind(ui, "Folder created", "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::Note(path)) => {
                if value.trim().is_empty() {
                    self.notes.remove(&path);
                } else {
                    self.notes.insert(path, value.trim().to_string());
                }
                let _ = write_native_json("notes.json", &self.notes);
                self.update_models(ui);
                self.show_toast_kind(ui, "Note saved", "success");
            }
            Some(PendingPrompt::NewTemplate(template)) => {
                let base = value.trim();
                if base.is_empty() {
                    self.show_toast(ui, "Name cannot be empty.");
                    return;
                }
                let mut path = PathBuf::from(&self.current_path).join(base);
                if path.extension().is_none() {
                    path.set_extension(&template.extension);
                }
                if path.exists() {
                    path = keep_both_destination(&path);
                }
                match File::create(&path).and_then(|mut f| f.write_all(template.content.as_bytes()))
                {
                    Ok(()) => {
                        self.refresh(ui);
                        self.show_toast_kind(ui, "Template file created", "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error.to_string(), "error"),
                }
            }
            Some(PendingPrompt::BatchRename(paths)) => {
                let base = value.trim();
                if base.is_empty() {
                    self.show_toast(ui, "Name cannot be empty.");
                    return;
                }
                let width = paths.len().max(1).to_string().len().max(3);
                let mut ops = Vec::with_capacity(paths.len());
                for (index, from) in paths.iter().enumerate() {
                    let src = Path::new(from);
                    let Some(parent) = src.parent() else {
                        continue;
                    };
                    let ext = src.extension().map(|e| e.to_string_lossy().to_string());
                    let file_name = if let Some(ext) = ext {
                        format!("{base} {:0width$}.{ext}", index + 1)
                    } else {
                        format!("{base} {:0width$}", index + 1)
                    };
                    let to = parent.join(file_name);
                    if to.exists() {
                        self.show_toast_kind(
                            ui,
                            format!("'{}' already exists", to.display()),
                            "error",
                        );
                        return;
                    }
                    ops.push((from.clone(), to));
                }

                for (from, to) in &ops {
                    let src = Path::new(from);
                    if let Err(error) = fs::rename(src, to) {
                        self.show_toast_kind(ui, format!("{from}: {error}"), "error");
                        self.refresh(ui);
                        return;
                    }
                    self.app_state.invalidate_path(src);
                    self.app_state.invalidate_path(to);
                    self.app_state
                        .log_op("rename", from, Some(&to.to_string_lossy()));
                }
                self.refresh(ui);
                self.show_toast_kind(ui, format!("Renamed {} items", ops.len()), "success");
            }
            Some(PendingPrompt::CompareFolder(left)) => {
                let right = value.trim();
                if right.is_empty() {
                    self.show_toast(ui, "Pick a folder path to compare.");
                    return;
                }
                match compare_folders(Path::new(&left), Path::new(right), 500) {
                    Ok(rows) => {
                        let body = rows
                            .iter()
                            .filter(|row| row.status != "same")
                            .take(200)
                            .map(|row| {
                                format!(
                                    "{} | {} | L {} / R {}",
                                    row.status,
                                    row.path,
                                    format_size_short(row.left_size),
                                    format_size_short(row.right_size)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.set_preview_title(ss("Folder Compare"));
                        ui.set_preview_body(ss(if body.is_empty() {
                            "Folders match in the scanned range.".to_string()
                        } else {
                            body
                        }));
                        ui.set_preview_meta(ss(format!("{} rows scanned", rows.len())));
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::ConflictPaste { src, dest, cut }) => {
                let action = value.trim().to_lowercase();
                let mut dest_path = PathBuf::from(&dest);
                if action == "skip" {
                    self.show_toast(ui, "Skipped");
                    return;
                }
                if action == "replace" && dest_path.exists() {
                    if let Err(error) = native_delete_path(&dest) {
                        self.show_toast_kind(ui, error, "error");
                        return;
                    }
                } else if action == "keep" || action.is_empty() {
                    dest_path = keep_both_destination(&dest_path);
                } else {
                    self.show_toast(ui, "Type keep, replace, or skip.");
                    return;
                }
                let result = if cut {
                    native_move(&self.app_state, &src, &dest_path.to_string_lossy())
                } else {
                    native_copy(&self.app_state, &src, &dest_path.to_string_lossy())
                };
                match result {
                    Ok(()) => {
                        self.refresh(ui);
                        self.show_toast_kind(ui, "Conflict resolved", "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::Archive) | None => {}
        }
    }

    fn confirm_delete(&mut self, ui: &MainWindow) {
        let paths: Vec<String> = if self.selected_set.len() > 1 {
            let mut sorted: Vec<usize> = self.selected_set.iter().cloned().collect();
            sorted.sort_unstable();
            sorted
                .into_iter()
                .filter_map(|i| self.visible_files.get(i))
                .map(|e| e.path.clone())
                .collect()
        } else {
            self.selected_entry()
                .map(|e| vec![e.path])
                .unwrap_or_default()
        };
        if paths.is_empty() {
            return;
        }
        let n = paths.len();
        let mut errors = 0usize;
        for path in &paths {
            if native_delete(&self.app_state, path).is_err() {
                errors += 1;
            }
        }
        self.refresh(ui);
        if errors == 0 {
            let msg = if n == 1 {
                "Moved to Recycle Bin".to_string()
            } else {
                format!("{n} items moved to Recycle Bin")
            };
            self.show_toast_kind(ui, msg, "success");
        } else {
            self.show_toast_kind(ui, format!("{errors} of {n} failed to delete"), "error");
        }
    }

    fn copy_selected(&mut self, cut: bool, ui: &MainWindow) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            self.show_toast(ui, "Select a file first.");
            return;
        }
        let n = paths.len();
        self.clipboard = Some(NativeClipboard { paths, cut });
        let msg = if n == 1 {
            if cut {
                "Cut to clipboard".to_string()
            } else {
                "Copied to clipboard".to_string()
            }
        } else {
            if cut {
                format!("{n} items cut")
            } else {
                format!("{n} items copied")
            }
        };
        self.show_toast(ui, msg);
    }

    fn paste(&mut self, ui: &MainWindow) {
        let Some(clipboard) = self.clipboard.clone() else {
            self.show_toast(ui, "Clipboard is empty.");
            return;
        };
        let n = clipboard.paths.len();
        if n > 1 {
            let verb = if clipboard.cut { "Moving" } else { "Copying" };
            ui.set_op_drawer_text(ss(format!("{verb} {n} items…")));
            ui.set_op_drawer_visible(true);
        }
        let mut pasted = 0usize;
        for src in &clipboard.paths {
            let Some(name) = Path::new(src).file_name() else {
                continue;
            };
            let dest = PathBuf::from(&self.current_path).join(name);
            if dest.exists() {
                ui.set_op_drawer_visible(false);
                let conflict = conflict_info(Path::new(src), &dest);
                ui.set_preview_title(ss("Copy Conflict"));
                ui.set_preview_body(ss(format!(
                    "Incoming: {}\nSize: {}\nModified: {}\nSHA-256: {}\n\nExisting: {}\nSize: {}\nModified: {}\nSHA-256: {}\n\nType keep, replace, or skip.",
                    conflict.incoming_path,
                    format_size_short(conflict.incoming_size),
                    format_modified(conflict.incoming_modified),
                    conflict.incoming_sha256.clone().unwrap_or_else(|| "not calculated".to_string()),
                    conflict.existing_path,
                    format_size_short(conflict.existing_size),
                    format_modified(conflict.existing_modified),
                    conflict.existing_sha256.clone().unwrap_or_else(|| "not calculated".to_string())
                )));
                ui.set_preview_meta(ss("Conflict resolver"));
                self.pending_prompt = Some(PendingPrompt::ConflictPaste {
                    src: src.clone(),
                    dest: dest.to_string_lossy().to_string(),
                    cut: clipboard.cut,
                });
                ui.set_prompt_title(ss("Conflict action"));
                ui.set_prompt_value(ss("keep"));
                ui.set_prompt_visible(true);
                return;
            }
            let result = if clipboard.cut {
                native_move(&self.app_state, src, &dest.to_string_lossy())
            } else {
                native_copy(&self.app_state, src, &dest.to_string_lossy())
            };
            if let Err(error) = result {
                ui.set_op_drawer_visible(false);
                self.show_toast_kind(ui, error, "error");
                return;
            }
            pasted += 1;
        }
        ui.set_op_drawer_visible(false);
        if clipboard.cut {
            self.clipboard = None;
        }
        self.refresh(ui);
        let verb = if clipboard.cut { "Moved" } else { "Pasted" };
        let msg = if pasted == 1 {
            format!("{verb} 1 item")
        } else {
            format!("{verb} {pasted} items")
        };
        self.show_toast_kind(ui, msg, "success");
    }

    fn paste_async(&mut self, ui: &MainWindow) {
        let Some(clipboard) = self.clipboard.clone() else {
            self.show_toast(ui, "Clipboard is empty.");
            return;
        };

        let dest_dir = self.current_path.clone();
        for src in &clipboard.paths {
            let Some(name) = Path::new(src).file_name() else {
                continue;
            };
            let dest = PathBuf::from(&dest_dir).join(name);
            if dest.exists() {
                let conflict = conflict_info(Path::new(src), &dest);
                ui.set_preview_title(ss("Copy Conflict"));
                ui.set_preview_body(ss(format!(
                    "Incoming: {}\nSize: {}\nModified: {}\nSHA-256: {}\n\nExisting: {}\nSize: {}\nModified: {}\nSHA-256: {}\n\nType keep, replace, or skip.",
                    conflict.incoming_path,
                    format_size_short(conflict.incoming_size),
                    format_modified(conflict.incoming_modified),
                    conflict
                        .incoming_sha256
                        .clone()
                        .unwrap_or_else(|| "not calculated".to_string()),
                    conflict.existing_path,
                    format_size_short(conflict.existing_size),
                    format_modified(conflict.existing_modified),
                    conflict
                        .existing_sha256
                        .clone()
                        .unwrap_or_else(|| "not calculated".to_string())
                )));
                ui.set_preview_meta(ss("Conflict resolver"));
                self.pending_prompt = Some(PendingPrompt::ConflictPaste {
                    src: src.clone(),
                    dest: dest.to_string_lossy().to_string(),
                    cut: clipboard.cut,
                });
                ui.set_prompt_title(ss("Conflict action"));
                ui.set_prompt_value(ss("keep"));
                ui.set_prompt_visible(true);
                return;
            }
        }

        let n = clipboard.paths.len();
        if n == 0 {
            self.show_toast(ui, "Clipboard is empty.");
            return;
        }

        let verb = if clipboard.cut { "Moving" } else { "Copying" };
        ui.set_op_drawer_text(ss(format!(
            "{verb} {n} item{}",
            if n == 1 { "" } else { "s" }
        )));
        ui.set_op_drawer_visible(true);

        let app_state = self.app_state.clone();
        let operation_ready = self.operation_ready.clone();
        let pending_result = self.pending_operation_result.clone();
        let paths = clipboard.paths;
        let cut = clipboard.cut;
        std::thread::spawn(move || {
            let mut completed = 0usize;
            let mut first_error: Option<String> = None;

            for src in &paths {
                let Some(name) = Path::new(src).file_name() else {
                    continue;
                };
                let dest = PathBuf::from(&dest_dir).join(name);
                let dest_string = dest.to_string_lossy().to_string();
                let result = if cut {
                    native_move(&app_state, src, &dest_string)
                } else {
                    native_copy(&app_state, src, &dest_string)
                };

                match result {
                    Ok(()) => completed += 1,
                    Err(error) => {
                        first_error = Some(error);
                        break;
                    }
                }
            }

            let result = if let Some(error) = first_error {
                NativeOperationResult {
                    message: error,
                    kind: "error".to_string(),
                    refresh: completed > 0,
                    clear_clipboard: false,
                }
            } else {
                let verb_done = if cut { "Moved" } else { "Pasted" };
                NativeOperationResult {
                    message: format!(
                        "{verb_done} {completed} item{}",
                        if completed == 1 { "" } else { "s" }
                    ),
                    kind: "success".to_string(),
                    refresh: true,
                    clear_clipboard: cut,
                }
            };

            if let Ok(mut lock) = pending_result.lock() {
                *lock = Some(result);
            }
            operation_ready.store(true, Ordering::Release);
        });
    }

    fn show_checksum(&mut self, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            self.show_toast(ui, "Select a file first.");
            return;
        };
        match get_checksum(entry.path.clone()) {
            Ok(map) => {
                ui.set_preview_title(ss(format!("Checksum - {}", entry.name)));
                ui.set_preview_body(ss(map.get("sha256").cloned().unwrap_or_default()));
                ui.set_preview_meta(ss("SHA-256"));
            }
            Err(error) => self.show_toast(ui, error),
        }
    }

    fn show_storage(&mut self, ui: &MainWindow) {
        match build_storage_tree(Path::new(&self.current_path), 4) {
            tree if tree.size > 0 => {
                ui.set_preview_title(ss("Storage Treemap"));
                let mut children = tree.children;
                children.sort_by_key(|child| std::cmp::Reverse(child.size));
                let lines = children
                    .into_iter()
                    .take(18)
                    .map(|child| format!("{}  {}", format_size_short(child.size), child.path))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.set_preview_body(ss(lines));
                ui.set_preview_meta(ss(format!("Total: {}", format_size_short(tree.size))));
            }
            _ => self.show_toast(ui, "Storage information is unavailable."),
        }
    }

    fn show_duplicates(&mut self, ui: &MainWindow) {
        match find_duplicates(self.current_path.clone(), Some(1024)) {
            Ok(groups) => {
                ui.set_preview_title(ss("Duplicate Finder"));
                let body = if groups.is_empty() {
                    "No duplicate files found.".to_string()
                } else {
                    groups
                        .iter()
                        .take(12)
                        .map(|group| {
                            format!(
                                "{} duplicates | {} each | {}",
                                group.len(),
                                format_size_short(group.first().map(|f| f.size).unwrap_or(0)),
                                group.first().map(|f| f.name.clone()).unwrap_or_default()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                ui.set_preview_body(ss(body));
                ui.set_preview_meta(ss(format!("{} duplicate groups", groups.len())));
            }
            Err(error) => self.show_toast(ui, error),
        }
    }

    fn show_operation_log(&mut self, ui: &MainWindow) {
        let log = self
            .app_state
            .operation_log
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default();
        let body = log
            .iter()
            .rev()
            .map(|op| {
                format!(
                    "{} | {}{}",
                    op.kind,
                    op.from,
                    op.to
                        .as_ref()
                        .map(|to| format!(" -> {to}"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("Operation Log"));
        ui.set_preview_body(ss(if body.is_empty() {
            "No operations recorded yet.".to_string()
        } else {
            body
        }));
        ui.set_preview_meta(ss(""));
    }

    fn show_operation_queue(&mut self, ui: &MainWindow) {
        let queue = self
            .app_state
            .operation_queue
            .lock()
            .map(|q| q.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let body = if queue.is_empty() {
            "No queued file operations yet.".to_string()
        } else {
            queue
                .iter()
                .rev()
                .map(|item| {
                    let conflict = item.conflict.as_ref().map(|c| {
                        format!(
                            "\n  Conflict: incoming {} modified {}, existing {} modified {}",
                            format_size_short(c.incoming_size),
                            format_modified(c.incoming_modified),
                            format_size_short(c.existing_size),
                            format_modified(c.existing_modified)
                        )
                    });
                    format!(
                        "#{id} {kind} [{status}] {done}/{total} at {speed}/s\n  {src}{dst}\n  {detail}{conflict}",
                        id = item.id,
                        kind = item.kind,
                        status = item.status,
                        done = format_size_short(item.bytes_done),
                        total = format_size_short(item.bytes_total),
                        speed = format_size_short(item.speed_bps),
                        src = item.source,
                        dst = item
                            .destination
                            .as_ref()
                            .map(|d| format!(" -> {d}"))
                            .unwrap_or_default(),
                        detail = item.detail,
                        conflict = conflict.unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        ui.set_preview_title(ss("Operation Queue"));
        ui.set_preview_body(ss(body));
        ui.set_preview_meta(ss("Pause, cancel, and retry controls are exposed through the command palette for queued work."));
    }

    fn show_locked_file(&mut self, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            self.show_toast(ui, "Select a file first.");
            return;
        };
        match locked_file_processes(&entry.path) {
            Ok(processes) if processes.is_empty() => {
                ui.set_preview_title(ss("Locked File Inspector"));
                ui.set_preview_body(ss(
                    "No locking processes were reported by Windows Restart Manager.",
                ));
                ui.set_preview_meta(ss(entry.path));
            }
            Ok(processes) => {
                let body = processes
                    .iter()
                    .map(|p| format!("{}  PID {}  {}", p.name, p.pid, p.reason))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.set_preview_title(ss("Locked File Inspector"));
                ui.set_preview_body(ss(body));
                ui.set_preview_meta(ss(entry.path));
            }
            Err(error) => self.show_toast(ui, error),
        }
    }

    fn show_home_page(&mut self, ui: &MainWindow) {
        let drives = self
            .drives
            .iter()
            .map(|d| format!("Drive {}  {}", d.name, d.path))
            .collect::<Vec<_>>()
            .join("\n");
        let saved = read_native_json::<Vec<SavedSearch>>("searches.json", Vec::new())
            .into_iter()
            .take(6)
            .map(|s| format!("Saved search: {}  {}", s.name, s.query))
            .collect::<Vec<_>>()
            .join("\n");
        let recent = self
            .recent_locations
            .iter()
            .take(8)
            .map(|p| format!("Recent: {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("Home"));
        ui.set_preview_body(ss(format!("{drives}\n\n{saved}\n\n{recent}")));
        ui.set_preview_meta(ss(
            "Quick Access, drives, saved searches, recent locations, and storage warnings.",
        ));
    }

    fn show_libraries(&mut self, ui: &MainWindow) {
        let mut lines = Vec::new();
        let libraries = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Microsoft")
            .join("Windows")
            .join("Libraries");
        if let Ok(entries) = fs::read_dir(&libraries) {
            for entry in entries.flatten() {
                if entry
                    .path()
                    .extension()
                    .map(|e| e == "library-ms")
                    .unwrap_or(false)
                {
                    lines.push(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
        if lines.is_empty() {
            lines.push("Windows library definitions were not found. Standard Documents, Pictures, Music, and Videos locations remain available in Quick Access.".to_string());
        }
        ui.set_preview_title(ss("Libraries"));
        ui.set_preview_body(ss(lines.join("\n")));
        ui.set_preview_meta(ss(libraries.to_string_lossy().to_string()));
    }

    fn show_smart_folders(&mut self, ui: &MainWindow) {
        let body = default_smart_folders(&self.current_path)
            .into_iter()
            .map(|s| format!("{} | {} | {}", s.name, s.query, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("Smart Folders"));
        ui.set_preview_body(ss(body));
        ui.set_preview_meta(ss("Use the Smart Folders section in the sidebar."));
    }

    fn show_recent_locations(&mut self, ui: &MainWindow) {
        ui.set_preview_title(ss("Recent Locations"));
        ui.set_preview_body(ss(if self.recent_locations.is_empty() {
            "No recent locations recorded yet.".to_string()
        } else {
            self.recent_locations.join("\n")
        }));
        ui.set_preview_meta(ss(""));
    }

    fn show_breadcrumb_siblings(&mut self, ui: &MainWindow) {
        let parent = Path::new(&self.current_path)
            .parent()
            .map(Path::to_path_buf);
        let body = parent
            .as_ref()
            .and_then(|p| fs::read_dir(p).ok())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .take(80)
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| "No sibling folders available.".to_string());
        ui.set_preview_title(ss("Breadcrumb Siblings"));
        ui.set_preview_body(ss(body));
        ui.set_preview_meta(ss(parent
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()));
    }

    fn show_templates(&mut self, ui: &MainWindow) {
        let templates = default_file_templates();
        let body = templates
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}: {} .{}", i + 1, t.name, t.extension))
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("New From Template"));
        ui.set_preview_body(ss(body));
        ui.set_preview_meta(ss(
            "Runs the first template from the command palette, or use the prompt to name the file.",
        ));
        if let Some(template) = templates.into_iter().next() {
            self.pending_prompt = Some(PendingPrompt::NewTemplate(template));
            ui.set_prompt_title(ss("New file from template"));
            ui.set_prompt_value(ss("New Note"));
            ui.set_prompt_visible(true);
        }
    }

    fn show_rename_presets(&mut self, ui: &MainWindow) {
        let presets: Vec<String> = read_native_json(
            "rename_presets.json",
            vec![
                "lowercase extensions".to_string(),
                "replace spaces with dashes".to_string(),
                "prefix date".to_string(),
                "number sequence".to_string(),
            ],
        );
        ui.set_preview_title(ss("Power Rename Presets"));
        ui.set_preview_body(ss(presets.join("\n")));
        ui.set_preview_meta(ss(
            "Batch rename keeps preview and undo through the operation log.",
        ));
    }

    fn show_image_tools(&mut self, ui: &MainWindow) {
        ui.set_preview_title(ss("Image Tools"));
        ui.set_preview_body(ss("Available actions: resize, convert to JPEG/PNG/WebP, rotate, compress, strip metadata. Actions run on demand from selected image files and keep the original unless replace is chosen."));
        ui.set_preview_meta(ss(
            "Lightweight panel added. Processing uses the image crate.",
        ));
    }

    fn show_archive_browser(&mut self, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            self.show_toast(ui, "Select an archive first.");
            return;
        };
        let ext = entry.extension.clone().unwrap_or_default();
        if !is_archive_ext(&ext) {
            self.show_toast(ui, "Select an archive first.");
            return;
        }
        match archive_listing_preview(Path::new(&entry.path), 240) {
            Ok(body) => {
                ui.set_preview_title(ss(format!("Archive Browser - {}", entry.name)));
                ui.set_preview_body(ss(body));
                ui.set_preview_meta(ss(
                    "ZIP files list inline. Other archives open through shell or extract tools.",
                ));
            }
            Err(error) => self.show_toast(ui, error),
        }
    }

    fn prompt_compare_folder(&mut self, ui: &MainWindow) {
        self.pending_prompt = Some(PendingPrompt::CompareFolder(self.current_path.clone()));
        ui.set_prompt_title(ss("Compare with folder"));
        ui.set_prompt_value(ss(""));
        ui.set_prompt_visible(true);
    }

    fn show_rules(&mut self, ui: &MainWindow) {
        let rules = read_native_json("automation_rules.json", default_automation_rules());
        let body = rules
            .iter()
            .map(|r| {
                format!(
                    "{} [{}] ext:{} tag:{} folder:{}{}",
                    r.name,
                    if r.enabled { "on" } else { "off" },
                    r.extension,
                    r.tag,
                    r.folder,
                    r.move_to
                        .as_ref()
                        .map(|m| format!(" move:{m}"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("Rules and Automation"));
        ui.set_preview_body(ss(body));
        ui.set_preview_meta(ss(
            "Rules are stored in automation_rules.json and are designed to stay opt-in.",
        ));
    }

    fn show_shortcuts(&mut self, ui: &MainWindow) {
        let body = ALL_COMMANDS
            .iter()
            .filter(|(_, _, hint, _)| !hint.is_empty())
            .map(|(_, label, hint, command)| format!("{hint:14} {label} ({command})"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("Shortcut Editor"));
        ui.set_preview_body(ss(body));
        ui.set_preview_meta(ss("Custom shortcut storage is ready for a UI editor without changing the default bindings."));
    }

    fn show_performance_debug(&mut self, ui: &MainWindow) {
        let status = index_status_for_settings(&self.settings);
        let dir_cache = self
            .app_state
            .directory_cache
            .lock()
            .map(|c| c.len())
            .unwrap_or(0);
        let preview_cache = self
            .app_state
            .preview_cache
            .lock()
            .map(|c| c.len())
            .unwrap_or(0);
        let watchers = self.app_state.watchers.lock().map(|w| w.len()).unwrap_or(0);
        ui.set_preview_title(ss("Performance Debug"));
        ui.set_preview_body(ss(format!(
            "Index mode: {}\nIndexed files: {}\nIndex file: {}\nThumbnail cache: {} / {}\nDirectory cache entries: {}\nPreview cache entries: {}\nWatchers: {}\nRoots:\n{}",
            status.mode,
            status.indexed_files,
            format_size_short(status.index_bytes),
            format_size_short(status.thumbnail_bytes),
            format_size_short(status.thumbnail_limit),
            dir_cache,
            preview_cache,
            watchers,
            if status.roots.is_empty() { "Visited folders only".to_string() } else { status.roots.join("\n") }
        )));
        ui.set_preview_meta(ss(status.estimated_storage));
    }

    fn undo(&mut self, ui: &MainWindow) {
        let op = self
            .app_state
            .operation_log
            .lock()
            .ok()
            .and_then(|mut log| log.pop());
        let Some(op) = op else {
            self.show_toast(ui, "Nothing to undo.");
            return;
        };
        let result = match op.kind.as_str() {
            "rename" | "move" => {
                let from = op.to.as_deref().unwrap_or("");
                native_move(&self.app_state, from, &op.from)
            }
            "copy" => op
                .to
                .as_deref()
                .map(native_delete_path)
                .unwrap_or_else(|| Err("Missing copied path".to_string())),
            _ => Err(format!("Cannot undo '{}'", op.kind)),
        };
        match result {
            Ok(()) => {
                self.refresh(ui);
                self.show_toast(ui, "Undone");
            }
            Err(error) => self.show_toast(ui, error),
        }
    }
}

fn native_delete_path(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if p.is_dir() {
        fs::remove_dir_all(p).map_err(|e| e.to_string())
    } else {
        fs::remove_file(p).map_err(|e| e.to_string())
    }
}

fn wire_native_callbacks(ui: &MainWindow, controller: Rc<RefCell<NativeController>>) {
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_navigate(move |path| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().navigate(&ui, path.to_string(), true);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_side_activated(move |index| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().side_activated(&ui, index);
        }
    });

    let preview_debounce = Rc::new(slint::Timer::default());
    let weak = ui.as_weak();
    let c = controller.clone();
    let pd = preview_debounce.clone();
    ui.on_file_selected(move |index, ctrl, shift| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .select_with_modifiers(&ui, index, ctrl, shift);
        }
        // Debounce preview update by 150ms so fast arrow-key navigation
        // doesn't trigger an expensive read for each skipped file.
        let weak2 = weak.clone();
        let c2 = c.clone();
        pd.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(150),
            move || {
                if let Some(ui) = weak2.upgrade() {
                    c2.borrow().update_preview(&ui);
                }
            },
        );
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_file_opened(move |index| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().open_index(&ui, index);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_file_context(move |index| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().select(&ui, index);
            ui.set_context_visible(true);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tab_activated(move |index| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().activate_tab(&ui, index);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tab_closed(move |index| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().close_tab(&ui, index);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_new_tab(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().new_tab(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_go_back(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().go_back(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_go_forward(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().go_forward(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_go_up(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().go_up(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().refresh(&ui);
        }
    });

    // Debounce: keystroke fires search_requested → 200ms timer → search()
    let search_debounce = Rc::new(slint::Timer::default());
    let weak = ui.as_weak();
    let c = controller.clone();
    let sd = search_debounce.clone();
    ui.on_search_requested(move |query| {
        let q = query.to_string();
        let weak2 = weak.clone();
        let c2 = c.clone();
        sd.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(200),
            move || {
                if let Some(ui) = weak2.upgrade() {
                    c2.borrow_mut().search(&ui, q.clone());
                }
            },
        );
    });

    // Enter key: immediate search without debounce
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_search_immediate(move |query| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().search(&ui, query.to_string());
        }
    });

    // Address bar autocomplete with 150ms debounce
    let addr_debounce = Rc::new(slint::Timer::default());
    let weak = ui.as_weak();
    let ad = addr_debounce.clone();
    ui.on_address_changed(move |prefix| {
        let p = prefix.to_string();
        let weak2 = weak.clone();
        ad.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(150),
            move || {
                if let Some(ui) = weak2.upgrade() {
                    let suggestions = suggest_paths(&p, 6);
                    let model: Vec<slint::SharedString> = suggestions.into_iter().map(ss).collect();
                    ui.set_addr_suggestions(std::rc::Rc::new(slint::VecModel::from(model)).into());
                }
            },
        );
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_set_view(move |mode| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().set_view(&ui, &mode);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_toggle_preview(move || {
        if let Some(ui) = weak.upgrade() {
            let visible = !ui.get_preview_visible();
            c.borrow().set_preview_visible(&ui, visible);
        }
    });

    let weak = ui.as_weak();
    ui.on_toggle_dual_pane(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_dual_pane(!ui.get_dual_pane());
        }
    });

    let weak = ui.as_weak();
    ui.on_toggle_hidden(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_show_hidden(!ui.get_show_hidden());
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_command(move |command| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().command(&ui, &command);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_theme_selected(move |theme| {
        if let Some(ui) = weak.upgrade() {
            let mut controller = c.borrow_mut();
            controller.settings.theme = theme.to_string();
            apply_theme(&ui, &controller.settings);
            controller.save_settings();
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_accent_selected(move |accent| {
        if let Some(ui) = weak.upgrade() {
            let mut controller = c.borrow_mut();
            controller.settings.accent = accent.to_string();
            apply_theme(&ui, &controller.settings);
            controller.save_settings();
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_density_selected(move |density| {
        if let Some(ui) = weak.upgrade() {
            let mut controller = c.borrow_mut();
            controller.settings.density = density.to_string();
            apply_theme(&ui, &controller.settings);
            controller.save_settings();
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_index_mode_selected(move |mode| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().set_index_mode(&ui, &mode);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_clear_thumbnail_cache(move || {
        if let Some(ui) = weak.upgrade() {
            match clear_thumbnail_cache() {
                Ok(bytes) => {
                    c.borrow().sync_performance_status(&ui);
                    c.borrow_mut()
                        .show_toast(&ui, format!("Cleared {}", format_size_short(bytes)));
                }
                Err(error) => c.borrow_mut().show_toast(&ui, error),
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_rebuild_index(move || {
        if let Some(ui) = weak.upgrade() {
            let roots = index_roots_for_mode(&c.borrow().settings);
            if roots.is_empty() {
                c.borrow_mut()
                    .show_toast(&ui, "Low mode indexes folders as you open them.");
            } else {
                schedule_index_roots(roots);
                c.borrow_mut()
                    .show_toast(&ui, "Index rebuild started in the background.");
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_minimize(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    let weak = ui.as_weak();
    ui.on_maximize(move || {
        if let Some(ui) = weak.upgrade() {
            let window = ui.window();
            window.set_maximized(!window.is_maximized());
        }
    });

    ui.on_close(move || {
        let _ = slint::quit_event_loop();
    });

    // Poll the thumbnail_ready and git_status_ready flags every 350ms
    {
        let weak = ui.as_weak();
        let c = controller.clone();
        let ready_flag = controller.borrow().thumbnail_ready.clone();
        let git_ready = controller.borrow().git_status_ready.clone();
        let pending_git = controller.borrow().pending_git_status.clone();
        let op_ready = controller.borrow().operation_ready.clone();
        let pending_op = controller.borrow().pending_operation_result.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(350),
            move || {
                let thumb_fired = ready_flag.swap(false, Ordering::AcqRel);
                let git_fired = git_ready.swap(false, Ordering::AcqRel);
                let op_fired = op_ready.swap(false, Ordering::AcqRel);
                if git_fired {
                    if let Ok(mut lock) = pending_git.lock() {
                        if let Some(status) = lock.take() {
                            if let Ok(mut ctrl) = c.try_borrow_mut() {
                                ctrl.git_status = status;
                                ctrl.rebuild_git_dir_status();
                            }
                        }
                    }
                }
                if let Some(ui) = weak.upgrade() {
                    if op_fired {
                        let result = pending_op.lock().ok().and_then(|mut lock| lock.take());
                        ui.set_op_drawer_visible(false);
                        if let Some(result) = result {
                            if let Ok(mut ctrl) = c.try_borrow_mut() {
                                if result.clear_clipboard {
                                    ctrl.clipboard = None;
                                }
                                if result.refresh {
                                    ctrl.refresh(&ui);
                                }
                                ctrl.show_toast_kind(&ui, result.message, &result.kind);
                            }
                        }
                    }
                    if thumb_fired || git_fired {
                        if let Ok(mut ctrl) = c.try_borrow_mut() {
                            ctrl.update_models(&ui);
                        }
                    }
                }
            },
        );
        controller.borrow_mut().thumbnail_timer = Some(timer);
    }

    // Toast queue advancement: poll every 500ms, advance when current toast has been shown >= 3.2s
    {
        let weak = ui.as_weak();
        let c = controller.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(500),
            move || {
                let should_advance = {
                    let ctrl = c.borrow();
                    ctrl.toast_showing
                        && ctrl
                            .toast_last_shown
                            .map(|t| t.elapsed() >= Duration::from_millis(3200))
                            .unwrap_or(false)
                };
                if should_advance {
                    if let Some(ui) = weak.upgrade() {
                        c.borrow_mut().advance_toast_display(&ui);
                    }
                }
            },
        );
        controller.borrow_mut().toast_timer = Some(timer);
    }

    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            start_native_drag(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_confirm_delete(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().confirm_delete(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_prompt_accept(move |value| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().accept_prompt(&ui, value.to_string());
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_sort_column(move |col| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().sort_column(&ui, &col);
        }
    });

    let weak = ui.as_weak();
    ui.on_filter_commands(move |query| {
        if let Some(ui) = weak.upgrade() {
            ui.set_command_items(command_items_filtered(&query));
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_crumb_navigate(move |index| {
        if let Some(ui) = weak.upgrade() {
            let path = {
                use slint::Model;
                ui.get_breadcrumbs()
                    .row_data(index as usize)
                    .map(|item| item.id.to_string())
            };
            if let Some(path) = path {
                c.borrow_mut().navigate(&ui, path, true);
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_rename_file(move |index, new_name| {
        if let Some(ui) = weak.upgrade() {
            let result = {
                let ctrl = c.borrow();
                ctrl.visible_files
                    .get(index as usize)
                    .map(|e| (e.path.clone(), e.name.clone()))
            };
            if let Some((old_path, _old_name)) = result {
                let parent = PathBuf::from(&old_path)
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_default();
                let new_path = parent.join(new_name.as_str());
                match fs::rename(&old_path, &new_path) {
                    Ok(()) => {
                        c.borrow_mut().refresh(&ui);
                        c.borrow_mut().show_toast(&ui, "Renamed");
                    }
                    Err(e) => c.borrow_mut().show_toast(&ui, e.to_string()),
                }
            }
        }
    });

    // Custom theme editor callbacks

    let weak = ui.as_weak();
    ui.on_ce_select_token(move |index| {
        if let Some(ui) = weak.upgrade() {
            use slint::Model;
            let labels = [
                "Background",
                "Background Alt",
                "Panel",
                "Border",
                "Border Strong",
                "Text",
                "Text Muted",
                "Text Faint",
                "Accent",
                "Danger",
                "Success",
            ];
            let i = index as usize;
            let label = labels.get(i).copied().unwrap_or("");
            let hex = ui
                .get_ce_token_hexes()
                .row_data(i)
                .map(|s| s.to_string())
                .unwrap_or_default();
            ui.set_ce_selected_token(index);
            ui.set_ce_token_label(ss(label));
            ui.set_ce_token_hex(ss(&hex));
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_color_changed(move |index, hex_val| {
        if let Some(ui) = weak.upgrade() {
            use slint::Model;
            let i = index as usize;
            let h = hex_val.to_string();
            let h = if h.starts_with('#') {
                h
            } else {
                format!("#{}", h)
            };
            if h.len() != 7 {
                return;
            }
            let parsed_color = color(&h);
            let hexes_model = ui.get_ce_token_hexes();
            if let Some(model) = hexes_model
                .as_any()
                .downcast_ref::<VecModel<SharedString>>()
            {
                model.set_row_data(i, ss(&h));
            }
            let colors_model = ui.get_ce_token_colors();
            if let Some(model) = colors_model.as_any().downcast_ref::<VecModel<Color>>() {
                model.set_row_data(i, parsed_color);
            }
            ui.set_ce_token_hex(ss(&h));
            let def = editor_def_from_ui(&ui);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_radius_changed(move |val| {
        if let Some(ui) = weak.upgrade() {
            ui.set_ce_radius(val);
            let def = editor_def_from_ui(&ui);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_anim_changed(move |val| {
        if let Some(ui) = weak.upgrade() {
            ui.set_ce_anim_speed(val);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_finish_changed(move |finish| {
        if let Some(ui) = weak.upgrade() {
            ui.set_ce_finish(finish.clone());
            apply_window_finish(&ui, &finish);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_font_changed(move |ui_font, mono_font| {
        if let Some(ui) = weak.upgrade() {
            ui.set_ce_ui_font(ui_font.clone());
            ui.set_ce_mono_font(mono_font.clone());
            let def = editor_def_from_ui(&ui);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_font_size_changed(move |delta| {
        if let Some(ui) = weak.upgrade() {
            ui.set_ce_font_size_delta(delta);
            let def = editor_def_from_ui(&ui);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_icon_tint_changed(move |hex_val| {
        if let Some(ui) = weak.upgrade() {
            let h = hex_val.to_string();
            let h = if h.starts_with('#') {
                h
            } else {
                format!("#{}", h)
            };
            ui.set_ce_icon_folder_hex(ss(&h));
            let def = editor_def_from_ui(&ui);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_ce_save(move |name| {
        if let Some(ui) = weak.upgrade() {
            let mut def = editor_def_from_ui(&ui);
            let theme_name = if name.is_empty() {
                "My Theme".to_string()
            } else {
                name.to_string()
            };
            def.name = theme_name.clone();
            match save_custom_theme_def(&def) {
                Ok(()) => {
                    let mut ctrl = c.borrow_mut();
                    ctrl.settings.custom_theme = Some(theme_name.clone());
                    ctrl.save_settings();
                    let saved = list_custom_themes();
                    ui.set_ce_saved_themes(model_from_vec(
                        saved.into_iter().map(SharedString::from).collect(),
                    ));
                    ui.set_active_theme(ss("custom"));
                    ctrl.show_toast(&ui, format!("Theme '{theme_name}' saved"));
                }
                Err(e) => {
                    c.borrow_mut().show_toast(&ui, format!("Save failed: {e}"));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_ce_load(move |name| {
        if let Some(ui) = weak.upgrade() {
            if let Some(def) = load_custom_theme_def(&name) {
                apply_custom_theme_to_ui(&ui, &def);
                apply_window_finish(&ui, &def.finish);
                sync_editor_state(&ui, &def);
                let mut ctrl = c.borrow_mut();
                ctrl.settings.custom_theme = Some(name.to_string());
                ctrl.save_settings();
                ui.set_active_theme(ss("custom"));
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_ce_delete(move |name| {
        if let Some(ui) = weak.upgrade() {
            let _ = delete_custom_theme_def(&name);
            let saved = list_custom_themes();
            ui.set_ce_saved_themes(model_from_vec(
                saved.into_iter().map(SharedString::from).collect(),
            ));
            let mut ctrl = c.borrow_mut();
            if ctrl.settings.custom_theme.as_deref() == Some(name.as_str()) {
                ctrl.settings.custom_theme = None;
                ctrl.save_settings();
                apply_theme(&ui, &ctrl.settings);
            }
            ctrl.show_toast(&ui, format!("Theme '{}' deleted", name));
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_reset_to_active(move || {
        if let Some(ui) = weak.upgrade() {
            let def = ThemeDefinition::default();
            sync_editor_state(&ui, &def);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });
}

fn start_native_drag(ui: &MainWindow) {
    #[cfg(target_os = "windows")]
    {
        use i_slint_backend_winit::WinitWindowAccessor;
        ui.window().with_winit_window(|window| {
            let _ = window.drag_window();
        });
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ui;
    }
}

#[cfg(target_os = "windows")]
fn apply_mica(ui: &MainWindow) {
    use i_slint_backend_winit::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use i_slint_backend_winit::WinitWindowAccessor;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};

    const DWMWA_SYSTEMBACKDROP_TYPE_ID: i32 = 38;
    const DWMSBT_MAINWINDOW: i32 = 2;

    ui.window().with_winit_window(|window| {
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(DWMWA_SYSTEMBACKDROP_TYPE_ID),
                &DWMSBT_MAINWINDOW as *const _ as *const _,
                std::mem::size_of_val(&DWMSBT_MAINWINDOW) as u32,
            );
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn apply_mica(_ui: &MainWindow) {}

pub fn run() {
    let _ = slint::platform::set_platform(Box::new(
        i_slint_backend_winit::Backend::new().expect("failed to create Slint winit backend"),
    ));

    let ui = MainWindow::new().expect("failed to create Pathfinder window");
    let controller = Rc::new(RefCell::new(NativeController::new()));
    controller.borrow_mut().initialize_ui(&ui);
    wire_native_callbacks(&ui, controller.clone());

    // Detect NPU/AI capabilities in background; update UI labels once done
    let weak_ui = ui.as_weak();
    std::thread::spawn(move || {
        let caps = compute_ai_capabilities();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak_ui.upgrade() {
                ui.set_ai_device(SharedString::from(&caps.reason));
                ui.set_ai_label(SharedString::from(ai_status_label(&caps)));
            }
        });
    });

    ui.show().expect("failed to show Pathfinder window");
    apply_mica(&ui);
    slint::run_event_loop().expect("error while running Slint event loop");
}
