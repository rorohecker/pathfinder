use base64::{engine::general_purpose, Engine as _};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Manager, State, Window};
use walkdir::WalkDir;

const DIRECTORY_CACHE_TTL: Duration = Duration::from_secs(20);
const PREVIEW_CACHE_TTL: Duration = Duration::from_secs(180);
const MAX_DIRECTORY_CACHE_ENTRIES: usize = 64;
const MAX_PREVIEW_CACHE_ENTRIES: usize = 96;

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

#[derive(Clone)]
struct AppState {
    directory_cache: Arc<Mutex<HashMap<String, CachedDirectory>>>,
    preview_cache: Arc<Mutex<HashMap<String, CachedPreview>>>,
    watchers: Arc<Mutex<HashMap<String, RecommendedWatcher>>>,
    search_generation: Arc<AtomicU64>,
    ai_capabilities: Arc<Mutex<Option<AiCapabilities>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            directory_cache: Arc::new(Mutex::new(HashMap::new())),
            preview_cache: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            search_generation: Arc::new(AtomicU64::new(0)),
            ai_capabilities: Arc::new(Mutex::new(None)),
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

    FileEntry {
        path: entry_path.to_string_lossy().to_string(),
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
    for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() && !src.is_symlink() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|e| e.to_string())?;
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
    let value = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        value.to_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        value
    }
}

fn cache_key_str(path: &str) -> String {
    cache_key(Path::new(path))
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| match (&a.kind, &b.kind) {
        (FileKind::Directory, FileKind::Directory) => {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
        (FileKind::Directory, _) => std::cmp::Ordering::Less,
        (_, FileKind::Directory) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
}

fn trim_cache<K, V>(cache: &mut HashMap<K, V>, max_entries: usize)
where
    K: Eq + std::hash::Hash + Clone,
{
    while cache.len() > max_entries {
        if let Some(key) = cache.keys().next().cloned() {
            cache.remove(&key);
        } else {
            break;
        }
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
            trim_cache(&mut cache, MAX_DIRECTORY_CACHE_ENTRIES);
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
            trim_cache(&mut cache, MAX_PREVIEW_CACHE_ENTRIES);
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
    let parsed = parse_query(&query);
    let parsed = Arc::new(parsed);
    let results = Arc::new(Mutex::new(Vec::<FileEntry>::new()));
    let full = Arc::new(AtomicBool::new(false));
    let generation = state.search_generation.clone();

    WalkDir::new(&dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .par_bridge()
        .for_each(|entry| {
            if full.load(Ordering::Relaxed) || generation.load(Ordering::SeqCst) != token {
                return;
            }

            let entry_path = entry.path().to_path_buf();
            if entry_path == dir {
                return;
            }

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => return,
            };

            if matches_query(&entry_path, &metadata, &parsed) {
                if let Ok(mut guard) = results.lock() {
                    if guard.len() < max {
                        guard.push(path_to_entry(&entry_path, &metadata));
                    }
                    if guard.len() >= max {
                        full.store(true, Ordering::Relaxed);
                    }
                }
            }
        });

    if state.search_generation.load(Ordering::SeqCst) != token {
        return Ok(Vec::new());
    }

    let mut output = results.lock().map_err(|_| "Search lock failed")?.clone();
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
                    for path in event.paths {
                        callback_state.invalidate_path(&path);
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
$devices = Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
  Where-Object { $_.FriendlyName -match 'NPU|Neural|AI Boost|VPU|Ryzen AI|Hexagon|Hailo|Movidius|Neural Processing' } |
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
    let npu_available = !devices.is_empty();
    let runtime_configured = std::env::var("PATHFINDER_LOCAL_AI_RUNTIME")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let enabled = npu_available && runtime_configured;
    let reason = if enabled {
        format!(
            "NPU detected and local AI runtime configured: {}",
            devices.join(", ")
        )
    } else if npu_available {
        format!(
            "NPU detected ({}) but no PATHFINDER_LOCAL_AI_RUNTIME is configured.",
            devices.join(", ")
        )
    } else {
        "No supported NPU was detected. Local AI features are disabled.".to_string()
    };

    AiCapabilities {
        npu_available,
        semantic_search: enabled,
        automatic_summaries: enabled,
        image_classification: enabled,
        local_embeddings: enabled,
        reason,
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

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_directory,
            get_file_info,
            get_home_directory,
            get_known_folders,
            get_parent_path,
            join_path,
            path_exists,
            get_drives,
            open_file,
            reveal_in_folder,
            rename_file,
            delete_file,
            create_directory,
            copy_file,
            move_file,
            search_files,
            windows_index_search,
            read_preview,
            warm_preview_cache,
            prefetch_paths,
            watch_paths,
            fetch_thumbnails,
            get_ai_capabilities,
            ai_semantic_search,
            ai_summarize_file,
            get_bookmarks,
            save_bookmarks,
            minimize_window,
            toggle_maximize_window,
            close_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
