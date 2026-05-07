#![allow(dead_code)]

use base64::{engine::general_purpose, Engine as _};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
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

fn read_text_for_search(path: &Path, metadata: &fs::Metadata) -> Option<String> {
    if metadata.len() > 1024 * 1024 {
        return None;
    }

    let ext = extension(path);
    if !is_text_ext(&ext) {
        return None;
    }

    fs::read_to_string(path).ok().map(|s| s.to_lowercase())
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
        if !content
            .as_ref()
            .map(|text| text.contains(expected))
            .unwrap_or(false)
        {
            return false;
        }
    }

    for term in &parsed.terms {
        let in_name = name.contains(term);
        let in_content = content
            .as_ref()
            .map(|text| text.contains(term))
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

    let mut output: Vec<FileEntry> = WalkDir::new(&dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .par_bridge()
        .filter_map(|entry| {
            if generation.load(Ordering::Relaxed) != token {
                return None;
            }
            let entry_path = entry.path().to_path_buf();
            if entry_path == dir {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if matches_query(&entry_path, &metadata, &parsed) {
                Some(path_to_entry(&entry_path, &metadata))
            } else {
                None
            }
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
        file.by_ref()
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

            if let Some(cached) = app_state.preview(&key) {
                return cached.data_url.map(|url| (path.clone(), url));
            }

            let img = image::open(&path_buf).ok()?;
            let thumb = img.thumbnail(px, px);
            let mut buf = Vec::new();
            thumb
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
                .ok()?;
            let data_url = format!(
                "data:image/jpeg;base64,{}",
                general_purpose::STANDARD.encode(&buf)
            );
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
struct NativeSettings {
    theme: String,
    accent: String,
    density: String,
    wallpaper: String,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            theme: "mica-dark".to_string(),
            accent: "blue".to_string(),
            density: "cozy".to_string(),
            wallpaper: "none".to_string(),
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
    tags: HashMap<String, String>,
    notes: HashMap<String, String>,
    git_status: Arc<GitStatusMap>,
    git_dir_status: HashMap<String, String>,
    settings: NativeSettings,
    ai: AiCapabilities,
    clipboard: Option<NativeClipboard>,
    pending_prompt: Option<PendingPrompt>,
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
    conn.pragma_update(None, "journal_mode", "DELETE")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
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

    fs::rename(&src, &dst).map_err(|e| e.to_string())?;
    state.invalidate_path(&src);
    state.invalidate_path(&dst);
    state.log_op("rename", path, Some(&dst.to_string_lossy()));
    Ok(dst.to_string_lossy().to_string())
}

fn native_delete(state: &AppState, path: &str) -> Result<(), String> {
    let path_buf = PathBuf::from(path);
    if !path_buf.exists() {
        return Err(format!("Path does not exist: {path}"));
    }
    trash::delete(&path_buf).map_err(|e| e.to_string())?;
    state.invalidate_path(&path_buf);
    Ok(())
}

fn native_create_directory(state: &AppState, path: &str) -> Result<(), String> {
    let path_buf = PathBuf::from(path);
    if path_buf.exists() {
        return Err(format!("Folder already exists: {}", path_buf.display()));
    }
    fs::create_dir_all(&path_buf).map_err(|e| e.to_string())?;
    state.invalidate_path(&path_buf);
    Ok(())
}

fn native_copy(state: &AppState, from: &str, to: &str) -> Result<(), String> {
    let src = PathBuf::from(from);
    let dst = PathBuf::from(to);
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
        state.log_op("copy", from, Some(to));
    }
    result
}

fn native_move(state: &AppState, from: &str, to: &str) -> Result<(), String> {
    let src = PathBuf::from(from);
    let dst = PathBuf::from(to);
    if dst.exists() {
        return Err(format!("Destination already exists: {}", dst.display()));
    }
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
    state.log_op("move", from, Some(to));
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

    let metrics = ui.global::<AppMetrics>();
    metrics.set_radius(palette.radius);
    metrics.set_radius_small(palette.radius_small);
    metrics.set_outer_border(palette.outer_border);
    metrics.set_ui_font(ss(palette.ui_font));
    metrics.set_mono_font(ss(palette.mono_font));
    metrics.set_light_controls(palette.light_controls);

    match settings.density.as_str() {
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

    ui.set_active_theme(ss(&settings.theme));
    ui.set_active_accent(ss(&settings.accent));
    ui.set_active_density(ss(&settings.density));
}

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
    ("Tools", "Checksum", "", "checksum"),
    ("Tools", "Storage Treemap", "", "storage"),
    ("Tools", "Find Duplicates", "", "duplicates"),
    ("Tools", "Operation Log", "", "operation-log"),
    ("Tools", "Undo Last Operation", "Ctrl+Z", "undo"),
    ("View", "Icon View", "Ctrl+1", "view-grid"),
    ("View", "Details View", "Ctrl+2", "view-list"),
    ("View", "Gallery View", "Ctrl+3", "view-gallery"),
    ("View", "Toggle Preview", "Ctrl+I", "toggle-preview"),
    ("View", "Toggle Dual Pane", "F3", "toggle-dual"),
    ("Settings", "Open Settings", "Ctrl+,", "settings"),
];

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
        ui.set_command_items(command_items());
        ui.set_ai_device(ss(&self.ai.reason));
        ui.set_ai_label(ss(ai_status_label(&self.ai)));
        apply_theme(ui, &self.settings);
        let path = self.current_path.clone();
        self.navigate(ui, path, false);
    }

    fn show_toast(&self, ui: &MainWindow, message: impl Into<String>) {
        ui.set_toast_text(ss(message));
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(3000), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_toast_text(ss(""));
            }
        });
    }

    fn save_settings(&self) {
        let _ = write_native_json("settings.json", &self.settings);
    }

    fn save_session(&self) {
        let _ = write_native_json("session.json", &self.tabs);
    }

    fn selected_entry(&self) -> Option<FileEntry> {
        self.visible_files
            .get(self.selected_index as usize)
            .cloned()
    }

    fn apply_filter(&mut self) {
        let query = self.search_query.trim().to_lowercase();
        self.visible_files.clear();
        if query.is_empty() {
            self.visible_files.extend_from_slice(&self.files);
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
    }

    fn update_status(&self, ui: &MainWindow) {
        let sel_count = self.selected_set.len();
        ui.set_status_left(ss(format!(
            "{} items{}",
            self.visible_files.len(),
            if sel_count > 0 {
                format!(" | {} selected", sel_count)
            } else {
                String::new()
            }
        )));
        ui.set_status_right(ss(self.current_path.clone()));
    }

    fn update_models(&mut self, ui: &MainWindow) {
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
        match native_list_directory(&self.app_state, &path) {
            Ok(files) => {
                ui.set_nav_opacity(0.0);
                self.current_path = path.clone();
                self.files = files;
                self.search_query.clear();
                self.selected_index = -1;
                self.selected_set.clear();
                self.select_anchor = -1;
                self.files_model = None;
                self.git_status = native_git_status(&self.app_state, &path);
                self.rebuild_git_dir_status();
                if push_history {
                    self.history.truncate(self.history_index + 1);
                    self.history.push(path.clone());
                    self.history_index = self.history.len().saturating_sub(1);
                }
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.path = path.clone();
                    tab.view = ui.get_view_mode().to_string();
                }
                self.apply_filter();
                self.update_models(ui);
                self.update_preview(ui);
                self.save_session();
                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::from_millis(40), move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_nav_opacity(1.0);
                    }
                });
            }
            Err(error) => self.show_toast(ui, error),
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
        if ui.get_preview_visible() {
            self.update_preview(ui);
        }
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
            return;
        };

        ui.set_preview_title(ss(&entry.name));
        match native_read_preview(&self.app_state, &entry.path, Some(4 * 1024)) {
            Ok(preview) => {
                let body = match preview.kind.as_str() {
                    "image" => "Image preview is available. Native bitmap display is prepared through the preview pipeline.".to_string(),
                    "text" => preview.text.unwrap_or_default(),
                    "folder" => "Folder".to_string(),
                    other => format!("{other} file"),
                };
                let meta = format!(
                    "Path: {}\nType: {}\nSize: {}\nModified: {}{}",
                    entry.path,
                    entry_type(&entry),
                    format_size_short(entry.size),
                    format_modified(entry.modified),
                    if preview.truncated {
                        "\nPreview truncated"
                    } else {
                        ""
                    }
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
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.view = mode.to_string();
        }
        self.save_session();
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
            "paste" => self.paste(ui),
            "checksum" => self.show_checksum(ui),
            "note" => self.prompt_note(ui),
            "storage" => self.show_storage(ui),
            "duplicates" => self.show_duplicates(ui),
            "operation-log" => self.show_operation_log(ui),
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

    fn prompt_delete(&mut self, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            self.show_toast(ui, "Select a file first.");
            return;
        };
        ui.set_confirm_text(ss(format!("Send '{}' to the Recycle Bin?", entry.name)));
        ui.set_confirm_visible(true);
    }

    fn accept_prompt(&mut self, ui: &MainWindow, value: String) {
        match self.pending_prompt.take() {
            Some(PendingPrompt::Rename(path)) => {
                match native_rename(&self.app_state, &path, &value) {
                    Ok(_) => {
                        self.refresh(ui);
                        self.show_toast(ui, "Renamed");
                    }
                    Err(error) => self.show_toast(ui, error),
                }
            }
            Some(PendingPrompt::NewFolder) => {
                let path = PathBuf::from(&self.current_path).join(value.trim());
                match native_create_directory(&self.app_state, &path.to_string_lossy()) {
                    Ok(()) => {
                        self.refresh(ui);
                        self.show_toast(ui, "Folder created");
                    }
                    Err(error) => self.show_toast(ui, error),
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
                self.show_toast(ui, "Note saved");
            }
            Some(PendingPrompt::Archive) | None => {}
        }
    }

    fn confirm_delete(&mut self, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        match native_delete(&self.app_state, &entry.path) {
            Ok(()) => {
                self.refresh(ui);
                self.show_toast(ui, "Deleted");
            }
            Err(error) => self.show_toast(ui, error),
        }
    }

    fn copy_selected(&mut self, cut: bool, ui: &MainWindow) {
        let Some(entry) = self.selected_entry() else {
            self.show_toast(ui, "Select a file first.");
            return;
        };
        self.clipboard = Some(NativeClipboard {
            paths: vec![entry.path],
            cut,
        });
        self.show_toast(
            ui,
            if cut {
                "Cut to clipboard"
            } else {
                "Copied to clipboard"
            },
        );
    }

    fn paste(&mut self, ui: &MainWindow) {
        let Some(clipboard) = self.clipboard.clone() else {
            self.show_toast(ui, "Clipboard is empty.");
            return;
        };
        for src in &clipboard.paths {
            let Some(name) = Path::new(src).file_name() else {
                continue;
            };
            let dest = PathBuf::from(&self.current_path).join(name);
            let result = if clipboard.cut {
                native_move(&self.app_state, src, &dest.to_string_lossy())
            } else {
                native_copy(&self.app_state, src, &dest.to_string_lossy())
            };
            if let Err(error) = result {
                self.show_toast(ui, error);
                return;
            }
        }
        if clipboard.cut {
            self.clipboard = None;
        }
        self.refresh(ui);
        self.show_toast(ui, "Pasted");
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

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_file_selected(move |index, ctrl, shift| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .select_with_modifiers(&ui, index, ctrl, shift);
        }
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

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_search_requested(move |query| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().search(&ui, query.to_string());
        }
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
                        c.borrow().show_toast(&ui, "Renamed");
                    }
                    Err(e) => c.borrow().show_toast(&ui, e.to_string()),
                }
            }
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
