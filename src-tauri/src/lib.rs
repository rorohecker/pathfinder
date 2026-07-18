#![allow(dead_code)]
#![allow(
    clippy::collapsible_if,
    clippy::needless_borrows_for_generic_args,
    clippy::type_complexity
)]

use base64::{Engine as _, engine::general_purpose};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Manager, State, Window};
use walkdir::WalkDir;

#[cfg(target_os = "windows")]
mod windows_integration;

#[cfg(target_os = "windows")]
mod file_drag;
#[cfg(target_os = "windows")]
mod file_icons;
#[cfg(target_os = "windows")]
mod folder_shell_registry;
mod gpu_detect;

// Detection probe helpers used by /examples/probe_npu.rs to verify NPU and
// GPU detection on new hardware without launching the full UI. Kept as
// hidden public functions so the example builds against the lib crate.
#[doc(hidden)]
pub fn __test_detect_npus() -> Vec<String> {
    gpu_detect::detect_npus()
}

#[doc(hidden)]
#[cfg(windows)]
pub fn __test_detect_npus_verbose() {
    gpu_detect::detect_npus_verbose();
}

#[doc(hidden)]
pub fn __test_detect_gpus() -> Vec<(String, u32, u64, bool, bool)> {
    gpu_detect::detect_gpus()
        .adapters
        .into_iter()
        .map(|a| {
            (
                a.name,
                a.vendor_id,
                a.dedicated_video_mb,
                a.is_hardware,
                a.is_discrete,
            )
        })
        .collect()
}
mod inference;
mod local_ai;

slint::include_modules!();

const DIRECTORY_CACHE_TTL: Duration = Duration::from_secs(20);
const PREVIEW_CACHE_TTL: Duration = Duration::from_secs(180);
const DRIVE_SPACE_CACHE_TTL: Duration = Duration::from_secs(10);
const MAX_DIRECTORY_CACHE_ENTRIES: usize = 64;
const MAX_PREVIEW_CACHE_ENTRIES: usize = 96;
// v0.9.11: bumped from 10 to 300. Users explicitly want to see every
// app/folder/file in a bucket when they drill in, not just the top 10
// (which left huge empty space in the list pane). 300 is enough to
// fill any practical viewport and still avoid pushing megabytes of
// strings to Slint for unusually busy buckets.
const STORAGE_BUCKET_DRILL_LIMIT: usize = 500;
// Limit for the overview's largest-items panel. Drill-in and full-list
// modes use STORAGE_BUCKET_DRILL_LIMIT instead.
const STORAGE_TOP_ITEMS_LIMIT: usize = 250;
const STORAGE_TIPS_LIMIT: usize = 12;
const STORAGE_CACHE_MAX_DRIVES: usize = 6;
const STORAGE_DUPLICATE_MIN_SIZE: u64 = 64 * 1024 * 1024;
const INDEX_DB_FILE: &str = ".pathfinder-index.sqlite3";
const THUMBNAIL_CACHE_LIMIT_BYTES: u64 = 50 * 1024 * 1024;
const INDEX_ESTIMATE_BYTES_PER_FILE: u64 = 420;
const MAX_OPERATION_QUEUE_ITEMS: usize = 200;
const MAX_HEAVY_OPS: usize = 2;
const GITHUB_LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/rorohecker/pathfinder/releases/latest";
const GITHUB_RELEASES_URL: &str = "https://github.com/rorohecker/pathfinder/releases";

static ACTIVE_HEAVY_OPS: AtomicUsize = AtomicUsize::new(0);

// Dedicated 2-thread pool for thumbnail generation. Threads run at below-normal
// priority on Windows so they don't compete with foreground I/O.
static THUMBNAIL_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .thread_name(|i| format!("pathfinder-thumb-{i}"))
        .spawn_handler(|thread| {
            std::thread::Builder::new()
                .name(thread.name().unwrap_or("thumb").to_owned())
                .spawn(move || {
                    #[cfg(target_os = "windows")]
                    unsafe {
                        use windows::Win32::System::Threading::{
                            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
                        };
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    thread.run();
                })
                .map(|_| ())
        })
        .build()
        .expect("thumbnail thread pool")
});

// RAII guard that decrements ACTIVE_HEAVY_OPS on drop.
struct HeavyOpGuard;
impl Drop for HeavyOpGuard {
    fn drop(&mut self) {
        ACTIVE_HEAVY_OPS.fetch_sub(1, Ordering::SeqCst);
    }
}

// Suppress the blank console window that Windows shows when spawning a child process.
trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}
impl NoWindow for ProcessCommand {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(0x0800_0000);
        }
        self
    }
}
const FIRST_DIRECTORY_CHUNK: usize = 2_500;
const LARGE_DIRECTORY_GIT_CAP: usize = 20_000;
const SEARCH_INDEX_LIMIT: usize = 1_200;
const SEARCH_LIVE_SCAN_LIMIT: usize = 5_000;
// Whole-drive live scans should surface a useful first page quickly. Asking the
// walker for tens of thousands of matches delays the first visible refresh and
// makes the second search feel faster only because the first one seeded the DB.
const SEARCH_DRIVE_SCAN_LIMIT: usize = 25_000;
const ARCHIVE_SCHEME: &str = "archive://";

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
pub struct UserPin {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub pinned_at: u64,
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
    pub device_name: String,
    pub acceleration_kind: String,
    pub runtime_configured: bool,
    pub reason: String,
    #[serde(default)]
    pub gpu_summary: String,
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

struct DirectoryPage {
    entries: Vec<FileEntry>,
    partial: bool,
    skipped_entries: u32,
}

#[derive(Clone)]
struct NativeDirectoryResult {
    path: String,
    entries: Vec<FileEntry>,
    /// Matches `NativeController::nav_generation` when the worker finished.
    /// Stale results from a superseded navigation are ignored.
    generation: u64,
    partial: bool,
    skipped_entries: u32,
    error: Option<String>,
}

enum SidebarActivateAction {
    None,
    Navigate(String),
    NavigateThenSearch { path: String, query: String },
}

#[derive(Clone)]
struct NativeSearchResult {
    path: String,
    query: String,
    entries: Vec<FileEntry>,
    source: String,
    partial: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileOp {
    pub kind: String,
    pub from: String,
    pub to: Option<String>,
    #[serde(default)]
    pub trash_id: Option<String>,
    /// Populated for `batch_rename` — reversed as one undo step.
    #[serde(default)]
    pub batch: Option<Vec<RenameOp>>,
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
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub encrypted: bool,
}

#[derive(Clone)]
struct ArchiveView {
    archive_path: String,
    prefix: String,
    return_path: String,
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
pub struct StoredDataItem {
    pub label: String,
    pub path: String,
    pub bytes: u64,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PrivacyStorageInfo {
    pub data_dir: String,
    pub cache_dir: String,
    pub index_path: String,
    pub thumbnail_cache_dir: String,
    pub directory_cache_entries: usize,
    pub preview_cache_entries: usize,
    pub watcher_count: usize,
    pub index_bytes: u64,
    pub thumbnail_cache_bytes: u64,
    pub thumbnail_cache_limit: u64,
    pub update_checks_enabled: bool,
    pub network_downloads_enabled: bool,
    pub network_uploads_enabled: bool,
    pub stored_items: Vec<StoredDataItem>,
    pub policy: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpdateCheckResult {
    pub available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub download_url: String,
    pub notes: String,
    pub message: String,
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

// ----- Storage analyzer -----------------------------------------------------
// Windows-style storage breakdown. Walks a root in parallel, categorizes every
// file into one of the standard buckets, and produces both a per-bucket roll-up
// and a flat top-N list of biggest entries (files and folders).

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StorageBucket {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub bytes: u64,
    pub file_count: u64,
    pub color: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StorageEntry {
    pub path: String,
    pub name: String,
    pub bytes: u64,
    #[serde(default)]
    pub modified: i64,
    pub is_dir: bool,
    pub bucket: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct StorageRootMtime {
    pub path: String,
    pub modified: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StorageScanResult {
    pub root: String,
    pub total_bytes: u64,
    pub scanned_files: u64,
    pub scanned_at: i64,
    pub buckets: Vec<StorageBucket>,
    pub top_items: Vec<StorageEntry>,
    // Per-bucket top files (individual), kept for fallback in case a
    // bucket has no large folders to roll up (rare; e.g., a bucket
    // dominated by standalone files like ISOs).
    pub bucket_items: std::collections::HashMap<String, Vec<StorageEntry>>,
    // Per-bucket top folders/apps. This is the primary source the UI
    // drill-in reads - clicking "Apps & games" shows a list of game
    // folders (not the 5 000 individual .pak files inside them).
    // Populated by classifying each top folder via storage_bucket_for.
    pub bucket_folder_items: std::collections::HashMap<String, Vec<StorageEntry>>,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub root_signature: Vec<StorageRootMtime>,
    #[serde(default)]
    pub duplicate_groups: u64,
    #[serde(default)]
    pub duplicate_count: u64,
    #[serde(default)]
    pub duplicate_reclaimable_bytes: u64,
}

/// Shared progress state for an in-flight storage scan. Lock-free counters
/// the background thread bumps as it walks; the UI polling tick reads them
/// to drive the live progress bar.
#[derive(Debug, Default)]
pub struct StorageScanProgress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub done: AtomicBool,
    pub cancelled: AtomicBool,
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
    queue_cancel: Arc<std::sync::atomic::AtomicBool>,
    git_cache: Arc<Mutex<GitCacheMap>>,
    // Debounce map for file watcher indexing: tracks last index time per path
    // to avoid excessive indexing when rapid file system events occur.
    index_debounce: Arc<Mutex<HashMap<String, Instant>>>,
    // Rasterized PDF first pages (RGBA + dimensions) keyed by path|mtime|width,
    // so re-selecting a PDF shows the rendered page instantly instead of
    // re-running pdfium each time. Values are Arc'd for cheap hand-off.
    pdf_page_cache: Arc<Mutex<HashMap<String, Arc<(Vec<u8>, u32, u32)>>>>,
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
            queue_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            git_cache: Arc::new(Mutex::new(HashMap::new())),
            index_debounce: Arc::new(Mutex::new(HashMap::new())),
            pdf_page_cache: Arc::new(Mutex::new(HashMap::new())),
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

fn unix_secs_i64(time: Result<SystemTime, std::io::Error>) -> i64 {
    unix_secs(time) as i64
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a "#RRGGBB" string into a slint::Color. Falls back to a neutral
/// accent if the string is malformed so the storage view never panics on
/// a bad bucket-metadata color.
fn parse_hex_color(hex: &str) -> slint::Color {
    let s = hex.trim_start_matches('#');
    if s.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&s[0..2], 16),
            u8::from_str_radix(&s[2..4], 16),
            u8::from_str_radix(&s[4..6], 16),
        ) {
            return slint::Color::from_rgb_u8(r, g, b);
        }
    }
    slint::Color::from_rgb_u8(0xBF, 0x3A, 0x1F)
}

fn bucket_display_name(id: &str) -> &'static str {
    match storage_canonical_bucket(id) {
        "apps" => "Apps",
        "documents" => "Documents",
        "media" => "Media",
        "downloads" => "Downloads",
        "desktop" => "Desktop",
        "cache" => "Cache",
        "system" => "System",
        _ => "Other",
    }
}

/// Map legacy bucket ids from older scans/cache files into the current taxonomy.
fn storage_canonical_bucket(id: &str) -> &'static str {
    match id {
        "pictures" | "videos" | "music" => "media",
        "temp" => "cache",
        "apps" => "apps",
        "documents" => "documents",
        "media" => "media",
        "downloads" => "downloads",
        "desktop" => "desktop",
        "cache" => "cache",
        "system" => "system",
        "other" => "other",
        _ => "other",
    }
}

fn users_profile_rest(path_bytes: &[u8]) -> Option<&[u8]> {
    const USERS: &[u8] = br"\users\";
    let pos = path_bytes.windows(USERS.len()).position(|window| {
        window
            .iter()
            .zip(USERS)
            .all(|(a, b)| ascii_byte_eq_ci(*a, *b))
    })?;
    let after = &path_bytes[pos + USERS.len()..];
    let sep = after.iter().position(|&b| b == b'\\' || b == b'/')?;
    Some(&after[sep + 1..])
}

fn profile_bucket_for_rest(rest: &[u8]) -> Option<&'static str> {
    if rest.is_empty() {
        return None;
    }
    if rest.starts_with(b"downloads\\") || rest == b"downloads" {
        return Some("downloads");
    }
    if rest.starts_with(b"desktop\\") || rest == b"desktop" {
        return Some("desktop");
    }
    if rest.starts_with(b"documents\\") || rest == b"documents" {
        return Some("documents");
    }
    if rest.starts_with(b"pictures\\")
        || rest == b"pictures"
        || rest.starts_with(b"videos\\")
        || rest == b"videos"
        || rest.starts_with(b"music\\")
        || rest == b"music"
    {
        return Some("media");
    }
    None
}

fn storage_is_app_path(path_bytes: &[u8]) -> bool {
    path_bytes_contains_ci(path_bytes, br"\program files\")
        || path_bytes_contains_ci(path_bytes, br"\program files (x86)\")
        || path_bytes_contains_ci(path_bytes, br"\programdata\")
        || path_bytes_contains_ci(path_bytes, br"\appdata\local\programs\")
        || path_bytes_contains_ci(
            path_bytes,
            br"\appdata\roaming\microsoft\windows\start menu\programs\",
        )
        || path_bytes_contains_ci(path_bytes, br"\steamapps\")
        || path_bytes_contains_ci(path_bytes, br"\epic games\")
        || path_bytes_contains_ci(path_bytes, br"\xboxgames\")
        || path_bytes_contains_ci(path_bytes, br"\riot games\")
        || path_bytes_contains_ci(path_bytes, br"\gog games\")
        || path_bytes_contains_ci(path_bytes, br"\gog galaxy\")
        || path_bytes_contains_ci(path_bytes, br"\ea games\")
        || path_bytes_contains_ci(path_bytes, br"\origin games\")
        || path_bytes_contains_ci(path_bytes, br"\ubisoft game launcher\")
        || path_bytes_contains_ci(path_bytes, br"\battle.net\")
        || path_bytes_contains_ci(path_bytes, br"\bethesda.net launcher\")
        || path_bytes_contains_ci(path_bytes, br"\rockstar games\")
        || path_bytes_contains_ci(path_bytes, br"\steamlibrary\")
        || path_bytes_contains_ci(path_bytes, br"\steam library\")
}

fn temp_path_depth_after_root(path_bytes: &[u8]) -> Option<usize> {
    for marker in [br"\appdata\local\temp\" as &[u8], br"\windows\temp\"] {
        if let Some(pos) = path_bytes.windows(marker.len()).position(|window| {
            window
                .iter()
                .zip(marker.iter())
                .all(|(a, b)| ascii_byte_eq_ci(*a, *b))
        }) {
            let rest = &path_bytes[pos + marker.len()..];
            let depth = rest
                .split(|&b| b == b'\\' || b == b'/')
                .filter(|seg| !seg.is_empty())
                .count();
            return Some(depth);
        }
    }
    None
}

fn storage_is_cache_path(path_bytes: &[u8], file_name: &str) -> bool {
    if path_bytes_contains_ci(path_bytes, br"\inetcache\") {
        return true;
    }
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".tmp") || lower.ends_with(".temp") {
        return true;
    }
    // Only shallow temp files count as cache. Deep trees under %TEMP%
    // (installers, extracted archives, dev sandboxes) stay in their
    // real category instead of being lumped into "Cache & temp".
    if let Some(depth) = temp_path_depth_after_root(path_bytes) {
        return depth <= 2;
    }
    false
}

fn bucket_color_for(id: &str) -> slint::Color {
    storage_bucket_meta()
        .iter()
        .find(|(bid, _, _, _)| *bid == id)
        .map(|(_, _, _, hex)| parse_hex_color(hex))
        .unwrap_or_else(|| slint::Color::from_rgb_u8(0x4A, 0x6A, 0x20))
}

#[inline]
fn ascii_byte_eq_ci(a: u8, b: u8) -> bool {
    a == b || (a.is_ascii_alphabetic() && b.is_ascii_alphabetic() && (a | 0x20) == (b | 0x20))
}

fn bytes_prefix_eq_ci(haystack: &[u8], prefix: &[u8]) -> bool {
    haystack.len() >= prefix.len()
        && haystack[..prefix.len()]
            .iter()
            .zip(prefix)
            .all(|(a, b)| ascii_byte_eq_ci(*a, *b))
}

/// Case-insensitive substring search on a path without allocating a lowercase copy.
fn path_bytes_contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(a, b)| ascii_byte_eq_ci(*a, *b))
    })
}

/// Precomputed paths for fast per-file categorization during a scan.
struct StorageScanCtx {
    home_lower: Option<Vec<u8>>,
}

impl StorageScanCtx {
    fn new() -> Self {
        let home_lower = dirs::home_dir().map(|h| {
            h.to_string_lossy()
                .trim_end_matches(['\\', '/'])
                .to_ascii_lowercase()
                .into_bytes()
        });
        Self { home_lower }
    }
}

/// Generate actionable recommendations + explanations for the
/// storage overview panel. Pulls from the scan result so we don't
/// need to re-walk the disk. Output is a small ordered list - the
/// most actionable / largest items come first so the user can see
/// quick wins without scrolling.
fn build_storage_tips(result: &StorageScanResult) -> Vec<StorageTipUi> {
    let mut tips: Vec<StorageTipUi> = Vec::new();

    // System reserved files: pagefile / hiberfil / swapfile. These
    // tend to top the list on Windows drives and confuse users who
    // try to delete them. Annotate with what they are + how to
    // shrink them rather than treating them as "junk".
    for e in result.top_items.iter() {
        let lower = e.name.to_ascii_lowercase();
        match lower.as_str() {
            "pagefile.sys" => tips.push(StorageTipUi {
                icon: ss("PG"),
                title: ss("pagefile.sys - Windows virtual memory"),
                detail: ss(
                    "System-managed swap file. Resize via Settings > System > About > Advanced \
                     system settings > Performance > Virtual memory. Do not delete manually.",
                ),
                bytes_text: ss(format_size_short(e.bytes)),
                severity: ss("info"),
                path: ss(""),
                action: ss(""),
                accent: TIP_ACCENT_INFO,
            }),
            "hiberfil.sys" => tips.push(StorageTipUi {
                icon: ss("HB"),
                title: ss("hiberfil.sys - Hibernation cache"),
                detail: ss(
                    "Reserved for Fast Startup and Hibernate. Disable via an admin PowerShell: \
                     `powercfg /h off` (or `/h size <N>` to shrink) to reclaim this space.",
                ),
                bytes_text: ss(format_size_short(e.bytes)),
                severity: ss("info"),
                path: ss(""),
                action: ss(""),
                accent: TIP_ACCENT_INFO,
            }),
            "swapfile.sys" => tips.push(StorageTipUi {
                icon: ss("SW"),
                title: ss("swapfile.sys - Modern app paging"),
                detail: ss(
                    "Windows uses this alongside pagefile.sys for UWP / packaged apps. \
                     Automatically managed; deleting it has no lasting effect.",
                ),
                bytes_text: ss(format_size_short(e.bytes)),
                severity: ss("info"),
                path: ss(""),
                action: ss(""),
                accent: TIP_ACCENT_INFO,
            }),
            _ => {}
        }
    }

    // Large items inside common "review and delete" locations. We
    // group multiple paths under the same parent so a Downloads
    // folder with 30 stale installers shows as one tip, not 30.
    add_location_tip(
        &mut tips,
        result,
        "DL",
        "Downloads",
        "Old downloads worth reviewing",
        &["\\downloads\\", "/downloads/"],
        TIP_ACCENT_TIP,
        "",
    );
    add_location_tip(
        &mut tips,
        result,
        "TM",
        "Temp / cache files",
        "Temporary files and browser caches you can usually clear safely",
        &[
            "\\appdata\\local\\temp\\",
            "\\windows\\temp\\",
            "\\inetcache\\",
        ],
        TIP_ACCENT_TIP,
        "",
    );
    add_location_tip(
        &mut tips,
        result,
        "RB",
        "Recycle Bin",
        "Files waiting to be permanently deleted. Empty the bin to reclaim.",
        &["\\$recycle.bin\\"],
        TIP_ACCENT_WARN,
        "",
    );

    add_age_based_tips(&mut tips, result);

    if result.duplicate_count > 0 && result.duplicate_reclaimable_bytes >= 128 * 1024 * 1024 {
        tips.push(StorageTipUi {
            icon: ss("DU"),
            title: ss(format!(
                "{} large duplicate{} found",
                result.duplicate_count,
                if result.duplicate_count == 1 { "" } else { "s" }
            )),
            detail: ss(format!(
                "{} duplicate group{} above {} each. Open Duplicate Finder before deleting so you keep the right copy.",
                result.duplicate_groups,
                if result.duplicate_groups == 1 { "" } else { "s" },
                format_size_short(STORAGE_DUPLICATE_MIN_SIZE)
            )),
            bytes_text: ss(format_size_short(result.duplicate_reclaimable_bytes)),
            severity: ss("warn"),
            path: ss(""),
            action: ss("duplicates"),
            accent: TIP_ACCENT_WARN,
        });
    }

    add_orphan_app_tip(&mut tips, result);

    // Per-bucket hints for the largest categories. Encourage drilling
    // in when one bucket dominates - that's usually where the biggest
    // cleanup wins live.
    let total = result.total_bytes.max(1);
    for b in result.buckets.iter() {
        let pct = (b.bytes as f64 / total as f64) * 100.0;
        if pct < 20.0 || b.id == "other" {
            continue;
        }
        let (icon, hint) = match b.id.as_str() {
            "apps" => (
                "AG",
                "Click the Apps & games card to find the largest installs - uninstalling unused \
                 ones is usually the fastest way to reclaim space.",
            ),
            "documents" => (
                "DC",
                "Click the Documents card to surface the biggest folders. Archives, project \
                 backups, and downloads pile up here.",
            ),
            "media" => (
                "MD",
                "Media files dominate this drive. Drill in to find large photos, videos, \
                 or music you can archive or remove.",
            ),
            "cache" => (
                "TC",
                "Temporary and cache files add up quickly. These are usually safe to clear \
                 from Settings > System > Storage.",
            ),
            "system" => (
                "SY",
                "System data is large. Windows Update files, driver packages, and component \
                 stores accumulate over time.",
            ),
            _ => continue,
        };
        tips.push(StorageTipUi {
            icon: ss(icon),
            title: ss(format!(
                "{} uses {}% ({})",
                b.name,
                ((pct * 10.0).round() / 10.0),
                format_size_short(b.bytes)
            )),
            detail: ss(hint),
            bytes_text: ss(""),
            severity: ss("tip"),
            path: ss(""),
            action: ss(""),
            accent: TIP_ACCENT_TIP,
        });
    }

    // Cap so the panel never becomes overwhelming.
    tips.truncate(12);
    tips
}

/// Sum up sizes for top_items whose path contains any of `markers`,
/// then push a single tip summarising the group. The first matching
/// path becomes the click target so the user can jump straight to it.
#[allow(clippy::too_many_arguments)]
fn add_location_tip(
    out: &mut Vec<StorageTipUi>,
    result: &StorageScanResult,
    icon: &str,
    title: &str,
    detail: &str,
    markers: &[&str],
    accent: slint::Color,
    action: &str,
) {
    let mut total: u64 = 0;
    let mut count: u32 = 0;
    let mut first_path: Option<String> = None;
    let mut largest: u64 = 0;
    for e in result.top_items.iter() {
        let lower = e.path.to_ascii_lowercase();
        if !markers.iter().any(|m| lower.contains(m)) {
            continue;
        }
        total = total.saturating_add(e.bytes);
        count += 1;
        if e.bytes > largest {
            largest = e.bytes;
            first_path = Some(e.path.clone());
        }
    }
    if count == 0 || total < 100 * 1024 * 1024 {
        return; // Skip when the cluster is too small to be worth surfacing.
    }
    out.push(StorageTipUi {
        icon: ss(icon),
        title: ss(format!(
            "{} ({} item{})",
            title,
            count,
            if count == 1 { "" } else { "s" }
        )),
        detail: ss(detail),
        bytes_text: ss(format_size_short(total)),
        severity: ss("tip"),
        path: ss(&first_path.unwrap_or_default()),
        action: ss(action),
        accent,
    });
}

fn storage_entry_key(path: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        path.to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.to_string()
    }
}

fn collect_storage_tip_entries(result: &StorageScanResult) -> Vec<StorageEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for entry in result
        .top_items
        .iter()
        .chain(result.bucket_items.values().flatten())
        .chain(result.bucket_folder_items.values().flatten())
    {
        if seen.insert(storage_entry_key(&entry.path)) {
            out.push(entry.clone());
        }
    }
    out.sort_unstable_by_key(|e| std::cmp::Reverse(e.bytes));
    out
}

fn entry_path_has_marker(entry: &StorageEntry, markers: &[&str]) -> bool {
    let lower = entry.path.to_ascii_lowercase();
    markers.iter().any(|m| lower.contains(m))
}

fn entry_is_system_reserved(entry: &StorageEntry) -> bool {
    let name = entry.name.to_ascii_lowercase();
    entry.bucket == "system"
        || matches!(
            name.as_str(),
            "pagefile.sys" | "hiberfil.sys" | "swapfile.sys"
        )
}

fn add_age_based_tips(out: &mut Vec<StorageTipUi>, result: &StorageScanResult) {
    let now = now_unix_secs() as i64;
    let entries = collect_storage_tip_entries(result);

    let old_downloads: Vec<StorageEntry> = entries
        .iter()
        .filter(|e| {
            e.modified > 0
                && now.saturating_sub(e.modified) > 90 * 86_400
                && entry_path_has_marker(e, &["\\downloads\\", "/downloads/"])
        })
        .take(40)
        .cloned()
        .collect();
    push_cleanup_tip(
        out,
        "90",
        "Old downloads (>90d)",
        "These downloads have not changed in more than 90 days. Review before deleting.",
        "cleanup:old-downloads",
        old_downloads,
        TIP_ACCENT_TIP,
    );

    let stale: Vec<StorageEntry> = entries
        .iter()
        .filter(|e| {
            e.modified > 0
                && now.saturating_sub(e.modified) > 365 * 86_400
                && !entry_is_system_reserved(e)
                && !entry_path_has_marker(e, &["\\windows\\", "/windows/"])
        })
        .take(40)
        .cloned()
        .collect();
    push_cleanup_tip(
        out,
        "1Y",
        "Stale >1y",
        "Large items that have not changed in over a year. Good candidates for archiving or cleanup.",
        "cleanup:stale",
        stale,
        TIP_ACCENT_WARN,
    );
}

fn push_cleanup_tip(
    out: &mut Vec<StorageTipUi>,
    icon: &str,
    title: &str,
    detail: &str,
    action: &str,
    entries: Vec<StorageEntry>,
    accent: slint::Color,
) {
    let count = entries.len();
    let total: u64 = entries.iter().map(|e| e.bytes).sum();
    if count == 0 || total < 100 * 1024 * 1024 {
        return;
    }
    let first_path = entries.first().map(|e| e.path.clone()).unwrap_or_default();
    out.push(StorageTipUi {
        icon: ss(icon),
        title: ss(format!(
            "{} ({} item{})",
            title,
            count,
            if count == 1 { "" } else { "s" }
        )),
        detail: ss(detail),
        bytes_text: ss(format_size_short(total)),
        severity: ss("warn"),
        path: ss(first_path),
        action: ss(action),
        accent,
    });
}

fn app_owner_from_storage_path(path: &str) -> Option<String> {
    let normalized = path.replace('/', "\\");
    let parts: Vec<&str> = normalized
        .split('\\')
        .filter(|part| !part.is_empty())
        .collect();
    for idx in 0..parts.len() {
        if parts[idx].eq_ignore_ascii_case("programdata") {
            return parts.get(idx + 1).map(|s| (*s).to_string());
        }
        if parts[idx].eq_ignore_ascii_case("appdata")
            && parts
                .get(idx + 1)
                .map(|s| s.eq_ignore_ascii_case("local") || s.eq_ignore_ascii_case("roaming"))
                .unwrap_or(false)
        {
            return parts.get(idx + 2).map(|s| (*s).to_string());
        }
    }
    None
}

fn normalize_app_match_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn is_generic_appdata_owner(owner: &str) -> bool {
    matches!(
        owner.to_ascii_lowercase().as_str(),
        "microsoft"
            | "windows"
            | "packages"
            | "package cache"
            | "temp"
            | "cache"
            | "crashdumps"
            | "connecteddevicesplatform"
            | "nvidia"
            | "intel"
            | "amd"
    )
}

fn installed_app_names_for_orphan_check() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        installed_app_display_names_windows()
            .into_iter()
            .map(|name| normalize_app_match_name(&name))
            .filter(|name| name.len() >= 4)
            .collect()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "windows")]
fn installed_app_display_names_windows() -> Vec<String> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey, RegEnumKeyExW,
        RegOpenKeyExW, RegQueryValueExW,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn wide_nul(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn read_reg_string(key: HKEY, name: &str) -> Option<String> {
        let name = wide_nul(name);
        let mut kind = REG_SZ;
        let mut buf = [0u16; 512];
        let mut size = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let chars = (size as usize / 2).saturating_sub(1).min(buf.len());
        let value = String::from_utf16_lossy(&buf[..chars]).trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    fn enumerate_uninstall_key(root: HKEY, rel: &str, out: &mut Vec<String>) {
        let rel = wide_nul(rel);
        let mut key = HKEY::default();
        if unsafe { RegOpenKeyExW(root, PCWSTR(rel.as_ptr()), None, KEY_READ, &mut key) }
            != ERROR_SUCCESS
        {
            return;
        }

        for idx in 0..4096u32 {
            let mut name_buf = [0u16; 256];
            let mut name_len = name_buf.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    key,
                    idx,
                    Some(PWSTR(name_buf.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if rc != ERROR_SUCCESS {
                break;
            }
            let sub_path: Vec<u16> = name_buf[..name_len as usize]
                .iter()
                .copied()
                .chain(std::iter::once(0))
                .collect();
            let mut sub = HKEY::default();
            if unsafe { RegOpenKeyExW(key, PCWSTR(sub_path.as_ptr()), None, KEY_READ, &mut sub) }
                == ERROR_SUCCESS
            {
                if let Some(display_name) = read_reg_string(sub, "DisplayName") {
                    out.push(display_name);
                }
                unsafe {
                    let _ = RegCloseKey(sub);
                }
            }
        }
        unsafe {
            let _ = RegCloseKey(key);
        }
    }

    let mut out = Vec::new();
    for rel in [
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ] {
        enumerate_uninstall_key(HKEY_CURRENT_USER, rel, &mut out);
        enumerate_uninstall_key(HKEY_LOCAL_MACHINE, rel, &mut out);
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn installed_app_name_matches(owner: &str, installed: &[String]) -> bool {
    let candidate = normalize_app_match_name(owner);
    if candidate.len() < 4 {
        return true;
    }
    installed.iter().any(|name| {
        name == &candidate
            || (candidate.len() >= 5 && name.contains(&candidate))
            || (name.len() >= 5 && candidate.contains(name))
    })
}

fn add_orphan_app_tip(out: &mut Vec<StorageTipUi>, result: &StorageScanResult) {
    let installed = installed_app_names_for_orphan_check();
    if installed.is_empty() {
        return;
    }

    let mut by_owner: HashMap<String, (String, u64)> = HashMap::new();
    for entry in collect_storage_tip_entries(result) {
        if !entry.is_dir || entry.bytes < 25 * 1024 * 1024 {
            continue;
        }
        let lower = entry.path.to_ascii_lowercase();
        if !(lower.contains("\\appdata\\")
            || lower.contains("/appdata/")
            || lower.contains("\\programdata\\")
            || lower.contains("/programdata/"))
        {
            continue;
        }
        let Some(owner) = app_owner_from_storage_path(&entry.path) else {
            continue;
        };
        if is_generic_appdata_owner(&owner) || installed_app_name_matches(&owner, &installed) {
            continue;
        }
        let key = normalize_app_match_name(&owner);
        let owner_entry = by_owner.entry(key).or_insert((entry.path.clone(), 0));
        owner_entry.1 = owner_entry.1.saturating_add(entry.bytes);
    }

    let count = by_owner.len();
    let total: u64 = by_owner.values().map(|(_, bytes)| *bytes).sum();
    if count == 0 || total < 100 * 1024 * 1024 {
        return;
    }
    let first_path = by_owner
        .values()
        .max_by_key(|(_, bytes)| *bytes)
        .map(|(path, _)| path.clone())
        .unwrap_or_default();
    out.push(StorageTipUi {
        icon: ss("OR"),
        title: ss(format!(
            "Possible orphan app data ({} folder{})",
            count,
            if count == 1 { "" } else { "s" }
        )),
        detail: ss(
            "AppData / ProgramData folders whose names do not match installed apps. Review before deleting.",
        ),
        bytes_text: ss(format_size_short(total)),
        severity: ss("warn"),
        path: ss(first_path),
        action: ss(""),
        accent: TIP_ACCENT_WARN,
    });
}

const TIP_ACCENT_INFO: slint::Color = slint::Color::from_rgb_u8(0x4a, 0x9c, 0xff);
const TIP_ACCENT_TIP: slint::Color = slint::Color::from_rgb_u8(0x58, 0xc7, 0x7a);
const TIP_ACCENT_WARN: slint::Color = slint::Color::from_rgb_u8(0xf2, 0xa8, 0x4a);

/// "5 seconds ago" / "3 minutes ago" / "1 day ago". Used by the Storage tab
/// to display when the cached scan was last refreshed.
fn format_relative_time(ts: i64) -> String {
    if ts <= 0 {
        return "just now".to_string();
    }
    let now = now_unix_secs() as i64;
    let diff = (now - ts).max(0);
    if diff < 5 {
        "just now".to_string()
    } else if diff < 60 {
        format!("{}s", diff)
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86400)
    }
}

fn format_storage_entry_age(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let now = now_unix_secs() as i64;
    let days = ((now - ts).max(0) / 86_400) as u64;
    if days == 0 {
        "today".to_string()
    } else if days == 1 {
        "yesterday".to_string()
    } else if days < 45 {
        format!("{days}d ago")
    } else if days < 365 {
        let months = (days / 30).max(1);
        if months == 1 {
            "1 month ago".to_string()
        } else {
            format!("{months} months ago")
        }
    } else {
        let years = (days / 365).max(1);
        if years == 1 {
            "1y ago".to_string()
        } else {
            format!("{years}y ago")
        }
    }
}

fn capture_storage_root_signature(root: &Path) -> Vec<StorageRootMtime> {
    let Ok(read_dir) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<StorageRootMtime> = read_dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = entry.metadata().ok()?;
            Some(StorageRootMtime {
                path: path.to_string_lossy().into_owned(),
                modified: unix_secs_i64(metadata.modified()),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.path
            .to_ascii_lowercase()
            .cmp(&b.path.to_ascii_lowercase())
    });
    out
}

// Note: storage_root_signature_matches was removed in v0.9.18 - it
// powered a Rescan fast-path that reused the cache when top-level
// subdir mtimes hadn't changed, but uninstallers that leave stale
// folders behind would silently slip past the check. The signature
// is still persisted in the scan result for potential future use
// (e.g. an explicit "Reload from cache" affordance).

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
            .map(|e| e.to_string_lossy().to_lowercase())
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

const OP_CANCELLED: &str = "Operation cancelled.";

fn copy_dir_recursive(state: &AppState, from: &Path, to: &Path) -> Result<(), String> {
    if state.queue_cancel_requested() {
        return Err(OP_CANCELLED.to_string());
    }
    if to.exists() {
        return Err(format!("Destination already exists: {}", to.display()));
    }
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        if state.queue_cancel_requested() {
            return Err(OP_CANCELLED.to_string());
        }
        for entry in fs::read_dir(&src_dir).map_err(|e| e.to_string())? {
            if state.queue_cancel_requested() {
                return Err(OP_CANCELLED.to_string());
            }
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

fn names_same_for_rename(old_name: &str, new_name: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        old_name.eq_ignore_ascii_case(new_name)
    }
    #[cfg(not(target_os = "windows"))]
    {
        old_name == new_name
    }
}

/// Merge the inline-rename stem the user typed with the original extension.
fn finalize_inline_rename_name(old_name: &str, typed: &str, is_dir: bool) -> String {
    let typed = typed.trim();
    if typed.is_empty() || is_dir {
        return typed.to_string();
    }
    let old = Path::new(old_name);
    if old
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') && !n[1..].contains('.'))
    {
        return typed.to_string();
    }
    let Some(ext) = old.extension() else {
        return typed.to_string();
    };
    let ext = ext.to_string_lossy();
    if typed.contains('.') {
        return typed.to_string();
    }
    format!("{typed}.{ext}")
}

#[cfg(target_os = "windows")]
fn today_yyyy_mm_dd() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
}

#[cfg(not(target_os = "windows"))]
fn today_yyyy_mm_dd() -> String {
    "1970-01-01".to_string()
}

fn apply_rename_preset(preset: &str, file_name: &str) -> Option<String> {
    match preset {
        "lowercase extensions" => {
            if let Some(pos) = file_name.rfind('.') {
                Some(format!(
                    "{}{}",
                    &file_name[..pos],
                    file_name[pos..].to_ascii_lowercase()
                ))
            } else {
                Some(file_name.to_string())
            }
        }
        "replace spaces with dashes" => Some(file_name.replace(' ', "-")),
        "prefix date" => Some(format!("{}_{file_name}", today_yyyy_mm_dd())),
        "number sequence" => None,
        _ => None,
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
    // Cap AFTER filtering for files. The previous implementation took the first
    // `max_entries` of the raw WalkDir iterator (which yields directories first
    // for nested structures), so a folder with > 25k subdirectories before any
    // file in walk order would report a total of 0 even though its files were
    // many GB. Now the cap applies to *files only*, so we always see real bytes.
    WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|m| m.is_file())
        .take(max_entries)
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

fn entry_sort_cmp(a: &FileEntry, b: &FileEntry) -> std::cmp::Ordering {
    match (&a.kind, &b.kind) {
        (FileKind::Directory, FileKind::Directory) => natural_cmp(&a.name_lower, &b.name_lower),
        (FileKind::Directory, _) => std::cmp::Ordering::Less,
        (_, FileKind::Directory) => std::cmp::Ordering::Greater,
        _ => natural_cmp(&a.name_lower, &b.name_lower),
    }
}

fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(entry_sort_cmp);
}

fn merge_sorted_file_entries(left: Vec<FileEntry>, mut right: Vec<FileEntry>) -> Vec<FileEntry> {
    if left.is_empty() {
        sort_entries(&mut right);
        return right;
    }
    if right.is_empty() {
        return left;
    }
    sort_entries(&mut right);
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut li = 0usize;
    let mut ri = 0usize;
    while li < left.len() && ri < right.len() {
        if entry_sort_cmp(&left[li], &right[ri]) != std::cmp::Ordering::Greater {
            merged.push(left[li].clone());
            li += 1;
        } else {
            merged.push(right[ri].clone());
            ri += 1;
        }
    }
    if li < left.len() {
        merged.extend_from_slice(&left[li..]);
    }
    if ri < right.len() {
        merged.extend(right.drain(ri..));
    }
    merged
}

fn dir_entry_name_cmp(a: &fs::DirEntry, b: &fs::DirEntry) -> std::cmp::Ordering {
    a.file_name()
        .to_string_lossy()
        .to_ascii_lowercase()
        .cmp(
            &b.file_name()
                .to_string_lossy()
                .to_ascii_lowercase(),
        )
}

fn sort_entries_by(entries: &mut [FileEntry], sort_by: &str, sort_dir: &str) {
    entries.sort_by(|a, b| {
        if sort_by == "name" {
            match (&a.kind, &b.kind) {
                (FileKind::Directory, FileKind::Directory) => {}
                (FileKind::Directory, _) => return std::cmp::Ordering::Less,
                (_, FileKind::Directory) => return std::cmp::Ordering::Greater,
                _ => {}
            }
        }
        let ord = match sort_by {
            "size" => a.size.cmp(&b.size),
            "modified" => a.modified.cmp(&b.modified),
            "type" => {
                let ta = a.extension.as_deref().unwrap_or("").to_lowercase();
                let tb = b.extension.as_deref().unwrap_or("").to_lowercase();
                natural_cmp(&ta, &tb)
            }
            _ => natural_cmp(&a.name_lower, &b.name_lower),
        };
        if sort_dir == "desc" {
            ord.reverse()
        } else {
            ord
        }
    });
}

/// Read the OS recycle bin and return its contents as virtual FileEntry rows.
/// Path field carries the `recycle://<original-path>` URI so the controller can
/// reverse-look-up the trash item later (for restore / permanent delete).
fn list_recycle_bin_entries() -> Vec<FileEntry> {
    let items = match trash::os_limited::list() {
        Ok(items) => items,
        Err(_) => return Vec::new(),
    };
    let mut entries: Vec<FileEntry> = items
        .into_iter()
        .map(|item| {
            let original = item.original_path();
            let original_str = original.to_string_lossy().into_owned();
            let name: String = std::path::Path::new(&item.name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| item.name.to_string_lossy().into_owned());
            let virtual_path = format!("recycle://{}", original_str);
            let extension = std::path::Path::new(&name)
                .extension()
                .map(|e| e.to_string_lossy().into_owned());
            let modified = item.time_deleted.max(0) as u64;
            FileEntry {
                path: virtual_path,
                name_lower: name.to_lowercase(),
                name,
                kind: FileKind::File,
                size: 0,
                modified,
                extension,
            }
        })
        .collect();
    sort_entries(&mut entries);
    entries
}

/// Apply a batch-rename template to one file and produce its new filename.
///
/// Tokens (case-sensitive):
///   `{n}`     1-based sequence number (no padding)
///   `{n:0N}`  zero-padded to N digits, e.g. `{n:04}` -> `0007`
///   `{name}`  original filename without extension
///   `{ext}`   original extension (without the dot)
///
/// Anything else passes through literally. Unknown tokens render as themselves
/// (e.g. `{foo}` stays `{foo}`) so typos don't silently corrupt output.
fn apply_rename_template(template: &str, src: &std::path::Path, n: usize) -> String {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut out = String::with_capacity(template.len() + 16);
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Find the matching '}'.
            if let Some(end_off) = bytes[i + 1..].iter().position(|&b| b == b'}') {
                let token = &template[i + 1..i + 1 + end_off];
                let resolved = match token {
                    "n" => n.to_string(),
                    "name" => stem.clone(),
                    "ext" => ext.clone(),
                    t if t.starts_with("n:0") => {
                        if let Ok(width) = t[3..].parse::<usize>() {
                            format!("{:0width$}", n, width = width)
                        } else {
                            format!("{{{t}}}")
                        }
                    }
                    other => format!("{{{other}}}"),
                };
                out.push_str(&resolved);
                i += 1 + end_off + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    // If the template ends in `.` because `{ext}` was empty (extension-less
    // source file), drop the dangling separator. NTFS would strip the trailing
    // dot anyway, so keeping it just makes the preview look wrong.
    while out.ends_with('.') {
        out.pop();
    }
    out
}

#[derive(Debug, Clone)]
struct BatchRenameParams {
    find: String,
    replace: String,
    prefix: String,
    suffix: String,
    template: String,
}

fn apply_batch_rename_advanced(
    params: &BatchRenameParams,
    src: &std::path::Path,
    n: usize,
) -> Result<String, String> {
    let template = params.template.trim();
    let mut name = if template.is_empty() {
        src.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        apply_rename_template(template, src, n)
    };

    if !params.find.trim().is_empty() {
        let re = regex::Regex::new(params.find.trim())
            .map_err(|e| format!("Invalid find pattern: {e}"))?;
        name = re
            .replace_all(&name, params.replace.as_str())
            .into_owned();
    }

    if !params.prefix.is_empty() {
        name = format!("{}{name}", params.prefix);
    }

    if !params.suffix.is_empty() {
        if let Some(dot) = name.rfind('.') {
            if dot > 0 {
                name = format!("{}{}{}", &name[..dot], params.suffix, &name[dot..]);
            } else {
                name.push_str(&params.suffix);
            }
        } else {
            name.push_str(&params.suffix);
        }
    }

    let name = name.trim();
    if name.is_empty() {
        return Err("Produced empty filename".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("Name cannot contain path separators".to_string());
    }
    Ok(name.to_string())
}

fn batch_rename_preview_lines(paths: &[String], params: &BatchRenameParams, limit: usize) -> String {
    let mut lines = Vec::new();
    for (i, path) in paths.iter().take(limit).enumerate() {
        let src = Path::new(path);
        let original = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        match apply_batch_rename_advanced(params, src, i + 1) {
            Ok(renamed) => lines.push(format!("{original}  ->  {renamed}")),
            Err(error) => return format!("Preview error: {error}"),
        }
    }
    lines.join("\n")
}

fn native_rename_replace(state: &AppState, path: &str, new_name: &str) -> Result<String, String> {
    let src = PathBuf::from(path);
    let parent = src.parent().ok_or("No parent directory")?;
    let dst = parent.join(new_name.trim());
    if dst.exists() && !same_destination(&src, &dst) {
        if dst.is_dir() {
            return Err("Cannot replace a folder with a file rename".to_string());
        }
        fs::remove_file(&dst).map_err(|e| e.to_string())?;
    }
    native_rename(state, path, new_name)
}

fn pdf_first_page_preview(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() > 12 * 1024 * 1024 {
        return None;
    }
    let text = pdf_extract::extract_text_from_mem(&bytes).ok()?;
    let preview: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(48)
        .collect::<Vec<_>>()
        .join("\n");
    if preview.is_empty() {
        None
    } else {
        Some(preview)
    }
}

/// Width (in pixels) the PDF first page is rasterized to for the preview pane.
/// Big enough to stay crisp in the pane and if the pane is widened.
const PDF_PREVIEW_WIDTH: u32 = 900;

/// Locations to look for the bundled pdfium dynamic library, in priority order.
fn pdfium_library_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Bundled next to the executable (Tauri resource) …
            candidates.push(dir.join("pdfium.dll"));
            // … or under a nested resources folder, depending on bundler.
            candidates.push(dir.join("resources").join("pdfium.dll"));
        }
    }
    // Dev builds run from target/<profile>; fall back to the checked-in copy.
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("pdfium")
            .join("pdfium.dll"),
    );
    candidates
}

/// Bind to the bundled pdfium library (falling back to a system copy). Returns
/// None when pdfium can't be found, so the preview degrades to text/metadata.
fn load_pdfium() -> Option<pdfium_render::prelude::Pdfium> {
    use pdfium_render::prelude::Pdfium;
    for candidate in pdfium_library_candidates() {
        if candidate.is_file() {
            if let Ok(bindings) = Pdfium::bind_to_library(&candidate) {
                return Some(Pdfium::new(bindings));
            }
        }
    }
    Pdfium::bind_to_system_library().ok().map(Pdfium::new)
}

/// Rasterize the first page of a PDF to RGBA8 pixels. Runs off the UI thread.
fn pdf_render_first_page(path: &Path, target_width: u32) -> Option<(Vec<u8>, u32, u32)> {
    use pdfium_render::prelude::*;
    let pdfium = load_pdfium()?;
    let document = pdfium.load_pdf_from_file(path, None).ok()?;
    let page = document.pages().first().ok()?;
    let config = PdfRenderConfig::new()
        .set_target_width(target_width as i32)
        .set_maximum_height((target_width as f32 * 1.6) as i32);
    let image = page
        .render_with_config(&config)
        .ok()?
        .as_image()
        .into_rgba8();
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    Some((image.into_raw(), w, h))
}

/// Cache key for a rasterized PDF page.
fn pdf_page_cache_key(path: &str, modified: u64, target_width: u32) -> String {
    format!("{path}|{modified}|{target_width}")
}

/// Return a cached rasterized PDF page without rendering (cheap, UI-thread safe).
fn pdf_page_cached_only(
    state: &AppState,
    path: &str,
    modified: u64,
    target_width: u32,
) -> Option<Arc<(Vec<u8>, u32, u32)>> {
    let key = pdf_page_cache_key(path, modified, target_width);
    state.pdf_page_cache.lock().ok()?.get(&key).cloned()
}

/// Return a rasterized PDF page, rendering (and caching) it on a miss. Slow on a
/// cold miss, so only call this off the UI thread.
fn pdf_page_rgba_cached(
    state: &AppState,
    path: &str,
    modified: u64,
    target_width: u32,
) -> Option<Arc<(Vec<u8>, u32, u32)>> {
    if let Some(hit) = pdf_page_cached_only(state, path, modified, target_width) {
        return Some(hit);
    }
    let arc = Arc::new(pdf_render_first_page(Path::new(path), target_width)?);
    if let Ok(mut cache) = state.pdf_page_cache.lock() {
        let key = pdf_page_cache_key(path, modified, target_width);
        cache.insert(key, arc.clone());
        // Bound the cache; evict an arbitrary entry when it grows too large.
        if cache.len() > 24 {
            if let Some(k) = cache.keys().next().cloned() {
                cache.remove(&k);
            }
        }
    }
    Some(arc)
}

/// Build a Slint image from raw RGBA8 pixel data on the UI thread.
fn rgba_to_slint_image(data: &(Vec<u8>, u32, u32)) -> slint::Image {
    let (raw, w, h) = data;
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(raw, *w, *h);
    slint::Image::from_rgba8(buf)
}

/// Natural / "Windows Explorer" string comparison: numeric runs are compared
/// as numbers so `file2.txt` sorts before `file10.txt` instead of after.
/// Both inputs should already be lowercase if you want case-insensitive ordering.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        let (ac, bc) = (ai.peek().copied(), bi.peek().copied());
        match (ac, bc) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    // Compare numeric runs as integers (skipping leading zeros for
                    // value comparison, but using the longer original-text length
                    // as a tiebreaker so "01" sorts before "1").
                    let mut na: u128 = 0;
                    let mut la = 0usize;
                    while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                        na = na
                            .saturating_mul(10)
                            .saturating_add((c as u8 - b'0') as u128);
                        la += 1;
                        ai.next();
                    }
                    let mut nb: u128 = 0;
                    let mut lb = 0usize;
                    while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                        nb = nb
                            .saturating_mul(10)
                            .saturating_add((c as u8 - b'0') as u128);
                        lb += 1;
                        bi.next();
                    }
                    match na.cmp(&nb) {
                        Ordering::Equal => match la.cmp(&lb) {
                            Ordering::Equal => continue,
                            ord => return ord,
                        },
                        ord => return ord,
                    }
                } else {
                    ai.next();
                    bi.next();
                    match ca.cmp(&cb) {
                        Ordering::Equal => continue,
                        ord => return ord,
                    }
                }
            }
        }
    }
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
        self.log_op_with_trash(kind, from, to, None);
    }

    fn log_op_batch_rename(&self, ops: Vec<RenameOp>) {
        if ops.is_empty() {
            return;
        }
        if let Ok(mut log) = self.operation_log.lock() {
            log.push(FileOp {
                kind: "batch_rename".to_string(),
                from: String::new(),
                to: None,
                trash_id: None,
                batch: Some(ops),
            });
            if log.len() > 50 {
                log.remove(0);
            }
        }
    }

    fn log_op_with_trash(
        &self,
        kind: &str,
        from: &str,
        to: Option<&str>,
        trash_id: Option<&str>,
    ) {
        if let Ok(mut log) = self.operation_log.lock() {
            log.push(FileOp {
                kind: kind.to_string(),
                from: from.to_string(),
                to: to.map(|s| s.to_string()),
                trash_id: trash_id.map(|s| s.to_string()),
                batch: None,
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

    /// Update the running totals on a queue entry so the UI can show a live
    /// progress bar during long compress / extract runs. Safe to call as
    /// often as you like - the lock contention is tiny because the queue
    /// is a short VecDeque and the call is non-blocking on failure.
    fn queue_progress(&self, id: u64, bytes_done: u64, started: Instant) {
        if let Ok(mut queue) = self.operation_queue.lock() {
            if let Some(item) = queue.iter_mut().find(|i| i.id == id) {
                item.bytes_done = bytes_done;
                let elapsed = started.elapsed().as_secs_f64();
                if elapsed > 0.05 {
                    let bps = bytes_done as f64 / elapsed;
                    item.speed_bps = bps as u64;
                    if item.bytes_total > bytes_done && bps > 1.0 {
                        let remaining = item.bytes_total - bytes_done;
                        item.eta_secs = Some((remaining as f64 / bps) as u64);
                    }
                }
            }
        }
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

    fn queue_cancel_requested(&self) -> bool {
        self.queue_cancel
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn request_queue_cancel(&self) {
        self.queue_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn clear_queue_cancel(&self) {
        self.queue_cancel
            .store(false, std::sync::atomic::Ordering::Relaxed);
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

fn user_pins_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| native_data_dir())
        .join("user_pins.json")
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
        "tif" | "tiff" => Some("image/tiff"),
        "tga" => Some("image/x-tga"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
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
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tif" | "tiff"
        | "tga" | "heic" | "heif" => "image",
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
    let path_lower = path.to_string_lossy().to_lowercase();
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
        let in_path = path_lower.contains(term.as_str());
        let in_content = content
            .as_ref()
            .map(|bytes| memchr::memmem::find(bytes, needle).is_some())
            .unwrap_or(false);
        if !in_name && !in_path && !in_content {
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

    // Collect DirEntry instead of PathBuf so we can call entry.metadata() which on Windows
    // reads from the cached WIN32_FIND_DATA returned by FindFirstFileEx - zero extra syscalls.
    let dir_entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();

    let mut entries: Vec<FileEntry> = dir_entries
        .par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            entry.metadata().ok().map(|m| path_to_entry(&path, &m))
        })
        .collect();

    sort_entries(&mut entries);
    Ok(entries)
}

fn list_directory_chunk(dir: &Path, max_entries: usize) -> Result<DirectoryPage, String> {
    if !dir.exists() {
        return Err(format!("Path does not exist: {}", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }

    let mut dir_entries: Vec<fs::DirEntry> = Vec::new();
    let mut skipped_entries = 0u32;
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        match entry {
            Ok(e) => dir_entries.push(e),
            Err(_) => skipped_entries += 1,
        }
    }
    let partial = dir_entries.len() > max_entries;
    if partial {
        // O(n) partial selection instead of sorting every name in huge folders.
        dir_entries.select_nth_unstable_by(max_entries - 1, dir_entry_name_cmp);
        dir_entries.truncate(max_entries);
    }
    dir_entries.sort_by(dir_entry_name_cmp);
    let to_process = dir_entries;
    let entries: Vec<FileEntry> = to_process
        .par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            entry.metadata().ok().map(|m| path_to_entry(&path, &m))
        })
        .collect();
    let mut entries = entries;
    sort_entries(&mut entries);
    Ok(DirectoryPage {
        entries,
        partial,
        skipped_entries,
    })
}

/// Instant folder listing from the SQLite index (visited folders). Used on
/// startup and navigation so hot/cold starts paint the last folder without
/// waiting for a full `read_dir` pass; a background refresh replaces stale rows.
fn list_directory_from_index(parent: &str) -> Option<Vec<FileEntry>> {
    let conn = open_index_connection().ok()?;
    let parent_key = cache_key_str(parent);
    let mut stmt = conn
        .prepare(
            "SELECT path, name, extension, is_dir, size, modified
             FROM files WHERE parent = ?1 OR parent = ?2",
        )
        .ok()?;
    let rows = stmt
        .query_map(params![parent_key, parent], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .ok()?;
    let mut entries = Vec::new();
    for row in rows.flatten() {
        let (path, name, ext, is_dir, size, modified) = row;
        let name_lower = name.to_lowercase();
        entries.push(FileEntry {
            path,
            name_lower,
            name,
            kind: if is_dir != 0 {
                FileKind::Directory
            } else {
                FileKind::File
            },
            size: size.max(0) as u64,
            modified: modified.max(0) as u64,
            extension: if ext.is_empty() {
                None
            } else {
                Some(ext)
            },
        });
    }
    if entries.is_empty() {
        return None;
    }
    sort_entries(&mut entries);
    Some(entries)
}

static APP_STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

static LOCAL_AI_SEMANTIC_READY: LazyLock<Mutex<Option<bool>>> =
    LazyLock::new(|| Mutex::new(None));

fn local_ai_semantic_ready_cached() -> bool {
    if let Ok(lock) = LOCAL_AI_SEMANTIC_READY.lock() {
        if let Some(v) = *lock {
            return v;
        }
    }
    let v = local_ai_semantic_ready();
    if let Ok(mut lock) = LOCAL_AI_SEMANTIC_READY.lock() {
        *lock = Some(v);
    }
    v
}

fn invalidate_local_ai_ready_cache() {
    if let Ok(mut lock) = LOCAL_AI_SEMANTIC_READY.lock() {
        *lock = None;
    }
}

fn local_ai_image_search_ready_cached() -> bool {
    local_ai_semantic_ready_cached() && crate::inference::image_classifier_available()
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
        drives.extend(enumerate_windows_drive_letters());
        drives.extend(discover_wsl_distros());
        drives.extend(discover_cloud_sync_folders());
        // Append unmapped network shares last; skip ones already represented
        // by a mapped letter so the list doesn't show the same share twice.
        let mapped_unc: std::collections::HashSet<String> = drives
            .iter()
            .filter(|d| d.kind == "network")
            .filter_map(|d| network_target_for_letter(&d.path))
            .map(|s| s.to_ascii_lowercase())
            .collect();
        for share in discover_remembered_shares() {
            let key = share.path.trim_end_matches('\\').to_ascii_lowercase();
            if !mapped_unc.contains(&key) {
                drives.push(share);
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

/// Enumerate every present drive letter using GetLogicalDrives, classify each
/// with GetDriveTypeW (fixed / removable / network / cdrom / ramdisk), and
/// attach the volume label via GetVolumeInformationW so the sidebar shows
/// "C: Windows" or "E: USB Drive" instead of a bare letter.
#[cfg(target_os = "windows")]
fn enumerate_windows_drive_letters() -> Vec<DriveInfo> {
    use windows::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows::core::PCWSTR;
    // GetDriveTypeW returns these well-known u32 codes. Inlined so we don't
    // need to enable the Win32_System_WindowsProgramming feature in the
    // windows crate (which would pull in a fair bit more codegen).
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return out;
    }
    for i in 0..26u32 {
        if mask & (1u32 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let path = format!("{}:\\", letter);
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let dtype = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
        let (kind, default_label) = match dtype {
            // Keep "local" for fixed drives so the "max" indexer keeps including them.
            x if x == DRIVE_FIXED => ("local", "Local Disk"),
            x if x == DRIVE_REMOVABLE => ("removable", "Removable Drive"),
            x if x == DRIVE_REMOTE => ("network", "Network Drive"),
            x if x == DRIVE_CDROM => ("cdrom", "CD Drive"),
            x if x == DRIVE_RAMDISK => ("ramdisk", "RAM Disk"),
            // DRIVE_UNKNOWN / DRIVE_NO_ROOT_DIR - skip.
            _ => continue,
        };
        // Volume label. Fails silently for empty CD trays and offline network
        // mounts; we fall back to the type-specific default in that case.
        let mut label = [0u16; 261];
        let mut serial = 0u32;
        let mut max_comp = 0u32;
        let mut flags = 0u32;
        let mut fs_name = [0u16; 32];
        let label_str = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                Some(&mut label),
                Some(&mut serial),
                Some(&mut max_comp),
                Some(&mut flags),
                Some(&mut fs_name),
            )
        }
        .map(|_| {
            let len = label.iter().position(|&c| c == 0).unwrap_or(label.len());
            String::from_utf16_lossy(&label[..len])
        })
        .unwrap_or_default();
        let label_part = if label_str.trim().is_empty() {
            default_label.to_string()
        } else {
            label_str
        };
        out.push(DriveInfo {
            name: format!("{}: {}", letter, label_part),
            path,
            kind: kind.to_string(),
        });
    }
    out
}

/// Read the UNC target of a mapped drive letter via WNetGetConnectionW so we
/// can dedupe a mapped-letter share against the remembered-shares list.
#[cfg(target_os = "windows")]
fn network_target_for_letter(path: &str) -> Option<String> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows::core::{PCWSTR, PWSTR};

    let letter = path.chars().next()?;
    let local: Vec<u16> = format!("{}:", letter)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let rc = unsafe {
        WNetGetConnectionW(
            PCWSTR(local.as_ptr()),
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let chars = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..chars]))
}

/// WSL distros register themselves under HKCU\Software\Microsoft\Windows
/// \CurrentVersion\Lxss\<GUID>\DistributionName. Mount via \\wsl.localhost\<name>
/// which is the modern share path WSL exposes to Windows.
#[cfg(target_os = "windows")]
fn discover_wsl_distros() -> Vec<DriveInfo> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::core::PCWSTR;

    let mut out = Vec::new();
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Lxss"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut lxss = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            None,
            KEY_READ,
            &mut lxss,
        )
    } != ERROR_SUCCESS
    {
        return out;
    }
    for idx in 0..200u32 {
        let mut name_buf = [0u16; 256];
        let mut name_len = name_buf.len() as u32;
        let rc = unsafe {
            RegEnumKeyExW(
                lxss,
                idx,
                Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            )
        };
        if rc != ERROR_SUCCESS {
            break;
        }
        // Open the subkey to read DistributionName.
        let sub_path: Vec<u16> = name_buf[..name_len as usize]
            .iter()
            .copied()
            .chain(std::iter::once(0))
            .collect();
        let mut sub = HKEY::default();
        if unsafe { RegOpenKeyExW(lxss, PCWSTR(sub_path.as_ptr()), None, KEY_READ, &mut sub) }
            != ERROR_SUCCESS
        {
            continue;
        }
        // Verify the distro is actually installed before listing it.
        // WSL leaves the registry entry around when a distro is uninstalled
        // partially, mid-install, or via `wsl --unregister` in some Windows
        // versions. State == 1 means "installed and runnable" - anything
        // else (0=uninstalled, 2=installing, 3=being-uninstalled, etc.) is
        // a ghost that File Explorer's Linux node correctly hides.
        let state_name: Vec<u16> = "State".encode_utf16().chain(std::iter::once(0)).collect();
        let mut state_val: u32 = 0;
        let mut state_size = std::mem::size_of::<u32>() as u32;
        let mut state_kind = windows::Win32::System::Registry::REG_DWORD;
        let state_q = unsafe {
            RegQueryValueExW(
                sub,
                PCWSTR(state_name.as_ptr()),
                None,
                Some(&mut state_kind),
                Some((&mut state_val) as *mut u32 as *mut u8),
                Some(&mut state_size),
            )
        };
        if state_q != ERROR_SUCCESS || state_val != 1 {
            unsafe {
                let _ = RegCloseKey(sub);
            }
            continue;
        }
        let value_name: Vec<u16> = "DistributionName"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut buf = [0u16; 256];
        let mut size = (buf.len() * 2) as u32;
        let mut kind = REG_SZ;
        let q = unsafe {
            RegQueryValueExW(
                sub,
                PCWSTR(value_name.as_ptr()),
                None,
                Some(&mut kind),
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        unsafe {
            let _ = RegCloseKey(sub);
        }
        if q != ERROR_SUCCESS {
            continue;
        }
        let chars = (size as usize / 2).saturating_sub(1);
        let dist = String::from_utf16_lossy(&buf[..chars]);
        if dist.is_empty() {
            continue;
        }
        out.push(DriveInfo {
            name: dist.clone(),
            path: format!("\\\\wsl.localhost\\{}\\", dist),
            kind: "wsl".to_string(),
        });
    }
    unsafe {
        let _ = RegCloseKey(lxss);
    }
    out
}

/// Detect cloud-sync folders by checking the well-known env vars and default
/// install paths set by each provider's desktop client. Folders that don't
/// exist on disk are skipped so we never show stale entries.
#[cfg(target_os = "windows")]
fn discover_cloud_sync_folders() -> Vec<DriveInfo> {
    // True when the folder looks like an actively-installed cloud sync target,
    // not just a stale empty directory left over from an uninstall. We check
    // for any well-known marker file from the cloud client OR for at least one
    // real child file. Without this guard, a `~/Proton Drive` directory left
    // behind by an uninstall would still show up in the sidebar.
    fn looks_active(dir: &Path) -> bool {
        // Cloud clients drop a sentinel/manifest file the first time they sync.
        // OneDrive: desktop.ini with a CLSID, or .849C9593-D756-4E56-8D6E-... settings.
        // Proton Drive: .pd-cache / .protonmeta / sync-related dotfiles.
        // Generic fallback: at least one non-hidden child file or subfolder
        // means the folder is actually in use.
        let known_markers = [
            "desktop.ini",
            ".849C9593-D756-4E56-8D6E-42412F2A707B",
            ".OneDrive",
            ".pd-cache",
            ".protonmeta",
            ".dropbox",
            ".dropbox.cache",
            ".icloud",
            "Google Drive.app",
            ".gdrive",
        ];
        for m in known_markers {
            if dir.join(m).exists() {
                return true;
            }
        }
        // Read up to 8 entries - enough to detect a non-empty folder without
        // walking large trees. Skip the marker files we already checked.
        if let Ok(rd) = std::fs::read_dir(dir) {
            for (i, entry) in rd.flatten().enumerate() {
                if i >= 8 {
                    return true;
                }
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with('.') && name_str != "desktop.ini" {
                    return true;
                }
            }
        }
        false
    }

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    let mut push = |label: &str, path_str: String| {
        if path_str.is_empty() {
            return;
        }
        let key = path_str.to_ascii_lowercase();
        if !seen.insert(key) {
            return;
        }
        let path = Path::new(&path_str);
        if path.exists() && path.is_dir() && looks_active(path) {
            out.push(DriveInfo {
                name: label.to_string(),
                path: path_str,
                kind: "cloud".to_string(),
            });
        }
    };

    // OneDrive: the desktop client exports up to three env vars depending on
    // which accounts are linked. The env var alone is not sufficient - it
    // persists across reboots even after the user signs out, so we still need
    // the looks_active sync-marker check.
    if let Ok(p) = std::env::var("OneDrive") {
        push("OneDrive", p);
    }
    if let Ok(p) = std::env::var("OneDriveConsumer") {
        push("OneDrive Personal", p);
    }
    if let Ok(p) = std::env::var("OneDriveCommercial") {
        push("OneDrive for Business", p);
    }

    // Defaults under the user home directory used by the other major clients.
    if let Some(home) = dirs::home_dir() {
        let candidates = [
            ("Proton Drive", "Proton Drive"),
            ("Proton Drive", "ProtonDrive"),
            ("Google Drive", "Google Drive"),
            ("Google Drive", "My Drive"),
            ("Dropbox", "Dropbox"),
            ("iCloud Drive", "iCloudDrive"),
            ("Box", "Box"),
            ("Sync", "Sync"),
            ("MEGA", "MEGA"),
        ];
        for (label, dirname) in candidates {
            push(label, home.join(dirname).to_string_lossy().into_owned());
        }
    }

    // Dropbox info.json holds the real path for users who installed to a
    // non-default location.
    if let Ok(appdata) = std::env::var("APPDATA") {
        let info = Path::new(&appdata).join("Dropbox").join("info.json");
        if let Ok(text) = std::fs::read_to_string(&info)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        {
            for key in ["personal", "business"] {
                if let Some(p) = v
                    .get(key)
                    .and_then(|o| o.get("path"))
                    .and_then(|s| s.as_str())
                {
                    let label = if key == "business" {
                        "Dropbox (Business)"
                    } else {
                        "Dropbox"
                    };
                    push(label, p.to_string());
                }
            }
        }
    }
    out
}

/// Enumerate remembered (persisted) network shares via WNetEnumResourceW so
/// shares the user once mapped show up even when not currently letter-mapped.
#[cfg(target_os = "windows")]
fn discover_remembered_shares() -> Vec<DriveInfo> {
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::WNet::{
        NETRESOURCEW, RESOURCE_REMEMBERED, RESOURCETYPE_DISK, RESOURCEUSAGE_CONNECTABLE,
        WNetCloseEnum, WNetEnumResourceW, WNetOpenEnumW,
    };

    let mut out = Vec::new();
    let mut handle: windows::Win32::Foundation::HANDLE =
        windows::Win32::Foundation::HANDLE::default();
    let open = unsafe {
        WNetOpenEnumW(
            RESOURCE_REMEMBERED,
            RESOURCETYPE_DISK,
            RESOURCEUSAGE_CONNECTABLE,
            None,
            &mut handle,
        )
    };
    if open != ERROR_SUCCESS {
        return out;
    }
    // 16KB scratch is plenty for the typical handful of remembered shares.
    let buf_bytes = 16 * 1024usize;
    let mut buf = vec![0u8; buf_bytes];
    loop {
        let mut count: u32 = u32::MAX;
        let mut size: u32 = buf_bytes as u32;
        let rc =
            unsafe { WNetEnumResourceW(handle, &mut count, buf.as_mut_ptr().cast(), &mut size) };
        if rc == ERROR_NO_MORE_ITEMS || count == 0 {
            break;
        }
        if rc != ERROR_SUCCESS {
            break;
        }
        let entries = unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const NETRESOURCEW, count as usize)
        };
        for e in entries {
            let remote = unsafe { wide_to_string(e.lpRemoteName) };
            if remote.is_empty() || !remote.starts_with("\\\\") {
                continue;
            }
            let comment = unsafe { wide_to_string(e.lpComment) };
            let label = if comment.is_empty() {
                remote.clone()
            } else {
                comment
            };
            let path = if remote.ends_with('\\') {
                remote
            } else {
                format!("{}\\", remote)
            };
            out.push(DriveInfo {
                name: label,
                path,
                kind: "network".to_string(),
            });
        }
    }
    unsafe {
        let _ = WNetCloseEnum(handle);
    }
    out
}

#[cfg(target_os = "windows")]
unsafe fn wide_to_string(p: windows::core::PWSTR) -> String {
    if p.0.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while *p.0.add(len) != 0 && len < 4096 {
            len += 1;
        }
    }
    let slice = unsafe { std::slice::from_raw_parts(p.0, len) };
    String::from_utf16_lossy(slice)
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
            .no_window()
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
    use windows::Win32::System::RestartManager::{
        CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
        RmStartSession,
    };
    use windows::core::{PCWSTR, PWSTR};

    let mut session = 0_u32;
    let mut key = vec![0_u16; CCH_RM_SESSION_KEY as usize + 1];
    let start = unsafe { RmStartSession(&mut session, Some(0), PWSTR(key.as_mut_ptr())) };
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
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Shell::SEE_MASK_INVOKEIDLIST;
        use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows::core::PCWSTR;

        let path_wide: Vec<u16> = OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let verb_wide: Vec<u16> = "properties\0".encode_utf16().collect();

        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            hwnd: HWND(std::ptr::null_mut()),
            lpVerb: PCWSTR(verb_wide.as_ptr()),
            lpFile: PCWSTR(path_wide.as_ptr()),
            lpParameters: PCWSTR::null(),
            lpDirectory: PCWSTR::null(),
            nShow: 1,
            ..Default::default()
        };

        unsafe { ShellExecuteExW(&mut info).map_err(|e| e.to_string()) }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err("Native Properties is only available on Windows.".to_string())
    }
}

// -- Windows-specific shell helpers ------------------------------------------

/// Run a file elevated via ShellExecuteExW "runas" - triggers UAC immediately, no PowerShell spawn.
fn run_as_admin(path: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows::core::PCWSTR;

        let path_wide: Vec<u16> = OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let verb_wide: Vec<u16> = "runas\0".encode_utf16().collect();

        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: Default::default(),
            hwnd: HWND(std::ptr::null_mut()),
            lpVerb: PCWSTR(verb_wide.as_ptr()),
            lpFile: PCWSTR(path_wide.as_ptr()),
            lpParameters: PCWSTR::null(),
            lpDirectory: PCWSTR::null(),
            nShow: 1,
            ..Default::default()
        };

        unsafe {
            ShellExecuteExW(&mut info).map_err(|e| format!("Run as Administrator failed: {e}"))
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err("Run as Administrator is only available on Windows.".to_string())
    }
}

/// List VSS shadow copies for the drive that contains `path`.
/// Returns human-readable lines; empty vec means none found or vssadmin not available.
fn list_previous_versions(path: &str) -> Vec<String> {
    let drive = Path::new(path)
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default();
    if drive.is_empty() {
        return Vec::new();
    }
    let out = ProcessCommand::new("vssadmin")
        .args(["list", "shadows", &format!("/for={}", drive)])
        .no_window()
        .output()
        .ok();
    let stdout = match out {
        Some(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        None => return Vec::new(),
    };
    let mut versions = Vec::new();
    let mut current_time = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Creation Time:") {
            current_time = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("Shadow Copy Volume:") {
            if !current_time.is_empty() {
                versions.push(format!("{} - {}", current_time, rest.trim()));
                current_time.clear();
            }
        }
    }
    versions
}

/// Open Explorer with the file selected so the user can access the full shell context menu.
fn open_more_options(path: &str, _ui: &MainWindow) -> Result<(), String> {
    reveal_in_folder(path.to_string())
}

#[cfg(target_os = "windows")]
fn main_window_hwnd(ui: Option<&MainWindow>) -> windows::Win32::Foundation::HWND {
    use i_slint_backend_winit::WinitWindowAccessor;
    use i_slint_backend_winit::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;

    if let Some(ui) = ui {
        let mut found = HWND(std::ptr::null_mut());
        ui.window().with_winit_window(|window| {
            if let Ok(handle) = window.window_handle() {
                if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                    found = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
                }
            }
        });
        if !found.0.is_null() {
            return found;
        }
    }
    find_pathfinder_hwnd().unwrap_or(HWND(std::ptr::null_mut()))
}

fn open_with_dialog(path: &str, ui: Option<&MainWindow>) -> Result<(), String> {
    if Path::new(path).is_dir() {
        return Err("Open With applies to files, not folders.".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::UI::Shell::{SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        use windows::core::PCWSTR;

        let path_wide: Vec<u16> = OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let verb_wide: Vec<u16> = "openas\0".encode_utf16().collect();
        let mut hwnd = main_window_hwnd(ui);
        if hwnd.0.is_null() {
            hwnd = unsafe { GetForegroundWindow() };
        }
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            hwnd,
            lpVerb: PCWSTR(verb_wide.as_ptr()),
            lpFile: PCWSTR(path_wide.as_ptr()),
            lpParameters: PCWSTR::null(),
            lpDirectory: PCWSTR::null(),
            nShow: 1,
            ..Default::default()
        };

        unsafe {
            if ShellExecuteExW(&mut info).is_ok() && info.hInstApp.0 as isize > 32 {
                return Ok(());
            }
        }

        ProcessCommand::new("rundll32.exe")
            .args(["shell32.dll,OpenAs_RunDLL", path])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (path, ui);
        Err("Open With is only available on Windows.".to_string())
    }
}

fn defender_scan_path(path: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        ProcessCommand::new("powershell")
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg("Start-MpScan -ScanType CustomScan -ScanPath $args[0]")
            .arg(path)
            .no_window()
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err("Microsoft Defender scan is only available on Windows.".to_string())
    }
}

fn shell_verb_summary(path: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        match windows_integration::get_context_menu_actions(path) {
            Ok(actions) if !actions.is_empty() => actions
                .iter()
                .map(|action| {
                    format!(
                        "{}. {}{}",
                        action.id,
                        action.name,
                        action
                            .help_text
                            .as_ref()
                            .map(|text| format!(" - {text}"))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Ok(_) => "No shell verbs were reported for this item.".to_string(),
            Err(error) => error,
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        "Shell verbs are Windows-specific.".to_string()
    }
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
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
        };

        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let byte_count = wide.len() * 2;

        unsafe {
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_count).map_err(|e| e.to_string())?;
            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(hmem));
                return Err("GlobalLock failed".to_string());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hmem);

            if let Err(e) = OpenClipboard(None) {
                let _ = GlobalFree(Some(hmem));
                return Err(e.to_string());
            }
            if let Err(e) = EmptyClipboard() {
                let _ = CloseClipboard();
                let _ = GlobalFree(Some(hmem));
                return Err(e.to_string());
            }
            const CF_UNICODETEXT: u32 = 13;
            if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hmem.0))) {
                let _ = CloseClipboard();
                let _ = GlobalFree(Some(hmem));
                return Err(e.to_string());
            }
            CloseClipboard().map_err(|e| e.to_string())?;
            Ok(())
        }
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
        copy_dir_recursive(&state, &src, &dst)
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
        copy_dir_recursive(&state, &src, &dst)?;
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
        let token = state.search_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (entries, _) = hybrid_search_background(&state, &path, &query, max, token);
        if state.search_generation.load(Ordering::SeqCst) == token {
            return Ok(entries);
        }
        return Ok(Vec::new());
    }

    let token = state.search_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let mut output = live_search_scan(&state, &path, &query, max, token);

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

    // PowerShell `-Command` with a string form does NOT forward trailing args
    // into `$args`. The previous version of this function tried to read
    // `$args[0]/[1]/[2]` and the script ran with empty parameters every time,
    // so Windows Search was effectively offline for every install. We
    // interpolate the values into the script body the same way the auto
    // updater does, with single quotes escaped.
    let scope_path = path.replace('\'', "''");
    let q_escaped = cleaned
        .replace('\'', "''")
        .replace('[', "[[]")
        .replace('%', "[%]")
        .replace('_', "[_]");
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
$connection = New-Object -ComObject ADODB.Connection
$recordset = New-Object -ComObject ADODB.Recordset
$connection.Open("Provider=Search.CollatorDSO;Extended Properties='Application=Windows';")
$scopeItem = Get-Item -LiteralPath '{scope}'
$scopeUri = $scopeItem.FullName.Replace('\', '/')
if ($scopeUri -notmatch '/$') {{ $scopeUri += '/' }}
$scopeUri = 'file:///' + $scopeUri
$sql = "SELECT TOP {max} System.ItemPathDisplay FROM SYSTEMINDEX WHERE SCOPE='$scopeUri' AND System.ItemNameDisplay LIKE '%{q}%'"
$recordset.Open($sql, $connection)
$paths = New-Object System.Collections.Generic.List[string]
while (-not $recordset.EOF) {{
  $value = $recordset.Fields.Item('System.ItemPathDisplay').Value
  if ($value) {{ $paths.Add([string]$value) }}
  $recordset.MoveNext()
}}
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
ConvertTo-Json -InputObject @($paths) -Compress
"#,
        scope = scope_path,
        max = max_results,
        q = q_escaped
    );

    let output = ProcessCommand::new("powershell")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(&script)
        .no_window()
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
    let _ = upsert_index_entries(&entries);
    sort_entries(&mut entries);
    Ok(entries)
}

fn merge_search_entries(target: &mut Vec<FileEntry>, incoming: Vec<FileEntry>, max: usize) {
    let mut seen: HashSet<String> = target
        .iter()
        .map(|entry| cache_key_str(&entry.path))
        .collect();
    for entry in incoming {
        if target.len() >= max {
            break;
        }
        if seen.insert(cache_key_str(&entry.path)) {
            target.push(entry);
        }
    }
}

/// Directories skipped during whole-drive live search (noise + extreme depth).
fn should_skip_search_walk_dir(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    STORAGE_SKIP_DIRS
        .iter()
        .any(|skip| lower == skip.to_ascii_lowercase())
        || matches!(
            lower.as_str(),
            "winsxs" | "installer" | "servicing" | "assembly" | "packages"
        )
}

/// Whole-drive search: walk each top-level folder in parallel with a per-tree
/// match budget so Users\\AppData\\... is not starved by shallow Program Files hits.
fn live_search_drive_deep(
    state: &AppState,
    root: &Path,
    query: &str,
    max: usize,
    token: u64,
) -> Vec<FileEntry> {
    let parsed = Arc::new(parse_query(query));
    let generation = state.search_generation.clone();
    let mut work_units: Vec<PathBuf> = fs::read_dir(root)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    work_units.sort_by_key(|p| {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "users" => 0u8,
            "programdata" => 1,
            _ => 2,
        }
    });
    let per_tree = max.saturating_div(work_units.len().max(1)).max(512);
    let chunks: Vec<Vec<FileEntry>> = work_units
        .into_par_iter()
        .map(|subtree| {
            let parsed = Arc::clone(&parsed);
            let generation = generation.clone();
            WalkDir::new(subtree)
                .follow_links(false)
                .into_iter()
                .filter_entry({
                    let generation = generation.clone();
                    move |e| {
                        if generation.load(Ordering::Relaxed) != token {
                            return false;
                        }
                        if e.file_type().is_dir() {
                            let name = e.file_name().to_string_lossy();
                            return !should_skip_search_walk_dir(&name);
                        }
                        true
                    }
                })
                .filter_map(move |e| {
                    if generation.load(Ordering::Relaxed) != token {
                        return None;
                    }
                    let e = e.ok()?;
                    let path = e.path();
                    let metadata = e.metadata().ok()?;
                    if matches_query(path, &metadata, &parsed) {
                        Some(path_to_entry(path, &metadata))
                    } else {
                        None
                    }
                })
                .take(per_tree)
                .collect()
        })
        .collect();
    let mut output: Vec<FileEntry> = chunks.into_iter().flatten().take(max).collect();
    sort_entries(&mut output);
    output
}

fn live_search_scan(
    state: &AppState,
    root: &str,
    query: &str,
    max: usize,
    token: u64,
) -> Vec<FileEntry> {
    let dir = PathBuf::from(root);
    if is_filesystem_root(&dir) {
        return live_search_drive_deep(state, &dir, query, max, token);
    }
    let parsed = Arc::new(parse_query(query));
    let generation = state.search_generation.clone();
    let is_drive_root = is_filesystem_root(&dir);
    let mut work_units: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    if is_drive_root {
        // Search-all really means the whole drive. Keep every root child and
        // only reorder common user content to surface likely hits sooner.
        work_units.sort_by_key(|p| {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            if name == "users" { 0u8 } else { 1u8 }
        });
    }

    let mut output: Vec<FileEntry> = work_units
        .into_par_iter()
        .flat_map_iter(|subtree| {
            let parsed = Arc::clone(&parsed);
            let generation_for_descent = Arc::clone(&generation);
            let generation_for_match = Arc::clone(&generation);
            WalkDir::new(subtree)
                .follow_links(false)
                .into_iter()
                .filter_entry(move |_| generation_for_descent.load(Ordering::Relaxed) == token)
                .filter_map(Result::ok)
                .filter_map(move |entry| {
                    if generation_for_match.load(Ordering::Relaxed) != token {
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
    sort_entries(&mut output);
    output
}

fn hybrid_search_background(
    state: &AppState,
    root: &str,
    query: &str,
    max: usize,
    token: u64,
) -> (Vec<FileEntry>, String) {
    let mut results = index_search(root, query, max).unwrap_or_default();
    let mut source = if results.is_empty() {
        "live scan".to_string()
    } else {
        "Pathfinder index".to_string()
    };

    if state.search_generation.load(Ordering::Relaxed) != token {
        return (Vec::new(), "cancelled".to_string());
    }

    if let Ok(windows_results) = windows_index_search_impl(query, root, max) {
        if !windows_results.is_empty() {
            let _ = upsert_index_entries(&windows_results);
            merge_search_entries(&mut results, windows_results, max);
            source = "Pathfinder index + Windows Search".to_string();
        }
    }

    if state.search_generation.load(Ordering::Relaxed) != token {
        return (Vec::new(), "cancelled".to_string());
    }

    if results.len() < max {
        let live = live_search_scan(state, root, query, max.saturating_sub(results.len()), token);
        if !live.is_empty() {
            let _ = upsert_index_entries(&live);
            merge_search_entries(&mut results, live, max);
            source = if source.contains("Windows") {
                "Pathfinder index + Windows Search + live scan".to_string()
            } else {
                "Pathfinder index + live scan".to_string()
            };
        }
    }

    sort_entries(&mut results);
    (results, source)
}

fn publish_search_result(
    pending: &Arc<Mutex<Option<NativeSearchResult>>>,
    ready: &Arc<AtomicBool>,
    path: &str,
    query: &str,
    entries: Vec<FileEntry>,
    source: String,
    partial: bool,
) {
    if let Ok(mut lock) = pending.lock() {
        *lock = Some(NativeSearchResult {
            path: path.to_string(),
            query: query.to_string(),
            entries,
            source,
            partial,
        });
    }
    ready.store(true, Ordering::Release);
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

fn find_7z() -> Option<PathBuf> {
    if ProcessCommand::new("7z")
        .arg("i")
        .no_window()
        .output()
        .is_ok()
    {
        return Some(PathBuf::from("7z"));
    }
    #[cfg(target_os = "windows")]
    {
        for candidate in [
            r"C:\Program Files\7-Zip\7z.exe",
            r"C:\Program Files (x86)\7-Zip\7z.exe",
        ] {
            let path = PathBuf::from(candidate);
            if path.exists() {
                return Some(path);
            }
        }
    }
    None
}

fn list_zip_archive(path: &Path, max_items: usize) -> Result<Vec<ArchiveEntry>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    for i in 0..archive.len().min(max_items) {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        entries.push(ArchiveEntry {
            name: entry.name().to_string(),
            size: entry.size(),
            is_dir: entry.is_dir(),
            encrypted: entry.encrypted(),
        });
    }
    Ok(entries)
}

fn list_7z_archive(path: &Path, max_items: usize) -> Result<Vec<ArchiveEntry>, String> {
    let seven_zip = find_7z().ok_or_else(|| "7-Zip was not found on this system.".to_string())?;
    let output = ProcessCommand::new(seven_zip)
        .arg("l")
        .arg("-slt")
        .arg(path)
        .no_window()
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    let mut entries = Vec::new();
    let mut name = String::new();
    let mut size = 0_u64;
    let mut is_dir = false;
    let mut encrypted = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(value) = line.strip_prefix("Path = ") {
            if !name.is_empty() && entries.len() < max_items {
                entries.push(ArchiveEntry {
                    name: std::mem::take(&mut name),
                    size,
                    is_dir,
                    encrypted,
                });
            }
            name = value.to_string();
            size = 0;
            is_dir = false;
            encrypted = false;
        } else if let Some(value) = line.strip_prefix("Size = ") {
            size = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("Folder = ") {
            is_dir = value.trim() == "+";
        } else if let Some(value) = line.strip_prefix("Encrypted = ") {
            encrypted = value.trim() == "+";
        }
    }
    if !name.is_empty() && entries.len() < max_items {
        entries.push(ArchiveEntry {
            name,
            size,
            is_dir,
            encrypted,
        });
    }
    entries.retain(|entry| entry.name != path.to_string_lossy());
    Ok(entries)
}

fn list_archive_entries(path: &Path, max_items: usize) -> Result<Vec<ArchiveEntry>, String> {
    let ext = extension(path);
    if ext == "zip" {
        list_zip_archive(path, max_items)
    } else {
        list_7z_archive(path, max_items)
    }
}

fn normalize_archive_prefix(value: &str) -> String {
    value
        .replace('\\', "/")
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn archive_virtual_path(archive_path: &str, prefix: &str) -> String {
    let encoded = general_purpose::URL_SAFE_NO_PAD.encode(archive_path.as_bytes());
    let prefix = normalize_archive_prefix(prefix);
    if prefix.is_empty() {
        format!("{ARCHIVE_SCHEME}{encoded}!/")
    } else {
        format!("{ARCHIVE_SCHEME}{encoded}!/{prefix}")
    }
}

fn parse_archive_virtual_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix(ARCHIVE_SCHEME)?;
    let (encoded, prefix) = rest.split_once("!/")?;
    let bytes = general_purpose::URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let archive_path = String::from_utf8(bytes).ok()?;
    Some((archive_path, normalize_archive_prefix(prefix)))
}

fn archive_display_path(archive_path: &str, prefix: &str) -> String {
    let prefix = normalize_archive_prefix(prefix);
    if prefix.is_empty() {
        format!("{archive_path}!/")
    } else {
        format!("{archive_path}!/{prefix}")
    }
}

fn archive_parent_prefix(prefix: &str) -> String {
    let prefix = normalize_archive_prefix(prefix);
    prefix
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

fn archive_breadcrumbs(archive_path: &str, prefix: &str) -> Vec<ChoiceItem> {
    let mut crumbs = vec![ChoiceItem {
        id: ss(archive_virtual_path(archive_path, "")),
        label: ss(Path::new(archive_path)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| archive_path.to_string())),
        description: ss("Archive root"),
        color: rgba_u8(0, 0, 0, 0.0),
    }];

    let mut accumulated = String::new();
    for part in normalize_archive_prefix(prefix).split('/') {
        if part.is_empty() {
            continue;
        }
        if !accumulated.is_empty() {
            accumulated.push('/');
        }
        accumulated.push_str(part);
        crumbs.push(ChoiceItem {
            id: ss(archive_virtual_path(archive_path, &accumulated)),
            label: ss(part),
            description: ss("Archive folder"),
            color: rgba_u8(0, 0, 0, 0.0),
        });
    }
    crumbs
}

fn list_archive_virtual_dir(archive_path: &str, prefix: &str) -> Result<Vec<FileEntry>, String> {
    let archive = Path::new(archive_path);
    let modified = fs::metadata(archive)
        .map(|m| unix_secs(m.modified()))
        .unwrap_or(0);
    let prefix = normalize_archive_prefix(prefix);
    let prefix_with_slash = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    let mut children: HashMap<String, FileEntry> = HashMap::new();

    for entry in list_archive_entries(archive, 100_000)? {
        let full = normalize_archive_prefix(&entry.name);
        if full.is_empty() {
            continue;
        }
        if !prefix.is_empty() && full != prefix && !full.starts_with(&prefix_with_slash) {
            continue;
        }
        let rest = if prefix.is_empty() {
            full.as_str()
        } else {
            full.strip_prefix(&prefix_with_slash).unwrap_or_default()
        };
        if rest.is_empty() {
            continue;
        }

        let (name, is_nested) = rest
            .split_once('/')
            .map(|(name, _)| (name.to_string(), true))
            .unwrap_or_else(|| (rest.to_string(), false));
        if name.is_empty() {
            continue;
        }
        let child_prefix = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = if is_nested || entry.is_dir {
            FileKind::Directory
        } else {
            FileKind::File
        };
        let existing_is_dir = children
            .get(&name)
            .map(|item| item.kind == FileKind::Directory)
            .unwrap_or(false);
        if existing_is_dir && kind != FileKind::Directory {
            continue;
        }

        let extension = if kind == FileKind::File {
            Path::new(&name)
                .extension()
                .map(|ext| ext.to_string_lossy().to_lowercase())
        } else {
            None
        };
        children.insert(
            name.clone(),
            FileEntry {
                path: archive_virtual_path(archive_path, &child_prefix),
                name_lower: name.to_lowercase(),
                name,
                kind,
                size: if is_nested { 0 } else { entry.size },
                modified,
                extension,
            },
        );
    }

    let mut entries: Vec<FileEntry> = children.into_values().collect();
    sort_entries(&mut entries);
    Ok(entries)
}

fn archive_listing_preview(path: &Path, max_items: usize) -> Result<String, String> {
    let entries = list_archive_entries(path, max_items)?;
    let mut lines = entries
        .iter()
        .map(|entry| {
            format!(
                "{}{}  {}",
                if entry.is_dir {
                    "<dir>".to_string()
                } else {
                    format_size_short(entry.size)
                },
                if entry.encrypted { " locked" } else { "" },
                entry.name
            )
        })
        .collect::<Vec<_>>();
    if entries.len() >= max_items {
        lines.push(format!("... showing first {max_items} entries"));
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
        let limit = max_bytes.unwrap_or(64 * 1024).min(64 * 1024);
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
        // Empty body (rather than duplicated metadata) when no text could be
        // extracted, so the preview pane can show a clear "no extractable text"
        // message instead of repeating the meta block below it.
        let text = pdf_first_page_preview(path_buf).unwrap_or_default();
        return Ok(PreviewContent {
            kind: "pdf".to_string(),
            mime: Some("application/pdf".to_string()),
            text: Some(text),
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
        let kind_label = if matches!(
            ext.as_str(),
            "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a"
        ) {
            "Audio"
        } else {
            "Video"
        };
        return Ok(PreviewContent {
            kind: "media".to_string(),
            mime: mime_for_ext(&ext).map(|s| s.to_string()),
            text: Some(generic_metadata_preview(path_buf, metadata, kind_label)),
            data_url: None,
            truncated: false,
        });
    }

    if is_image_ext(&ext) {
        let mime = mime_for_ext(&ext).unwrap_or("image/*");
        // Inline embedding limit: 8 MB. Larger images get a metadata preview with dimensions
        // read from the image header only - no full decode needed.
        const INLINE_MAX: u64 = 8 * 1024 * 1024;
        let can_inline = is_inline_preview_image_ext(&ext);
        let can_decode = is_thumbnail_image_ext(&ext);
        if !can_inline || metadata.len() > INLINE_MAX {
            let dims = if can_decode {
                image::ImageReader::open(path_buf)
                    .ok()
                    .and_then(|r| r.with_guessed_format().ok())
                    .and_then(|r| r.into_dimensions().ok())
            } else {
                None
            };
            let text = match dims {
                Some((w, h)) => format!(
                    "Kind: Image\nExtension: {}\nDimensions: {} x {}\nSize: {}\nModified: {}\nPath: {}",
                    ext.to_uppercase(),
                    w,
                    h,
                    format_size_short(metadata.len()),
                    format_modified(unix_secs(metadata.modified())),
                    path_buf.display()
                ),
                None => {
                    let mut text = generic_metadata_preview(path_buf, metadata, "Image");
                    if !can_decode {
                        text.push_str("\nPreview: metadata only for this image container");
                    }
                    text
                }
            };
            return Ok(PreviewContent {
                kind: if can_inline {
                    "image-too-large".to_string()
                } else {
                    "image-metadata".to_string()
                },
                mime: Some(mime.to_string()),
                text: Some(text),
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
        let limit = max_bytes.unwrap_or(64 * 1024).min(64 * 1024);
        let mut file = File::open(path_buf).map_err(|e| e.to_string())?;
        let mut bytes = Vec::with_capacity(limit + 1);
        std::io::Read::by_ref(&mut file)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);

        // A null byte in the first 512 bytes means binary despite the extension.
        if memchr::memchr(0, &bytes[..bytes.len().min(512)]).is_some() {
            return Ok(PreviewContent {
                kind: "binary".to_string(),
                mime: None,
                text: Some(generic_metadata_preview(path_buf, metadata, "Binary file")),
                data_url: None,
                truncated: false,
            });
        }

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
    if ACTIVE_HEAVY_OPS.fetch_add(1, Ordering::SeqCst) >= MAX_HEAVY_OPS {
        ACTIVE_HEAVY_OPS.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    let app_state = state.inner().clone();
    std::thread::spawn(move || {
        let _guard = HeavyOpGuard;
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
                            // Use debounced indexing to avoid excessive database operations
                            // when rapid file system events occur (e.g., after delete/recycle bin).
                            schedule_index_directory_debounced(
                                &callback_state,
                                parent_string,
                                entries,
                            );
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

fn detect_npu_names() -> Vec<String> {
    // SetupDi class enumeration is roughly 1000x faster than spawning PowerShell
    // with Get-PnpDevice. The same ComputeAccelerator class GUID, but no shell.
    gpu_detect::detect_npus()
}

fn gpu_capability_summary() -> String {
    if !cfg!(target_os = "windows") {
        return "GPU: detailed adapter listing is only implemented on Windows.".to_string();
    }
    let inv = gpu_detect::detect_gpus();
    if inv.adapters.is_empty() {
        return "GPU: Windows did not return any DXGI adapters.".to_string();
    }
    let discrete: Vec<String> = inv
        .discrete()
        .iter()
        .map(|a| format!("{} ({} MB VRAM)", a.name, a.dedicated_video_mb))
        .collect();
    let integrated: Vec<String> = inv.integrated().iter().map(|a| a.name.clone()).collect();
    if discrete.is_empty() && integrated.is_empty() {
        return "GPU: only software / remote DXGI adapters detected.".to_string();
    }
    if discrete.is_empty() {
        format!(
            "dGPU: none detected. Integrated GPU only: {}",
            integrated.join(" | ")
        )
    } else if integrated.is_empty() {
        format!(
            "dGPU detected: {} (used for DirectML acceleration)",
            discrete.join(" | ")
        )
    } else {
        format!(
            "dGPU detected: {} (used for DirectML) | Integrated: {}",
            discrete.join(" | "),
            integrated.join(" | ")
        )
    }
}

#[cfg(target_os = "windows")]
fn process_memory_stats() -> Option<(u64, u64)> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size).is_ok() {
            let working_set_mb = counters.WorkingSetSize as u64 / (1024 * 1024);
            let private_mb = counters.PagefileUsage as u64 / (1024 * 1024);
            Some((working_set_mb, private_mb))
        } else {
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn process_working_set_mb() -> Option<u64> {
    process_memory_stats().map(|(ws, _)| ws)
}

#[cfg(not(target_os = "windows"))]
fn process_working_set_mb() -> Option<u64> {
    None
}

#[cfg(not(target_os = "windows"))]
fn process_memory_stats() -> Option<(u64, u64)> {
    None
}

fn compute_ai_capabilities() -> AiCapabilities {
    let devices = detect_npu_names();
    let npu_hardware_found = !devices.is_empty();
    let env_runtime = std::env::var("PATHFINDER_LOCAL_AI_RUNTIME")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let onnx_installed = local_ai::onnx_runtime_installed();
    let models_ready = local_ai::core_models_installed();
    let manifest_installed = matches!(
        local_ai::read_manifest().state,
        local_ai::InstallState::Installed
    );
    // ORT DLL beside models, explicit env override, or completed installer manifest.
    let runtime_configured = onnx_installed || env_runtime || manifest_installed;
    // NPU is "available" for inference only when hardware + runtime + models line up.
    let npu_enabled = npu_hardware_found && runtime_configured && models_ready;
    let device_name = if npu_hardware_found {
        devices.join(", ")
    } else {
        "CPU Fallback".to_string()
    };
    let acceleration_kind = if npu_enabled { "NPU" } else { "CPU" }.to_string();
    let ort = crate::inference::ort_runtime_line();
    let reason = if npu_enabled {
        format!(
            "NPU acceleration: {}. Local AI on-device (DirectML EP). [{}]",
            device_name, ort
        )
    } else if npu_hardware_found && runtime_configured && !models_ready {
        format!(
            "NPU detected ({}). Install embedding models from Settings -> Local AI to enable acceleration. [{}]",
            device_name, ort
        )
    } else if npu_hardware_found && !runtime_configured {
        format!(
            "NPU detected ({}). Install Local AI (ONNX Runtime + models) from Settings to enable. [{}]",
            device_name, ort
        )
    } else {
        format!("No NPU detected - CPU inference on-device. [{}]", ort)
    };
    let gpu_summary = gpu_capability_summary();

    AiCapabilities {
        npu_available: npu_enabled,
        semantic_search: true,
        automatic_summaries: true,
        image_classification: true,
        local_embeddings: true,
        device_name,
        acceleration_kind,
        runtime_configured,
        reason,
        gpu_summary,
    }
}

fn local_ai_semantic_ready() -> bool {
    if !local_ai::core_models_installed() {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        let env_runtime = std::env::var("PATHFINDER_LOCAL_AI_RUNTIME")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let manifest_installed = matches!(
            local_ai::read_manifest().state,
            local_ai::InstallState::Installed
        );
        local_ai::onnx_runtime_installed() || env_runtime || manifest_installed
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

fn local_ai_image_search_ready() -> bool {
    local_ai_semantic_ready() && crate::inference::image_classifier_available()
}

fn ai_status_label(capabilities: &AiCapabilities) -> &'static str {
    if capabilities.npu_available && capabilities.acceleration_kind == "NPU" {
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
    let _ = capabilities;
    search_files(state, query, path, max_results, Some(true))
}

#[tauri::command]
fn ai_summarize_file(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let capabilities = get_ai_capabilities(state.clone());
    let _ = capabilities;

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
    get_user_pins(app)
        .into_iter()
        .map(|pin| Bookmark {
            name: pin.name,
            path: pin.path,
        })
        .collect()
}

#[tauri::command]
fn save_bookmarks(app: AppHandle, bookmarks: Vec<Bookmark>) -> Result<(), String> {
    let pins = bookmarks
        .into_iter()
        .map(bookmark_to_pin)
        .collect::<Vec<_>>();
    save_user_pins(app, pins)
}

#[tauri::command]
fn get_user_pins(app: AppHandle) -> Vec<UserPin> {
    let path = user_pins_path(&app);
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(pins) = serde_json::from_str::<Vec<UserPin>>(&data) {
            return pins
                .into_iter()
                .filter(|pin| Path::new(&pin.path).exists())
                .collect();
        }
    }

    native_user_pins()
}

#[tauri::command]
fn save_user_pins(app: AppHandle, pins: Vec<UserPin>) -> Result<(), String> {
    let path = user_pins_path(&app);
    write_json_file(&path, &pins)?;
    let _ = save_native_user_pins(&pins);
    Ok(())
}

#[tauri::command]
fn add_user_pin(
    app: AppHandle,
    path: String,
    name: Option<String>,
) -> Result<Vec<UserPin>, String> {
    let path_buf = PathBuf::from(&path);
    if !path_buf.exists() {
        return Err(format!("Path does not exist: {path}"));
    }
    let normalized = path_buf.to_string_lossy().to_string();
    let mut pins = get_user_pins(app.clone());
    pins.retain(|pin| !same_path_string(&pin.path, &normalized));
    pins.insert(
        0,
        UserPin {
            name: pin_name_for_path(&path_buf, name),
            path: normalized,
            kind: if path_buf.is_dir() { "folder" } else { "file" }.to_string(),
            pinned_at: now_unix_secs(),
        },
    );
    save_user_pins(app, pins.clone())?;
    Ok(pins)
}

#[tauri::command]
fn remove_user_pin(app: AppHandle, path: String) -> Result<Vec<UserPin>, String> {
    let mut pins = get_user_pins(app.clone());
    pins.retain(|pin| !same_path_string(&pin.path, &path));
    save_user_pins(app, pins.clone())?;
    Ok(pins)
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

// ----- helpers -----

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

// ----- checksum -----

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

// ----- terminal -----

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

// ----- file notes -----

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

// ----- batch rename -----

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

// ----- git status -----

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
        .no_window()
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

// ----- image info -----

#[tauri::command]
fn get_image_info(path: String) -> Result<ImageInfo, String> {
    let ext = extension(Path::new(&path));
    if is_image_ext(&ext) && !is_thumbnail_image_ext(&ext) {
        return Ok(ImageInfo {
            width: 0,
            height: 0,
            format: ext.to_uppercase(),
        });
    }
    // Read only the image header for dimensions - avoids decoding the full file.
    let (width, height) = image::ImageReader::open(&path)
        .ok()
        .and_then(|r| r.with_guessed_format().ok())
        .and_then(|r| r.into_dimensions().ok())
        .ok_or_else(|| "Could not read image dimensions".to_string())?;
    Ok(ImageInfo {
        width,
        height,
        format: ext.to_uppercase(),
    })
}

// ----- duplicate finder -----

fn find_duplicates_impl(dir: &Path, min: u64) -> Result<Vec<Vec<FileEntry>>, String> {
    // Phase 1: group by exact size - unique sizes cannot be duplicates.
    // WalkDir entry.metadata() is cache-backed on Windows (FindFirstFileExW), zero extra syscalls.
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        if let Ok(m) = entry.metadata() {
            let size = m.len();
            if size >= min {
                by_size
                    .entry(size)
                    .or_default()
                    .push(entry.path().to_path_buf());
            }
        }
    }

    let size_candidates: Vec<PathBuf> = by_size
        .into_values()
        .filter(|v| v.len() > 1)
        .flatten()
        .collect();

    if size_candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Phase 2: partial hash (first 64 KB) - eliminates near-size false matches cheaply.
    let partial_results: Vec<(String, PathBuf)> = THUMBNAIL_POOL.install(|| {
        size_candidates
            .par_iter()
            .filter_map(|p| quick_sha256(p, 64 * 1024).map(|h| (h, p.clone())))
            .collect()
    });

    let mut by_partial: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (hash, p) in partial_results {
        by_partial.entry(hash).or_default().push(p);
    }

    let full_candidates: Vec<PathBuf> = by_partial
        .into_values()
        .filter(|v| v.len() > 1)
        .flatten()
        .collect();

    if full_candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Phase 3: full hash - only files that survived both prior filters.
    let items: Vec<(String, FileEntry)> = THUMBNAIL_POOL.install(|| {
        full_candidates
            .par_iter()
            .filter_map(|p| {
                let meta = fs::metadata(p).ok()?;
                let mut file = File::open(p).ok()?;
                let mut hasher = Sha256::new();
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = file.read(&mut buf).ok()?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                Some((hex::encode(hasher.finalize()), path_to_entry(p, &meta)))
            })
            .collect()
    });

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

fn duplicate_reclaimable_bytes(groups: &[Vec<FileEntry>]) -> (u64, u64, u64) {
    let mut group_count = 0u64;
    let mut duplicate_count = 0u64;
    let mut reclaimable = 0u64;
    for group in groups.iter().filter(|g| g.len() > 1) {
        let Some(size) = group.first().map(|entry| entry.size) else {
            continue;
        };
        group_count += 1;
        duplicate_count += group.len().saturating_sub(1) as u64;
        reclaimable =
            reclaimable.saturating_add(size.saturating_mul(group.len().saturating_sub(1) as u64));
    }
    (group_count, duplicate_count, reclaimable)
}

#[tauri::command]
fn find_duplicates(path: String, min_size: Option<u64>) -> Result<Vec<Vec<FileEntry>>, String> {
    if ACTIVE_HEAVY_OPS.fetch_add(1, Ordering::SeqCst) >= MAX_HEAVY_OPS {
        ACTIVE_HEAVY_OPS.fetch_sub(1, Ordering::SeqCst);
        return Err("Too many operations in progress. Please wait.".to_string());
    }
    let _guard = HeavyOpGuard;
    let dir = PathBuf::from(&path);
    find_duplicates_impl(&dir, min_size.unwrap_or(4096))
}

// ----- storage tree -----

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

// ----- Storage analyzer -----------------------------------------------------
// Pre-compiled bucket definitions. Order matters: first match wins, so the
// path-based "Apps" check must come before extension checks (a .dll inside
// Program Files should count as Apps, not Other).

const STORAGE_SKIP_DIRS: &[&str] = &[
    "$Recycle.Bin",
    "System Volume Information",
    "Windows.old",
    "Recovery",
    "PerfLogs",
    "Config.Msi",
    "$WinREAgent",
    "OneDriveTemp",
];

/// Skip folders that are too generic to be useful in the drill-in
/// list. These are top-level system / vendor directories sitting
/// directly under the drive root: their rolled-up size dominates
/// (entire Program Files tree, entire Users tree, etc.) but the user
/// can't realistically act on a single 200GB "Program Files" entry.
/// Per-application folders deeper in the tree are far more actionable.
fn is_too_generic_folder(path: &Path, root_components: usize) -> bool {
    let depth = path.components().count();
    // Drive root itself or one level below (e.g., C:\, C:\Users) only.
    if depth > root_components + 1 {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "program files"
            | "program files (x86)"
            | "programdata"
            | "windows"
            | "windows.old"
            | "users"
            | "perflogs"
            | "recovery"
            | "$recycle.bin"
            | "system volume information"
            | "$windows.~bt"
            | "$windows.~ws"
            | "msocache"
            | "config.msi"
            | "documents and settings"
            | "onedrivetemp"
    )
}

fn storage_path_prefixes(path: &Path) -> Vec<(String, PathBuf)> {
    let mut current = PathBuf::new();
    let mut out = Vec::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if let std::path::Component::Normal(name) = component {
            out.push((name.to_string_lossy().to_string(), current.clone()));
        }
    }
    out
}

fn is_default_storage_segment(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "users"
            | "appdata"
            | "local"
            | "locallow"
            | "roaming"
            | "program files"
            | "program files (x86)"
            | "programdata"
            | "steamapps"
            | "common"
            | "games"
            | "game library"
            | "steamlibrary"
            | "steam library"
            | "packages"
            | "package cache"
            | "content"
            | "data"
            | "pak"
            | "paks"
            | "binaries"
            | "bin"
            | "win64"
            | "win32"
            | "x64"
            | "_commonredist"
            | "redist"
            | "redistributables"
            | "cache"
            | "caches"
            | "temp"
            | "tmp"
            | "windows"
            | "microsoft"
            | "start menu"
            | "programs"
            | "$recycle.bin"
            | "system volume information"
            | "perflogs"
            | "recovery"
    )
}

fn known_profile_folder(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "desktop" | "documents" | "downloads" | "pictures" | "videos" | "music"
    )
}

fn storage_rollup_folder_for_file(file_path: &Path, root_components: usize) -> Option<PathBuf> {
    let parent = file_path.parent()?;
    let prefixes = storage_path_prefixes(parent);
    if prefixes.is_empty() {
        return None;
    }
    let names: Vec<String> = prefixes
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();

    let first_below_root = prefixes
        .iter()
        .position(|(_, path)| path.components().count() > root_components)
        .unwrap_or(0);

    // v0.9.19: specialized game-launcher patterns win before the
    // generic Program Files handler. Without this, a file under
    // ...\Steam\steamapps\common\<Game>\... rolls up to "Steam"
    // (good for "Discord" under AppData, wrong here) because the
    // Program Files match fires first in path order. Walk all
    // segments first to detect known game-store directory shapes,
    // and surface the actual game/app folder when found.
    for i in first_below_root..names.len() {
        match names[i].as_str() {
            "steamapps" if names.get(i + 1).map(|s| s.as_str()) == Some("common") => {
                if let Some((_, path)) = prefixes.get(i + 2) {
                    return Some(path.clone());
                }
            }
            "common" if i > 0 && names.get(i - 1).map(|s| s.as_str()) == Some("steamapps") => {
                if let Some((_, path)) = prefixes.get(i + 1) {
                    return Some(path.clone());
                }
            }
            "epic games"
            | "xboxgames"
            | "riot games"
            | "gog games"
            | "gog galaxy"
            | "ea games"
            | "origin games"
            | "ubisoft game launcher"
            | "battle.net"
            | "bethesda.net launcher"
            | "rockstar games" => {
                let mut owner = i + 1;
                while names
                    .get(owner)
                    .map(|name| is_default_storage_segment(name))
                    .unwrap_or(false)
                    && owner + 1 < names.len()
                {
                    owner += 1;
                }
                if let Some((_, path)) = prefixes.get(owner) {
                    return Some(path.clone());
                }
            }
            _ => {}
        }
    }

    // Generic game-library folders such as D:\Games\<Game> need a softer
    // fallback: surface the first useful child, but only after known launcher
    // shapes above had a chance to win.
    for i in first_below_root..names.len() {
        match names[i].as_str() {
            "games" | "game library" | "steamlibrary" | "steam library" => {
                let mut owner = i + 1;
                while names
                    .get(owner)
                    .map(|name| is_default_storage_segment(name))
                    .unwrap_or(false)
                    && owner + 1 < names.len()
                {
                    owner += 1;
                }
                if let Some((_, path)) = prefixes.get(owner) {
                    return Some(path.clone());
                }
            }
            _ => {}
        }
    }

    for i in first_below_root..names.len() {
        match names[i].as_str() {
            "users" => {
                let after_user = i + 2;
                if names.get(after_user).map(|s| s.as_str()) == Some("appdata") {
                    let channel = after_user + 1;
                    let mut owner = if matches!(
                        names.get(channel).map(|s| s.as_str()),
                        Some("local" | "locallow" | "roaming")
                    ) {
                        channel + 1
                    } else {
                        channel
                    };
                    while names
                        .get(owner)
                        .map(|name| is_default_storage_segment(name))
                        .unwrap_or(false)
                        && owner + 1 < names.len()
                    {
                        owner += 1;
                    }
                    if let Some((_, path)) = prefixes.get(owner) {
                        return Some(path.clone());
                    }
                }
                if names
                    .get(after_user)
                    .map(|name| known_profile_folder(name))
                    .unwrap_or(false)
                {
                    if let Some((_, path)) = prefixes.get(after_user + 1) {
                        return Some(path.clone());
                    }
                    if let Some((_, path)) = prefixes.get(after_user) {
                        return Some(path.clone());
                    }
                }
                if let Some((_, path)) = prefixes.get(after_user) {
                    return Some(path.clone());
                }
            }
            "program files" | "program files (x86)" | "programdata" => {
                let mut owner = i + 1;
                while names
                    .get(owner)
                    .map(|name| is_default_storage_segment(name))
                    .unwrap_or(false)
                    && owner + 1 < names.len()
                {
                    owner += 1;
                }
                if let Some((_, path)) = prefixes.get(owner) {
                    return Some(path.clone());
                }
            }
            "steamapps" if names.get(i + 1).map(|s| s.as_str()) == Some("common") => {
                if let Some((_, path)) = prefixes.get(i + 2) {
                    return Some(path.clone());
                }
            }
            "common" if i > 0 && names.get(i - 1).map(|s| s.as_str()) == Some("steamapps") => {
                if let Some((_, path)) = prefixes.get(i + 1) {
                    return Some(path.clone());
                }
            }
            "epic games"
            | "xboxgames"
            | "riot games"
            | "gog games"
            | "gog galaxy"
            | "ea games"
            | "origin games"
            | "ubisoft game launcher"
            | "battle.net"
            | "bethesda.net launcher"
            | "rockstar games" => {
                if let Some((_, path)) = prefixes.get(i + 1) {
                    return Some(path.clone());
                }
            }
            _ => {}
        }
    }

    prefixes
        .iter()
        .skip(first_below_root)
        .find(|(name, path)| {
            !is_default_storage_segment(name) && path.components().count() > root_components
        })
        .map(|(_, path)| path.clone())
        .or_else(|| Some(parent.to_path_buf()))
}

fn storage_bucket_for(path: &Path, ctx: &StorageScanCtx) -> &'static str {
    let path_bytes = path.as_os_str().as_encoded_bytes();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    if file_name.to_ascii_lowercase().ends_with(".sys") {
        return "system";
    }
    if path_bytes_contains_ci(path_bytes, br"\windows\")
        || path_bytes_contains_ci(path_bytes, br"\winsxs\")
    {
        return "system";
    }
    if storage_is_cache_path(path_bytes, file_name) {
        return "cache";
    }
    if storage_is_app_path(path_bytes) {
        return "apps";
    }
    if let Some(rest) = users_profile_rest(path_bytes) {
        if let Some(bucket) = profile_bucket_for_rest(rest) {
            return bucket;
        }
    }
    if let Some(home) = ctx.home_lower.as_ref() {
        if bytes_prefix_eq_ci(path_bytes, home)
            && (path_bytes.len() == home.len()
                || path_bytes[home.len()] == b'\\'
                || path_bytes[home.len()] == b'/')
        {
            let rest = &path_bytes[home.len()..];
            let rest = rest.strip_prefix(b"\\").or_else(|| rest.strip_prefix(b"/"));
            if let Some(rest) = rest {
                if let Some(bucket) = profile_bucket_for_rest(rest) {
                    return bucket;
                }
            }
        }
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    storage_bucket_for_ext(ext, path_bytes)
}

fn storage_bucket_for_ext(ext: &str, path_bytes: &[u8]) -> &'static str {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "raw"
        | "cr2" | "nef" | "arw" | "svg" | "mp4" | "mov" | "mkv" | "avi" | "webm" | "wmv"
        | "flv" | "m4v" | "mpg" | "mpeg" | "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a"
        | "wma" | "opus" => "media",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf"
        | "odt" | "epub" | "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "cab" => {
            "documents"
        }
        "exe" | "msi" | "msix" | "appx" if storage_is_app_path(path_bytes) => "apps",
        "exe" | "msi" | "msix" | "appx" => "other",
        "dll"
            if storage_is_app_path(path_bytes)
                || path_bytes_contains_ci(path_bytes, br"\windows\") =>
        {
            "apps"
        }
        "dll" => "other",
        "sys" => "system",
        _ => "other",
    }
}

/// Bucket metadata. Order here drives the display order in the UI.
fn storage_bucket_meta() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        (
            "apps",
            "Apps & games",
            "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z",
            "#7B3FA0",
        ),
        (
            "documents",
            "Documents",
            "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z M14 2v6h6",
            "#185FA5",
        ),
        (
            "media",
            "Media",
            "M21 15V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2z M8 11a2 2 0 1 0 0-4 2 2 0 0 0 0 4z M21 19l-6-6-9 9",
            "#A87EF8",
        ),
        (
            "downloads",
            "Downloads",
            "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4 M7 10l5 5 5-5 M12 15V3",
            "#3B6D11",
        ),
        (
            "desktop",
            "Desktop",
            "M2 4h20v12H2z M8 21h8 M12 17v4",
            "#8A5A00",
        ),
        (
            "cache",
            "Cache & temp",
            "M3 6h18 M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6 M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2",
            "#C04870",
        ),
        (
            "system",
            "System",
            "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33",
            "#5F5E5A",
        ),
        (
            "other",
            "Other",
            "M3 7.5A2.5 2.5 0 0 1 5.5 5H10l2 2h6.5A2.5 2.5 0 0 1 21 9.5v7A2.5 2.5 0 0 1 18.5 19h-13A2.5 2.5 0 0 1 3 16.5z",
            "#4A6A20",
        ),
    ]
}

fn remap_storage_entry_bucket(entry: &mut StorageEntry) {
    entry.bucket = storage_canonical_bucket(&entry.bucket).to_string();
}

fn migrate_storage_scan_result(result: &mut StorageScanResult) {
    for entry in &mut result.top_items {
        remap_storage_entry_bucket(entry);
    }
    for items in result.bucket_items.values_mut() {
        for entry in items {
            remap_storage_entry_bucket(entry);
        }
    }
    for items in result.bucket_folder_items.values_mut() {
        for entry in items {
            remap_storage_entry_bucket(entry);
        }
    }

    let meta = storage_bucket_meta();
    let mut merged: HashMap<String, StorageBucket> = HashMap::new();
    for b in result.buckets.drain(..) {
        let id = storage_canonical_bucket(&b.id).to_string();
        let entry = merged.entry(id.clone()).or_insert_with(|| {
            let (_, name, icon, color) = meta
                .iter()
                .find(|(bid, _, _, _)| *bid == id.as_str())
                .copied()
                .unwrap_or(("other", "Other", "", "#4A6A20"));
            StorageBucket {
                id: id.clone(),
                name: name.to_string(),
                icon: icon.to_string(),
                bytes: 0,
                file_count: 0,
                color: color.to_string(),
            }
        });
        entry.bytes = entry.bytes.saturating_add(b.bytes);
        entry.file_count = entry.file_count.saturating_add(b.file_count);
    }
    result.buckets = meta
        .iter()
        .filter_map(|(id, _, _, _)| merged.remove(*id))
        .collect();

    let mut merged_items: HashMap<String, Vec<StorageEntry>> = HashMap::new();
    for (key, items) in result.bucket_items.drain() {
        let canon = storage_canonical_bucket(&key).to_string();
        merged_items.entry(canon).or_default().extend(items);
    }
    result.bucket_items = merged_items;

    let mut merged_folders: HashMap<String, Vec<StorageEntry>> = HashMap::new();
    for (key, items) in result.bucket_folder_items.drain() {
        let canon = storage_canonical_bucket(&key).to_string();
        merged_folders.entry(canon).or_default().extend(items);
    }
    result.bucket_folder_items = merged_folders;
}

/// Whole-drive storage scan, optimized for throughput.
///
/// Performance pipeline:
///   1. Directory walk uses `jwalk`, which descends multiple subtrees in
///      parallel on rayon's pool. On a typical NVMe with 500k files this
///      takes 3-8 seconds vs walkdir's ~30 seconds.
///   2. Metadata is read at readdir time (Windows FindFirstFileW returns size
///      in the same call), so no extra syscall per file.
///   3. Categorization + per-bucket aggregation runs inside jwalk's
///      `process_read_dir` callback, so it happens in parallel during the
///      walk - no second pass over the entries.
///   4. Per-bucket top-N lists are maintained via bounded min-heaps; memory
///      stays at O(buckets x top_per_bucket) regardless of total file count.
///   5. Progress counters are lock-free AtomicU64s the UI polls at 100ms,
///      so the user sees live counts during the scan with zero overhead in
///      the hot path.
fn scan_storage_with_progress(
    root: &Path,
    top_n: usize,
    progress: Option<Arc<StorageScanProgress>>,
) -> StorageScanResult {
    use jwalk::WalkDir as JWalkDir;
    use std::collections::{BinaryHeap, HashMap};
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    let started = Instant::now();
    let scanned_at = now_unix_secs() as i64;
    let scan_ctx = Arc::new(StorageScanCtx::new());

    let per_bucket_n = top_n.max(50);
    let bucket_ids: Vec<&'static str> = storage_bucket_meta()
        .iter()
        .map(|(id, _, _, _)| *id)
        .collect();

    #[derive(PartialEq, Eq, Clone)]
    struct MinByBytes(std::cmp::Reverse<u64>, String, i64);
    impl Ord for MinByBytes {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.0.cmp(&other.0)
        }
    }
    impl PartialOrd for MinByBytes {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    struct BucketState {
        bytes: AtomicU64,
        file_count: AtomicU64,
        top: StdMutex<BinaryHeap<MinByBytes>>,
    }

    fn push_top(
        heap: &mut BinaryHeap<MinByBytes>,
        cap: usize,
        size: u64,
        modified: i64,
        path: String,
    ) {
        if heap.len() < cap {
            heap.push(MinByBytes(std::cmp::Reverse(size), path, modified));
        } else if let Some(min) = heap.peek()
            && (min.0).0 < size
        {
            heap.pop();
            heap.push(MinByBytes(std::cmp::Reverse(size), path, modified));
        }
    }

    let bucket_states: Arc<HashMap<&'static str, BucketState>> = Arc::new(
        bucket_ids
            .iter()
            .map(|id| {
                (
                    *id,
                    BucketState {
                        bytes: AtomicU64::new(0),
                        file_count: AtomicU64::new(0),
                        top: StdMutex::new(BinaryHeap::with_capacity(per_bucket_n + 1)),
                    },
                )
            })
            .collect(),
    );

    #[derive(Clone, Copy, Default)]
    struct FolderAggregate {
        bytes: u64,
        modified: i64,
    }

    const SHARDS: usize = 64;
    let folder_shards: Arc<Vec<StdMutex<HashMap<String, FolderAggregate>>>> =
        Arc::new((0..SHARDS).map(|_| StdMutex::new(HashMap::new())).collect());

    let total_bytes_atomic = Arc::new(AtomicU64::new(0));
    let total_files_atomic = Arc::new(AtomicU64::new(0));
    let progress_ref = progress.clone();
    let root_components = root.components().count();

    let walker = JWalkDir::new(root)
        .skip_hidden(false)
        .parallelism(jwalk::Parallelism::RayonExistingPool {
            pool: std::sync::Arc::new(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(num_cpus())
                    .build()
                    .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap()),
            ),
            busy_timeout: None,
        })
        .process_read_dir({
            let bucket_states = bucket_states.clone();
            let folder_shards = folder_shards.clone();
            let total_bytes_atomic = total_bytes_atomic.clone();
            let total_files_atomic = total_files_atomic.clone();
            let progress_ref = progress_ref.clone();
            let scan_ctx = scan_ctx.clone();
            move |_depth, _dir_path, _state, children| {
                let mut files: Vec<(u64, i64, PathBuf)> = Vec::new();
                children.retain(|c| {
                    let Ok(entry) = c.as_ref() else {
                        return false;
                    };
                    if entry.file_type().is_dir() {
                        let name = entry.file_name().to_string_lossy();
                        return !STORAGE_SKIP_DIRS
                            .iter()
                            .any(|skip| name.eq_ignore_ascii_case(skip));
                    }
                    let Ok(metadata) = entry.metadata() else {
                        return false;
                    };
                    let size = metadata.len();
                    let modified = unix_secs_i64(metadata.modified());
                    files.push((size, modified, entry.path()));
                    false
                });
                files.par_iter().for_each(|(size, modified, path)| {
                    if let Some(p) = progress_ref.as_ref()
                        && p.cancelled.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    let bucket = storage_bucket_for(path, scan_ctx.as_ref());
                    let Some(state) = bucket_states.get(bucket) else {
                        return;
                    };
                    state.bytes.fetch_add(*size, Ordering::Relaxed);
                    state.file_count.fetch_add(1, Ordering::Relaxed);
                    total_bytes_atomic.fetch_add(*size, Ordering::Relaxed);
                    let file_count_total = total_files_atomic.fetch_add(1, Ordering::Relaxed) + 1;

                    let path_str = path.to_string_lossy().into_owned();
                    if let Ok(mut heap) = state.top.lock() {
                        push_top(&mut heap, per_bucket_n, *size, *modified, path_str.clone());
                    }

                    if let Some(folder) = storage_rollup_folder_for_file(path, root_components) {
                        if folder.components().count() > root_components {
                            let key = folder.to_string_lossy();
                            let shard_idx = (fxhash_str(key.as_ref()) as usize) % SHARDS;
                            if let Ok(mut shard) = folder_shards[shard_idx].lock() {
                                let agg = shard.entry(key.into_owned()).or_default();
                                agg.bytes = agg.bytes.saturating_add(*size);
                                agg.modified = agg.modified.max(*modified);
                            }
                        }
                    }

                    if let Some(p) = progress_ref.as_ref()
                        && file_count_total.is_multiple_of(4096)
                    {
                        p.files.store(file_count_total, Ordering::Relaxed);
                        p.bytes.store(
                            total_bytes_atomic.load(Ordering::Relaxed),
                            Ordering::Relaxed,
                        );
                    }
                });
            }
        });

    for result in walker {
        if let Some(p) = progress_ref.as_ref()
            && p.cancelled.load(Ordering::Relaxed)
        {
            break;
        }
        let _ = result;
    }

    // Drain per-bucket heaps into sorted Vec<StorageEntry>.
    let mut bucket_items: HashMap<String, Vec<StorageEntry>> = HashMap::new();
    for (id, state) in bucket_states.iter() {
        let heap = state
            .top
            .lock()
            .map(|h| h.clone().into_sorted_vec())
            .unwrap_or_default();
        // into_sorted_vec on a min-heap gives ascending order; reverse for desc.
        let entries: Vec<StorageEntry> = heap
            .into_iter()
            .rev()
            .map(|MinByBytes(size_rev, path, modified)| {
                let name = Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                StorageEntry {
                    path,
                    name,
                    bytes: size_rev.0,
                    modified,
                    is_dir: false,
                    bucket: (*id).to_string(),
                }
            })
            .collect();
        bucket_items.insert((*id).to_string(), entries);
    }

    // Build bucket roll-ups in display order.
    let meta = storage_bucket_meta();
    let mut buckets: Vec<StorageBucket> = meta
        .iter()
        .map(|(id, name, icon, color)| {
            let st = bucket_states.get(*id);
            let bytes = st.map(|s| s.bytes.load(Ordering::Relaxed)).unwrap_or(0);
            let file_count = st
                .map(|s| s.file_count.load(Ordering::Relaxed))
                .unwrap_or(0);
            StorageBucket {
                id: (*id).to_string(),
                name: (*name).to_string(),
                icon: (*icon).to_string(),
                bytes,
                file_count,
                color: (*color).to_string(),
            }
        })
        .collect();
    buckets.retain(|b| b.bytes > 0 || b.id == "other");

    let total_bytes = total_bytes_atomic.load(Ordering::Relaxed);
    let scanned_files = total_files_atomic.load(Ordering::Relaxed);

    // Combine all folder shards then take top folders.
    let mut folder_sizes: HashMap<String, FolderAggregate> = HashMap::new();
    for shard in folder_shards.iter() {
        if let Ok(map) = shard.lock() {
            for (k, v) in map.iter() {
                let agg = folder_sizes.entry(k.clone()).or_default();
                agg.bytes = agg.bytes.saturating_add(v.bytes);
                agg.modified = agg.modified.max(v.modified);
            }
        }
    }
    let mut folders: Vec<(String, FolderAggregate)> = folder_sizes.into_iter().collect();
    folders.sort_unstable_by_key(|b| std::cmp::Reverse(b.1.bytes));

    // Global top-N: favor FOLDERS over individual files (4:1 mix). A single
    // game install folder at 80 GB is far more useful to surface than 50
    // individual .pak files inside it. Files only earn a slot if they're
    // truly large standalone items (ISOs, VM images, backups). Previous
    // 50/50 split made the list feel noisy because games and apps generated
    // hundreds of files that crowded out actual folder-level insights.
    let folders_slots = (top_n * 4 / 5).max(1); // 80%
    let files_slots = top_n.saturating_sub(folders_slots).max(1);
    let mut combined: Vec<StorageEntry> = Vec::with_capacity(per_bucket_n * meta.len());
    for entries in bucket_items.values() {
        combined.extend(entries.iter().cloned());
    }
    combined.sort_unstable_by_key(|b| std::cmp::Reverse(b.bytes));
    let top_files: Vec<StorageEntry> = combined.into_iter().take(files_slots).collect();
    let top_folders: Vec<StorageEntry> = folders
        .iter()
        .take(folders_slots)
        .map(|(path, agg)| {
            let name = Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            let bucket = storage_bucket_for(Path::new(&path), scan_ctx.as_ref()).to_string();
            StorageEntry {
                path: path.clone(),
                name,
                bytes: agg.bytes,
                modified: agg.modified,
                is_dir: true,
                bucket,
            }
        })
        .collect();
    let mut top_items: Vec<StorageEntry> = top_files.into_iter().chain(top_folders).collect();
    top_items = refine_storage_largest_list(top_items);
    top_items.sort_unstable_by_key(|b| std::cmp::Reverse(b.bytes));
    top_items.truncate(top_n);

    // Per-bucket folder roll-ups: walk the globally-sorted folder list,
    // classify each by bucket, push to that bucket's list until we have
    // per_bucket_n entries. This is what the drill-in UI shows when the
    // user clicks "Apps & games" etc. - full folders/apps grouped by
    // category, not the thousands of individual files inside them.
    let mut bucket_folder_items: HashMap<String, Vec<StorageEntry>> = HashMap::new();
    for (id, _, _, _) in meta.iter() {
        bucket_folder_items.insert((*id).to_string(), Vec::with_capacity(per_bucket_n));
    }
    let buckets_count = meta.len();
    let mut completed_buckets = 0usize;
    // Bounded to top 4 000 folders so the classify loop stays cheap
    // even when one or two buckets have few matching folders.
    for (path, agg) in folders.iter().take(4000) {
        if completed_buckets >= buckets_count {
            break;
        }
        let pb = Path::new(path);
        // Skip "too generic" entries: top-level system folders right
        // under a drive root (Program Files, Windows, Users, etc.).
        // Users want to see e.g. "Crimson Desert", not "Program Files
        // (x86)" with 200GB rolled up. Per-application folders deeper
        // in the tree are far more actionable for cleanup.
        if is_too_generic_folder(pb, root_components) {
            continue;
        }
        let bucket = storage_bucket_for(pb, scan_ctx.as_ref()).to_string();
        let Some(vec) = bucket_folder_items.get_mut(&bucket) else {
            continue;
        };
        if vec.len() >= per_bucket_n {
            continue;
        }
        let name = pb
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        let was_one_short = vec.len() == per_bucket_n - 1;
        vec.push(StorageEntry {
            path: path.clone(),
            name,
            bytes: agg.bytes,
            modified: agg.modified,
            is_dir: true,
            bucket,
        });
        if was_one_short {
            completed_buckets += 1;
        }
    }

    if let Some(p) = progress.as_ref() {
        p.files.store(scanned_files, Ordering::Relaxed);
        p.bytes.store(total_bytes, Ordering::Relaxed);
        p.done.store(true, Ordering::Release);
    }

    let root_signature = capture_storage_root_signature(root);
    let (duplicate_groups, duplicate_count, duplicate_reclaimable_bytes) = if progress
        .as_ref()
        .map(|p| p.cancelled.load(Ordering::Relaxed))
        .unwrap_or(false)
    {
        (0, 0, 0)
    } else {
        find_duplicates_impl(root, STORAGE_DUPLICATE_MIN_SIZE)
            .map(|groups| duplicate_reclaimable_bytes(&groups))
            .unwrap_or((0, 0, 0))
    };

    StorageScanResult {
        root: root.to_string_lossy().into_owned(),
        total_bytes,
        scanned_files,
        scanned_at,
        buckets,
        top_items,
        bucket_items,
        bucket_folder_items,
        elapsed_ms: started.elapsed().as_millis() as u64,
        root_signature,
        duplicate_groups,
        duplicate_count,
        duplicate_reclaimable_bytes,
    }
}

fn scan_storage(root: &Path, top_n: usize) -> StorageScanResult {
    scan_storage_with_progress(root, top_n, None)
}

fn path_is_strict_parent(parent: &str, child: &str) -> bool {
    let p = parent
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    let c = child.replace('/', "\\").to_ascii_lowercase();
    if p.is_empty() || c.len() <= p.len() || p == c {
        return false;
    }
    c.starts_with(&p) && matches!(c.as_bytes().get(p.len()), Some(b'\\'))
}

/// Drill-in folder lists should not show both a parent folder and its nested
/// children (e.g. `common`, `Crimson Desert`, and `0015` all at once).
fn is_storage_apps_drill_noise(path: &str) -> bool {
    let lower = path.replace('/', "\\").to_ascii_lowercase();
    if lower.contains("\\appdata\\") && !lower.contains("\\appdata\\local\\programs\\") {
        return true;
    }
    false
}

fn is_steam_common_container_path(path: &str) -> bool {
    let lower = path.replace('/', "\\").to_ascii_lowercase();
    lower.ends_with("\\steamapps\\common") || lower.ends_with("\\steamapps\\common\\")
}

/// Paths that should never appear in the global "Largest items" list.
fn is_storage_largest_folder_skip(path: &str) -> bool {
    let lower = path.replace('/', "\\").to_ascii_lowercase();
    if is_steam_common_container_path(path) {
        return true;
    }
    if is_storage_apps_drill_noise(path) {
        return true;
    }
    if lower.contains("\\windows\\")
        || lower.contains("\\winsxs\\")
        || lower.ends_with("\\appdata\\local")
        || lower.ends_with("\\appdata\\roaming")
        || lower.ends_with("\\appdata\\locallow")
    {
        return true;
    }
    false
}

fn is_storage_largest_file_skip(path: &str) -> bool {
    is_storage_apps_drill_noise(path)
}

/// Patch / content subfolders inside a game install (e.g. `0015`, `pak`).
fn is_storage_noise_subfolder_name(name: &str) -> bool {
    let n = name.trim();
    if n.len() <= 4 && n.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    matches!(
        n.to_ascii_lowercase().as_str(),
        "content" | "data" | "pak" | "paks" | "binaries" | "bin" | "win64" | "win32" | "saved"
    )
}

/// Prefer the deepest actionable folder (e.g. a Steam game) over parent
/// containers (`steamapps\\common`, `Program Files`, etc.).
fn refine_storage_folder_entries(
    bucket_id: Option<&str>,
    mut folders: Vec<StorageEntry>,
) -> Vec<StorageEntry> {
    folders.sort_unstable_by(|a, b| {
        Path::new(&b.path)
            .components()
            .count()
            .cmp(&Path::new(&a.path).components().count())
            .then_with(|| b.bytes.cmp(&a.bytes))
    });
    let mut out: Vec<StorageEntry> = Vec::new();
    for e in folders {
        if is_default_storage_segment(&e.name) || is_storage_noise_subfolder_name(&e.name) {
            continue;
        }
        if is_storage_largest_folder_skip(&e.path) {
            continue;
        }
        if bucket_id == Some("apps") && is_storage_apps_drill_noise(&e.path) {
            continue;
        }
        if out.iter().any(|k| {
            path_is_strict_parent(&e.path, &k.path) && !is_storage_noise_subfolder_name(&k.name)
        }) {
            continue;
        }
        out.retain(|k| {
            !path_is_strict_parent(&e.path, &k.path) || is_storage_noise_subfolder_name(&k.name)
        });
        out.push(e);
    }
    out
}

/// Collapse nested folder roll-ups and drop AppData / Steam container shells
/// from the overview "Largest items" list.
fn refine_storage_largest_list(entries: Vec<StorageEntry>) -> Vec<StorageEntry> {
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for e in entries {
        if e.is_dir {
            folders.push(e);
        } else {
            files.push(e);
        }
    }
    let mut out = refine_storage_folder_entries(None, folders);
    for f in files {
        if is_storage_largest_file_skip(&f.path) {
            continue;
        }
        if out
            .iter()
            .any(|fldr| path_is_strict_parent(&fldr.path, &f.path) || fldr.path == f.path)
        {
            continue;
        }
        out.push(f);
    }
    out.sort_unstable_by_key(|e| std::cmp::Reverse(e.bytes));
    out
}

fn storage_path_summary(path: &str) -> String {
    let norm = path.replace('/', "\\");
    let lower = norm.to_ascii_lowercase();
    for marker in [
        "\\steamapps\\common\\",
        "\\steamapps\\",
        "\\program files (x86)\\",
        "\\program files\\",
        "\\users\\",
    ] {
        if let Some(idx) = lower.find(marker) {
            let tail = norm[idx + 1..].trim_start_matches('\\');
            if !tail.is_empty() {
                return format!("…\\{tail}");
            }
        }
    }
    if norm.len() > 72 {
        format!("…{}", &norm[norm.len().saturating_sub(68)..])
    } else {
        norm
    }
}

fn storage_folder_display_title(path: &Path, fallback: &str) -> String {
    let path_str = path.to_string_lossy();
    let lower = path_str.to_ascii_lowercase();
    if let Some(idx) = lower.find("\\steamapps\\common\\") {
        let tail = &path_str[idx + "\\steamapps\\common\\".len()..];
        if let Some(game) = tail.split('\\').next() {
            if !game.is_empty() && !is_default_storage_segment(game) {
                return game.to_string();
            }
        }
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if !is_default_storage_segment(name) {
            return name.to_string();
        }
    }
    fallback.to_string()
}

fn storage_entry_display_labels(entry: &StorageEntry) -> (String, String) {
    let title = if entry.is_dir {
        storage_folder_display_title(Path::new(&entry.path), &entry.name)
    } else {
        entry.name.clone()
    };
    let path_line = storage_path_summary(&entry.path);
    (title, path_line)
}

fn storage_apps_drill_rank(path: &str) -> u8 {
    let lower = path.replace('/', "\\").to_ascii_lowercase();
    if lower.contains("\\steamapps\\common\\")
        || lower.contains("\\steamlibrary\\steamapps\\common\\")
        || lower.contains("\\epic games\\")
        || lower.contains("\\xboxgames\\")
        || lower.contains("\\riot games\\")
        || lower.contains("\\gog games\\")
        || lower.contains("\\ea games\\")
        || lower.contains("\\origin games\\")
        || lower.contains("\\ubisoft game launcher\\")
        || lower.contains("\\battle.net\\")
        || lower.contains("\\rockstar games\\")
    {
        return 3;
    }
    if lower.contains("\\program files\\") || lower.contains("\\program files (x86)\\") {
        return 2;
    }
    if lower.contains("\\games\\") || lower.contains("\\game library\\") {
        return 1;
    }
    0
}

/// Re-order the drill-in / largest list per the user-selected sort mode.
/// "size" preserves the size-ranked behavior (with the apps bucket's
/// game-launcher priority); "name" and "recent" let the user pivot the same
/// data without a rescan.
fn apply_storage_sort_mode(mode: &str, bucket_id: &str, entries: &mut [StorageEntry]) {
    match mode {
        "name" => entries.sort_unstable_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| b.bytes.cmp(&a.bytes))
        }),
        "recent" => entries.sort_unstable_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| b.bytes.cmp(&a.bytes))
        }),
        "oldest" => entries.sort_unstable_by(|a, b| {
            a.modified
                .cmp(&b.modified)
                .then_with(|| b.bytes.cmp(&a.bytes))
        }),
        _ => sort_storage_drill_entries(bucket_id, entries),
    }
}

fn sort_storage_drill_entries(bucket_id: &str, entries: &mut [StorageEntry]) {
    if bucket_id == "apps" {
        entries.sort_unstable_by(|a, b| {
            storage_apps_drill_rank(&b.path)
                .cmp(&storage_apps_drill_rank(&a.path))
                .then_with(|| b.bytes.cmp(&a.bytes))
                .then_with(|| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                })
        });
        return;
    }
    entries.sort_unstable_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
}

/// v0.9.11: dropped the steam-only filter for the `apps` bucket - it was
/// hiding Epic/GOG/standalone installs and limiting users to ~5 entries.
/// The generic dedup pass below already collapses nested duplicates;
/// non-steam app folders sit alongside steam games naturally.
fn refine_storage_drill_folders(bucket_id: &str, folders: Vec<StorageEntry>) -> Vec<StorageEntry> {
    let mut out = refine_storage_folder_entries(Some(bucket_id), folders);
    sort_storage_drill_entries(bucket_id, &mut out);
    out
}

/// Cheap FNV-1a string hash for picking a shard. Stable per-run, that's all
/// we need for spreading the folder-aggregation map across shards.
fn fxhash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 32)
}

#[tauri::command]
fn scan_storage_root(root: String, top_n: Option<usize>) -> Result<StorageScanResult, String> {
    let dir = PathBuf::from(&root);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {root}"));
    }
    Ok(scan_storage(&dir, top_n.unwrap_or(200)))
}

#[cfg(test)]
mod path_query_tests {
    use super::*;

    #[test]
    fn expand_path_query_expands_percent_vars() {
        if std::env::var("USERPROFILE").is_err() {
            return;
        }
        let expanded = expand_path_query("%USERPROFILE%\\Documents");
        assert!(!expanded.contains('%'));
        assert!(expanded.to_ascii_lowercase().contains("documents"));
    }

    #[test]
    fn looks_like_path_query_detects_env_syntax() {
        assert!(looks_like_path_query(r"%localappdata%\Pagoda"));
        assert!(!looks_like_path_query("pagoda"));
    }

    #[test]
    fn looks_like_path_query_skips_virtual_nav_paths() {
        assert!(!looks_like_path_query("recycle://"));
        assert!(!looks_like_path_query("storage://"));
        assert!(!looks_like_path_query("archive://Zm9v!/%2F"));
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[test]
    fn storage_bucket_classifies_profile_media() {
        let ctx = StorageScanCtx::new();
        let path = Path::new(r"C:\Users\Someone\Pictures\vacation.jpg");
        assert_eq!(storage_bucket_for(path, &ctx), "media");
    }

    #[test]
    fn storage_bucket_classifies_documents_and_archives() {
        let ctx = StorageScanCtx::new();
        assert_eq!(
            storage_bucket_for(Path::new(r"C:\Data\backup.zip"), &ctx),
            "documents"
        );
    }

    #[test]
    fn storage_bucket_narrow_cache_not_browser_cache() {
        let ctx = StorageScanCtx::new();
        let browser = Path::new(
            r"C:\Users\a\AppData\Local\Google\Chrome\User Data\Default\Cache\f_001",
        );
        assert_ne!(storage_bucket_for(browser, &ctx), "cache");
        let temp = Path::new(r"C:\Users\a\AppData\Local\Temp\setup.tmp");
        assert_eq!(storage_bucket_for(temp, &ctx), "cache");
    }

    #[test]
    fn storage_canonical_bucket_merges_legacy_ids() {
        assert_eq!(storage_canonical_bucket("pictures"), "media");
        assert_eq!(storage_canonical_bucket("temp"), "cache");
    }

    #[test]
    fn storage_drill_folders_drop_nested_subfolders() {
        let folders = vec![
            StorageEntry {
                path: r"C:\Steam\steamapps\common".to_string(),
                name: "common".to_string(),
                bytes: 200,
                modified: 0,
                is_dir: true,
                bucket: "apps".to_string(),
            },
            StorageEntry {
                path: r"C:\Steam\steamapps\common\GameA".to_string(),
                name: "GameA".to_string(),
                bytes: 150,
                modified: 0,
                is_dir: true,
                bucket: "apps".to_string(),
            },
            StorageEntry {
                path: r"C:\Steam\steamapps\common\GameA\pak".to_string(),
                name: "pak".to_string(),
                bytes: 50,
                modified: 0,
                is_dir: true,
                bucket: "apps".to_string(),
            },
        ];
        let refined = refine_storage_drill_folders("apps", folders);
        // Default container folders are ignored; the highest useful child wins.
        assert_eq!(refined.len(), 1);
        assert_eq!(refined[0].name, "GameA");
    }

    #[test]
    fn storage_largest_list_prefers_game_over_steam_common() {
        let raw = vec![
            StorageEntry {
                path: r"C:\Program Files (x86)\Steam\steamapps\common".to_string(),
                name: "common".to_string(),
                bytes: 191_000_000_000,
                modified: 0,
                is_dir: true,
                bucket: "apps".to_string(),
            },
            StorageEntry {
                path: r"C:\Program Files (x86)\Steam\steamapps\common\Crimson Desert".to_string(),
                name: "Crimson Desert".to_string(),
                bytes: 129_000_000_000,
                modified: 0,
                is_dir: true,
                bucket: "apps".to_string(),
            },
            StorageEntry {
                path: r"C:\Program Files (x86)\Steam\steamapps\common\Crimson Desert\0015"
                    .to_string(),
                name: "0015".to_string(),
                bytes: 44_000_000_000,
                modified: 0,
                is_dir: true,
                bucket: "apps".to_string(),
            },
        ];
        let refined = refine_storage_largest_list(raw);
        assert_eq!(refined.len(), 1);
        assert_eq!(refined[0].name, "Crimson Desert");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn storage_rollup_skips_default_container_folders() {
        let root = Path::new(r"C:\");
        let root_components = root.components().count();
        let appdata_file =
            Path::new(r"C:\Users\shade\AppData\Local\Google\Chrome\User Data\Cache\a.bin");
        let steam_file = Path::new(r"D:\SteamLibrary\steamapps\common\SpaceGame\pak0.pak");

        let appdata_rollup =
            storage_rollup_folder_for_file(appdata_file, root_components).expect("appdata rollup");
        let steam_rollup =
            storage_rollup_folder_for_file(steam_file, root_components).expect("steam rollup");

        assert_eq!(
            appdata_rollup.file_name().and_then(|n| n.to_str()),
            Some("Google")
        );
        assert_eq!(
            steam_rollup.file_name().and_then(|n| n.to_str()),
            Some("SpaceGame")
        );
    }

    #[test]
    fn storage_bucket_drill_in_uses_folder_rollups_for_apps() {
        let root =
            std::env::temp_dir().join(format!("pathfinder-storage-rollup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let game_dir = root.join("steamapps").join("common").join("SpaceGame");
        fs::create_dir_all(&game_dir).expect("create test game dir");
        fs::write(game_dir.join("pak0.pak"), vec![1u8; 4096]).expect("write pak0");
        fs::write(game_dir.join("pak1.pak"), vec![2u8; 2048]).expect("write pak1");

        let result = scan_storage(&root, 25);
        let app_rows = result
            .bucket_folder_items
            .get("apps")
            .expect("apps bucket exists");

        assert!(
            app_rows.iter().any(|entry| {
                entry.is_dir && entry.path.to_ascii_lowercase().contains("spacegame")
            }),
            "apps drill-in should include the app folder, not only its files"
        );
        assert!(
            app_rows.iter().all(|entry| entry.is_dir),
            "apps drill-in rollup rows should be folders"
        );

        let _ = fs::remove_dir_all(&root);
    }
}

// ----- archives -----

fn safe_archive_out_path(dest: &Path, entry_name: &str) -> Option<PathBuf> {
    let relative = Path::new(entry_name);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(dest.join(relative))
}

fn extract_zip_archive(
    state: &AppState,
    src: &Path,
    dest: &Path,
    selected: Option<&HashSet<String>>,
    conflict: &str,
) -> Result<(), String> {
    let file = File::open(src).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let total = src.metadata().map(|m| m.len()).unwrap_or(0);
    let op_id = state.queue_start(
        "extract",
        &src.to_string_lossy(),
        Some(&dest.to_string_lossy()),
        total,
    );
    let started = Instant::now();
    let mut bytes_done: u64 = 0;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        if let Some(items) = selected {
            let normalized_name = normalize_archive_prefix(&name);
            let matched = items.iter().any(|item| {
                normalized_name == *item || normalized_name.starts_with(&format!("{item}/"))
            });
            if !matched {
                continue;
            }
        }
        let Some(mut out) = safe_archive_out_path(dest, &name) else {
            continue;
        };
        if out.exists() {
            match conflict {
                "replace" => {
                    if out.is_dir() {
                        fs::remove_dir_all(&out).map_err(|e| e.to_string())?;
                    } else {
                        fs::remove_file(&out).map_err(|e| e.to_string())?;
                    }
                }
                "skip" => continue,
                _ => out = keep_both_destination(&out),
            }
        }
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut outfile = File::create(&out).map_err(|e| e.to_string())?;
            // Stream so the queue progress reflects extraction throughput.
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = match entry.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => return Err(e.to_string()),
                };
                outfile.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                bytes_done = bytes_done.saturating_add(n as u64);
                state.queue_progress(op_id, bytes_done, started);
            }
        }
    }

    state.invalidate_path(dest);
    state.queue_finish(op_id, "done", "Extracted", total, started.elapsed());
    Ok(())
}

fn extract_with_7z(
    state: &AppState,
    src: &Path,
    dest: &Path,
    selected: &[String],
    password: Option<&str>,
    conflict: &str,
) -> Result<(), String> {
    let seven_zip = find_7z().ok_or_else(|| "7-Zip was not found on this system.".to_string())?;
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let total = src.metadata().map(|m| m.len()).unwrap_or(0);
    let op_id = state.queue_start(
        "extract",
        &src.to_string_lossy(),
        Some(&dest.to_string_lossy()),
        total,
    );
    let started = Instant::now();
    let overwrite = match conflict {
        "replace" => "-aoa",
        "skip" => "-aos",
        _ => "-aou",
    };
    let mut command = ProcessCommand::new(seven_zip);
    command
        .arg("x")
        .arg(src)
        .arg(format!("-o{}", dest.display()))
        .arg(overwrite);
    if let Some(password) = password {
        command.arg(format!("-p{password}"));
    }
    for item in selected {
        command.arg(item);
    }
    command.no_window();
    let output = command.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        state.invalidate_path(dest);
        state.queue_finish(op_id, "done", "Extracted", total, started.elapsed());
        Ok(())
    } else {
        let error = String::from_utf8_lossy(&output.stderr).to_string();
        state.queue_finish(op_id, "failed", error.clone(), 0, started.elapsed());
        Err(error)
    }
}

fn extract_archive_impl(
    state: &AppState,
    path: &str,
    dest: &str,
    selected: &[String],
    password: Option<&str>,
    conflict: &str,
) -> Result<(), String> {
    let src = PathBuf::from(path);
    let dst = PathBuf::from(dest);
    fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
    let ext = extension(&src);
    if ext == "zip" && password.unwrap_or_default().is_empty() {
        let selected_set = if selected.is_empty() {
            None
        } else {
            Some(selected.iter().cloned().collect::<HashSet<_>>())
        };
        extract_zip_archive(state, &src, &dst, selected_set.as_ref(), conflict)
    } else {
        extract_with_7z(state, &src, &dst, selected, password, conflict)
    }
}

fn archive_has_encrypted_entries(path: &str) -> bool {
    list_archive_entries(Path::new(path), 2_000)
        .map(|entries| entries.iter().any(|entry| entry.encrypted))
        .unwrap_or(false)
}

fn create_zip_archive_impl(state: &AppState, paths: &[String], dest: &Path) -> Result<(), String> {
    let total = paths
        .iter()
        .map(|path| folder_size_quick(Path::new(path), 25_000))
        .sum();
    let op_id = state.queue_start("archive", "", Some(&dest.to_string_lossy()), total);
    let started = Instant::now();
    let file = File::create(dest).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Track bytes written across all files so the queue progress reflects
    // the entire archive operation, not just the current file.
    let mut bytes_done: u64 = 0;

    for p in paths {
        let src = PathBuf::from(p);
        let name = src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if src.is_dir() {
            for entry in WalkDir::new(&src).into_iter().filter_map(Result::ok) {
                let rel = entry.path().strip_prefix(&src).unwrap_or(entry.path());
                let rel = rel.to_string_lossy().replace('\\', "/");
                let entry_name = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{name}/{rel}")
                };
                if entry.file_type().is_dir() {
                    zip.add_directory(&entry_name, opts)
                        .map_err(|e| e.to_string())?;
                } else {
                    zip.start_file(&entry_name, opts)
                        .map_err(|e| e.to_string())?;
                    let f = File::open(entry.path()).map_err(|e| e.to_string())?;
                    let copied =
                        copy_with_progress(f, &mut zip, state, op_id, &mut bytes_done, started)?;
                    let _ = copied;
                }
            }
        } else {
            zip.start_file(&name, opts).map_err(|e| e.to_string())?;
            let f = File::open(&src).map_err(|e| e.to_string())?;
            let copied = copy_with_progress(f, &mut zip, state, op_id, &mut bytes_done, started)?;
            let _ = copied;
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    state.invalidate_path(dest);
    state.queue_finish(op_id, "done", "Archive created", total, started.elapsed());
    Ok(())
}

/// Stream bytes from a reader into a writer in 64 KB chunks, pushing live
/// progress into the operation queue after each chunk. Returns the number
/// of bytes copied.
fn copy_with_progress<R: io::Read, W: io::Write>(
    mut reader: R,
    writer: &mut W,
    state: &AppState,
    op_id: u64,
    running_total: &mut u64,
    started: Instant,
) -> Result<u64, String> {
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(e.to_string()),
        };
        writer.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        total += n as u64;
        *running_total += n as u64;
        state.queue_progress(op_id, *running_total, started);
    }
    Ok(total)
}

fn create_archive_impl(state: &AppState, paths: &[String], dest: &str) -> Result<(), String> {
    let dst = PathBuf::from(dest);
    if dst.exists() {
        return Err(format!("'{}' already exists", dst.display()));
    }
    let ext = archive_format_from_path(&dst);
    if ext == "zip" {
        // Prefer 7-Zip for ZIP creation when present. It uses SIMD-optimized
        // deflate and is typically 2 to 3 times faster than the pure-Rust
        // zip crate on large inputs. Fall back to the internal implementation
        // when 7-Zip is unavailable so we never block creation.
        if let Some(seven_zip) = find_7z() {
            let total = paths
                .iter()
                .map(|path| folder_size_quick(Path::new(path), 25_000))
                .sum();
            let op_id = state.queue_start("archive", "", Some(&dst.to_string_lossy()), total);
            let started = Instant::now();
            let mut command = ProcessCommand::new(seven_zip);
            command
                .arg("a")
                .arg("-tzip")
                // Mid-tier compression (-mx=5) balances speed and ratio. Use
                // multi-thread deflate when 7-Zip supports it (-mmt=on).
                .arg("-mx=5")
                .arg("-mmt=on")
                .arg(&dst);
            for path in paths {
                command.arg(path);
            }
            command.no_window();
            match command.output() {
                Ok(output) if output.status.success() => {
                    state.invalidate_path(&dst);
                    state.queue_finish(op_id, "done", "Archive created", total, started.elapsed());
                    return Ok(());
                }
                Ok(output) => {
                    // 7-Zip ran but failed (corrupt input, permission, etc.).
                    // Surface that error rather than silently falling back.
                    let error = String::from_utf8_lossy(&output.stderr).to_string();
                    state.queue_finish(op_id, "failed", error.clone(), 0, started.elapsed());
                    return Err(error);
                }
                Err(_) => {
                    // 7-Zip binary disappeared between find_7z() and exec.
                    // Fall through to the internal zip writer below.
                    state.queue_finish(
                        op_id,
                        "failed",
                        "7-Zip launch failed".to_string(),
                        0,
                        started.elapsed(),
                    );
                }
            }
        }
        return create_zip_archive_impl(state, paths, &dst);
    }

    let seven_zip = find_7z().ok_or_else(|| {
        format!(
            "{} creation needs 7-Zip installed or available on PATH.",
            ext.to_uppercase()
        )
    })?;
    let total = paths
        .iter()
        .map(|path| folder_size_quick(Path::new(path), 25_000))
        .sum();
    let op_id = state.queue_start("archive", "", Some(&dst.to_string_lossy()), total);
    let started = Instant::now();
    let archive_type = match ext.as_str() {
        "7z" => "7z",
        "tar" | "tar.gz" | "tgz" => "tgzip",
        _ => "7z",
    };
    let mut command = ProcessCommand::new(seven_zip);
    command.arg("a").arg(format!("-t{archive_type}")).arg(&dst);
    for path in paths {
        command.arg(path);
    }
    command.no_window();
    let output = command.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        state.invalidate_path(&dst);
        state.queue_finish(op_id, "done", "Archive created", total, started.elapsed());
        Ok(())
    } else {
        let error = String::from_utf8_lossy(&output.stderr).to_string();
        state.queue_finish(op_id, "failed", error.clone(), 0, started.elapsed());
        Err(error)
    }
}

fn archive_format_from_path(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tar.gz") {
        "tar.gz".to_string()
    } else if name.ends_with(".tgz") {
        "tgz".to_string()
    } else {
        extension(path)
    }
}

#[tauri::command]
fn extract_archive(state: State<'_, AppState>, path: String, dest: String) -> Result<(), String> {
    extract_archive_impl(&state, &path, &dest, &[], None, "keep")
}

#[tauri::command]
fn list_archive(path: String, max_items: Option<usize>) -> Result<Vec<ArchiveEntry>, String> {
    list_archive_entries(Path::new(&path), max_items.unwrap_or(500).min(5_000))
}

#[tauri::command]
fn extract_archive_selected(
    state: State<'_, AppState>,
    path: String,
    dest: String,
    selected: Vec<String>,
    password: Option<String>,
    conflict: Option<String>,
) -> Result<(), String> {
    extract_archive_impl(
        &state,
        &path,
        &dest,
        &selected,
        password.as_deref(),
        conflict.as_deref().unwrap_or("keep"),
    )
}

#[tauri::command]
fn create_archive(
    state: State<'_, AppState>,
    paths: Vec<String>,
    dest: String,
) -> Result<(), String> {
    create_archive_impl(&state, &paths, &dest)
}

// ----- saved searches -----

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

// ----- session -----

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

// ----- operation log / undo -----

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
        "delete" => {
            undo_delete_from_trash(op.trash_id.as_deref())?;
            Ok(format!("Restored '{}'", op.from))
        }
        "batch_rename" => {
            let ops = op.batch.ok_or("Missing batch rename metadata")?;
            for item in ops.iter().rev() {
                let old_name = Path::new(&item.from)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| item.from.clone());
                let src = Path::new(&item.to);
                let dst = src.parent().map(|p| p.join(&old_name)).ok_or("No parent")?;
                if dst.exists() {
                    return Err(format!("'{}' already exists", dst.display()));
                }
                fs::rename(src, &dst).map_err(|e| e.to_string())?;
                state.invalidate_path(src);
                state.invalidate_path(&dst);
            }
            Ok(format!("Undid batch rename ({} items)", ops.len()))
        }
        _ => Err(format!("Cannot undo '{}'", op.kind)),
    }
}

fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "tif"
            | "tiff"
            | "tga"
            | "heic"
            | "heif"
    )
}

fn is_thumbnail_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "tga"
    )
}

fn is_inline_preview_image_ext(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "ico")
}

#[derive(Clone, Copy)]
enum ImageToolAction {
    RotateLeft,
    RotateRight,
    ResizeHalf,
    ResizeQuarter,
    ResizePct(u32),
    ConvertJpeg,
    ConvertPng,
    ConvertWebp,
    CompressJpeg,
    StripMetadata,
}

impl ImageToolAction {
    fn from_command(command: &str) -> Option<Self> {
        match command {
            "rotate-left" => Some(Self::RotateLeft),
            "rotate-right" => Some(Self::RotateRight),
            "resize-half" => Some(Self::ResizeHalf),
            "resize-quarter" => Some(Self::ResizeQuarter),
            "convert-jpeg" => Some(Self::ConvertJpeg),
            "convert-png" => Some(Self::ConvertPng),
            "convert-webp" => Some(Self::ConvertWebp),
            "compress-jpeg" => Some(Self::CompressJpeg),
            "strip-metadata" => Some(Self::StripMetadata),
            _ => {
                if let Some(rest) = command.strip_prefix("resize-pct-") {
                    rest.parse::<u32>()
                        .ok()
                        .filter(|&n| n > 0 && n <= 400)
                        .map(Self::ResizePct)
                } else {
                    None
                }
            }
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::RotateLeft => "Rotated left",
            Self::RotateRight => "Rotated right",
            Self::ResizeHalf => "Resized to 50%",
            Self::ResizeQuarter => "Resized to 25%",
            Self::ResizePct(_) => "Resized",
            Self::ConvertJpeg => "Converted to JPEG",
            Self::ConvertPng => "Converted to PNG",
            Self::ConvertWebp => "Converted to WebP",
            Self::CompressJpeg => "Compressed JPEG",
            Self::StripMetadata => "Stripped metadata",
        }
    }

    fn suffix(&self) -> String {
        match self {
            Self::RotateLeft => "rotated-left".to_string(),
            Self::RotateRight => "rotated-right".to_string(),
            Self::ResizeHalf => "50pct".to_string(),
            Self::ResizeQuarter => "25pct".to_string(),
            Self::ResizePct(n) => format!("{n}pct"),
            Self::ConvertJpeg => "jpeg".to_string(),
            Self::ConvertPng => "png".to_string(),
            Self::ConvertWebp => "webp".to_string(),
            Self::CompressJpeg => "compressed".to_string(),
            Self::StripMetadata => "clean".to_string(),
        }
    }
}

fn image_output_path(source: &Path, suffix: &str, output_ext: &str) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());
    let parent = source.parent().map(Path::to_path_buf).unwrap_or_default();
    keep_both_destination(&parent.join(format!("{stem}-{suffix}.{output_ext}")))
}

fn safe_image_output_ext(source: &Path) -> String {
    match extension(source).as_str() {
        "jpg" | "jpeg" => "jpg".to_string(),
        "png" => "png".to_string(),
        "webp" => "webp".to_string(),
        "bmp" => "bmp".to_string(),
        // GIF animation is not preserved by the image crate's single-frame path,
        // so write edited copies as PNG rather than pretending it stayed animated.
        _ => "png".to_string(),
    }
}

fn save_jpeg_image(img: &image::DynamicImage, dest: &Path, quality: u8) -> Result<(), String> {
    let rgb = img.to_rgb8();
    let mut file = File::create(dest).map_err(|e| e.to_string())?;
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, quality);
    encoder
        .encode(
            &rgb,
            rgb.width(),
            rgb.height(),
            image::ColorType::Rgb8.into(),
        )
        .map_err(|e| e.to_string())
}

fn save_image_with_extension(
    img: &image::DynamicImage,
    dest: &Path,
    ext: &str,
) -> Result<(), String> {
    match ext {
        "jpg" | "jpeg" => save_jpeg_image(img, dest, 92),
        "png" => img
            .save_with_format(dest, image::ImageFormat::Png)
            .map_err(|e| e.to_string()),
        "webp" => img
            .save_with_format(dest, image::ImageFormat::WebP)
            .map_err(|e| e.to_string()),
        "bmp" => img
            .save_with_format(dest, image::ImageFormat::Bmp)
            .map_err(|e| e.to_string()),
        _ => img
            .save_with_format(dest, image::ImageFormat::Png)
            .map_err(|e| e.to_string()),
    }
}

fn process_image_tool(source: &Path, action: ImageToolAction) -> Result<PathBuf, String> {
    if !source.is_file() || !is_thumbnail_image_ext(&extension(source)) {
        return Err(format!("'{}' is not a supported image", source.display()));
    }

    let img = image::open(source).map_err(|e| e.to_string())?;
    let source_ext = safe_image_output_ext(source);
    let suffix = action.suffix();

    match action {
        ImageToolAction::RotateLeft => {
            let dest = image_output_path(source, &suffix, &source_ext);
            save_image_with_extension(&img.rotate270(), &dest, &source_ext)?;
            Ok(dest)
        }
        ImageToolAction::RotateRight => {
            let dest = image_output_path(source, &suffix, &source_ext);
            save_image_with_extension(&img.rotate90(), &dest, &source_ext)?;
            Ok(dest)
        }
        ImageToolAction::ResizeHalf | ImageToolAction::ResizeQuarter => {
            let factor = if matches!(action, ImageToolAction::ResizeHalf) {
                2
            } else {
                4
            };
            let width = (img.width() / factor).max(1);
            let height = (img.height() / factor).max(1);
            let resized = img.resize(width, height, image::imageops::FilterType::Lanczos3);
            let dest = image_output_path(source, &suffix, &source_ext);
            save_image_with_extension(&resized, &dest, &source_ext)?;
            Ok(dest)
        }
        ImageToolAction::ResizePct(pct) => {
            let scale = pct as f32 / 100.0;
            let width = ((img.width() as f32 * scale).round() as u32).max(1);
            let height = ((img.height() as f32 * scale).round() as u32).max(1);
            let resized = img.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
            let dest = image_output_path(source, &suffix, &source_ext);
            save_image_with_extension(&resized, &dest, &source_ext)?;
            Ok(dest)
        }
        ImageToolAction::ConvertJpeg => {
            let dest = image_output_path(source, &suffix, "jpg");
            save_jpeg_image(&img, &dest, 92)?;
            Ok(dest)
        }
        ImageToolAction::ConvertPng => {
            let dest = image_output_path(source, &suffix, "png");
            save_image_with_extension(&img, &dest, "png")?;
            Ok(dest)
        }
        ImageToolAction::ConvertWebp => {
            let dest = image_output_path(source, &suffix, "webp");
            save_image_with_extension(&img, &dest, "webp")?;
            Ok(dest)
        }
        ImageToolAction::CompressJpeg => {
            let dest = image_output_path(source, &suffix, "jpg");
            save_jpeg_image(&img, &dest, 76)?;
            Ok(dest)
        }
        ImageToolAction::StripMetadata => {
            let dest = image_output_path(source, &suffix, &source_ext);
            save_image_with_extension(&img, &dest, &source_ext)?;
            Ok(dest)
        }
    }
}

fn thumbnail_cache_key(path: &Path, modified: u64, px: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cache_key(path));
    hasher.update(modified.to_le_bytes());
    hasher.update(px.to_le_bytes());
    hex::encode(hasher.finalize())
}

fn thumbnail_cache_size() -> u64 {
    open_index_connection()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COALESCE(SUM(byte_len), 0) FROM thumbnail_cache",
                [],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|n| n.max(0) as u64)
        })
        .unwrap_or_else(|| {
            fs::read_dir(thumbnail_cache_dir())
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.metadata().ok().map(|m| m.len()))
                .sum()
        })
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

    THUMBNAIL_POOL.install(|| {
        paths
            .par_iter()
            .filter_map(|path| {
                let path_buf = PathBuf::from(path);
                if !is_thumbnail_image_ext(&extension(&path_buf)) {
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
                let data_url = store_thumbnail_on_disk(
                    &path_buf,
                    mtime,
                    px,
                    &buf,
                    THUMBNAIL_CACHE_LIMIT_BYTES,
                )
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
    })
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
    update_checks_enabled: bool,
    network_downloads_enabled: bool,
    ui_mode: String,
    window_x: i32,
    window_y: i32,
    window_w: u32,
    window_h: u32,
    window_maximized: bool,
    /// Toolbar: rank indexed hits by on-device text embedding similarity.
    search_semantic_mode: bool,
    /// Reserved for optional CLIP model (not bundled in this build).
    clip_search_enabled: bool,
    /// Suppress the first-run welcome dialog after the user dismisses it once.
    #[serde(default)]
    first_run_welcome_dismissed: bool,
    /// Shown once: tip toast when opening the command palette.
    #[serde(default)]
    palette_tip_shown: bool,
    /// Override folder icon color set on the Appearance tab. None means use
    /// the per-theme defaults from `icon_folder_colors`.
    #[serde(default)]
    folder_color: Option<String>,
    /// User-defined accent hex used when `accent == "custom"`. Stored separately
    /// from the preset id so the hex survives switching to a preset and back.
    #[serde(default)]
    custom_accent_hex: Option<String>,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            theme: "mica-dark".to_string(),
            accent: "blue".to_string(),
            density: "cozy".to_string(),
            wallpaper: "none".to_string(),
            custom_theme: None,
            // v0.9.20: default to "max" so fresh installs index every
            // fixed drive, which makes "Search entire drive" find
            // every file on the first try instead of needing a follow-
            // up scan to expand coverage.
            index_mode: "max".to_string(),
            index_roots: Vec::new(),
            thumbnail_cache_limit_mb: 50,
            // Auto-update check runs once at startup and lights up the green
            // status-bar pill if a newer release is available. Default true so
            // new installs hear about patches without having to dig into Settings.
            update_checks_enabled: true,
            network_downloads_enabled: false,
            ui_mode: String::new(),
            window_x: i32::MIN,
            window_y: i32::MIN,
            window_w: 0,
            window_h: 0,
            window_maximized: false,
            search_semantic_mode: false,
            clip_search_enabled: false,
            first_run_welcome_dismissed: false,
            palette_tip_shown: false,
            folder_color: None,
            custom_accent_hex: None,
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
    #[serde(default)]
    gradient_background: bool,
    #[serde(default)]
    gradient_accent_tip: bool,
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
            ui_font: "noto-sans".to_string(),
            mono_font: "noto-sans-mono".to_string(),
            font_size_delta: 0,
            icon_folder_hex: "#e2a934".to_string(),
            gradient_background: false,
            gradient_accent_tip: false,
        }
    }
}

fn normalize_theme_font_presets(def: &mut ThemeDefinition) {
    def.ui_font = normalize_ui_font_preset(&def.ui_font);
    def.mono_font = normalize_mono_font_preset(&def.mono_font);
}

fn normalize_ui_font_preset(raw: &str) -> String {
    let s = raw.trim().to_ascii_lowercase().replace(" ", "-");
    if s.contains("press") || s.contains("start2p") || s == "press-start-2p" {
        return "press-start-2p".to_string();
    }
    if s == "noto-sans"
        || s.is_empty()
        || s.contains("segoe")
        || s.contains("arial")
        || s.contains("inter")
        || s.contains("system")
    {
        return "noto-sans".to_string();
    }
    "noto-sans".to_string()
}

fn normalize_mono_font_preset(raw: &str) -> String {
    let s = raw.trim().to_ascii_lowercase().replace(" ", "-");
    if s.contains("press") || s.contains("start2p") || s == "press-start-2p" {
        return "press-start-2p".to_string();
    }
    if s.contains("jetbrains") {
        return "jetbrains-mono".to_string();
    }
    if s == "noto-sans-mono"
        || s.is_empty()
        || s.contains("cascadia")
        || s.contains("consolas")
        || s.contains("courier")
    {
        return "noto-sans-mono".to_string();
    }
    "noto-sans-mono".to_string()
}

fn bundled_ui_family_from_preset(preset: &str) -> &'static str {
    match preset {
        "press-start-2p" => "Press Start 2P",
        _ => "Noto Sans",
    }
}

fn bundled_mono_family_from_preset(preset: &str) -> &'static str {
    match preset {
        "jetbrains-mono" => "JetBrains Mono",
        "press-start-2p" => "Press Start 2P",
        _ => "Noto Sans Mono",
    }
}

#[derive(Clone)]
struct NativeClipboard {
    paths: Vec<String>,
    cut: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivePane {
    Primary,
    Secondary,
}

#[derive(Clone)]
struct ArchiveExtractJob {
    source: String,
    dest: String,
    selected: Vec<String>,
}

enum PendingPrompt {
    Rename(String),
    NewFolder,
    NewFile,
    Note(String),
    Archive,
    ArchivePassword {
        jobs: Vec<ArchiveExtractJob>,
        conflict: String,
    },
    NewTemplate(FileTemplate),
    CompareFolder(String),
    SaveWorkspace,
    BatchRename(Vec<String>),
    RenamePreset(Vec<String>),
    RenameConflict { path: String, new_name: String },
    ConflictPaste {
        src: String,
        dest: String,
        cut: bool,
    },
    StorageCleanup(Vec<String>),
    RenameTag(String),
}

/// Sidebar drives + storage scan cache loaded on a background thread after
/// the first frame. Applied on the UI thread via the poll timer (Send-safe).
struct DeferredStartupData {
    drives: Vec<DriveInfo>,
    storage_caches: HashMap<String, StorageScanResult>,
    user_pins: Vec<UserPin>,
    recent_locations: Vec<String>,
}

struct NativeController {
    app_state: AppState,
    current_path: String,
    files: Vec<FileEntry>,
    visible_files: Vec<FileEntry>,
    active_archive: Option<ArchiveView>,
    selected_index: i32,
    selected_set: std::collections::HashSet<usize>,
    select_anchor: i32,
    files_model: Option<ModelRc<FileItem>>,
    search_query: String,
    search_all_scope: bool,
    history: Vec<String>,
    history_index: usize,
    // Per-folder scroll memory keyed by absolute path. Updated whenever we
    // navigate away from a folder; consulted whenever we navigate into one so
    // Back / Up / re-entering a folder restores the row the user was looking at.
    path_scroll: HashMap<String, f32>,
    // Per-drive scan results keyed by root path (e.g. "C:\\"). Lets the user
    // switch drives without re-scanning each time they revisit Storage.
    storage_caches: HashMap<String, StorageScanResult>,
    storage_scan_pending: Arc<Mutex<Option<(u64, Option<StorageScanResult>)>>>,
    storage_scan_ready: Arc<AtomicBool>,
    storage_scan_generation: Arc<AtomicU64>,
    storage_scan_active: bool,
    storage_show_all_state: bool,
    storage_path_before: String,
    // Live progress counters the background scan thread updates; the polling
    // tick reads them and pushes into Slint properties so the user sees a
    // smooth files/bytes counter and a progress bar while scanning.
    storage_progress: Arc<StorageScanProgress>,
    storage_current_root: String,
    storage_selected_bucket: String,
    // v0.9.14: drill-in search filter (case-insensitive substring).
    // Empty = no filter. Re-applied on every push_storage_top_items
    // call so the bound model and the input box stay in sync.
    storage_drill_search: String,
    // v0.9.21: drill-in sort mode - "size" (default), "name", or "recent".
    // Applied in push_storage_top_items after merge/filter so the user can
    // re-order any bucket/full list without a rescan.
    storage_sort_mode: String,
    // Snapshots of the preview pane's prior visibility + width so closing
    // the storage view restores the pane to exactly what the user had
    // before opening Storage. Captured in open_storage_view.
    storage_preview_visible_before: bool,
    storage_preview_w_before: f32,
    storage_subtitle_last_update: Instant,
    // Incremented on every folder navigation. Background directory workers
    // stamp results with the generation they started under; stale loads are
    // dropped so rapid sidebar clicks don't queue blocking I/O on the UI thread.
    nav_generation: Arc<AtomicU64>,
    // Coalesced sidebar navigation target. A short debounce timer collapses
    // rapid sidebar clicks into a single navigate() call (last click wins).
    sidebar_nav_pending: Option<String>,
    sidebar_nav_timer: Option<slint::Timer>,
    // Total used bytes on the current scan root (from GetDiskFreeSpaceExW).
    // Used as the progress-bar denominator so % shown is real progress vs.
    // the actual amount of data on the drive, not just "bytes seen so far".
    storage_disk_used: u64,
    drive_space_cache: HashMap<String, (u64, u64, Instant)>,
    tabs: Vec<SessionTab>,
    active_tab: usize,
    known_folders: Vec<KnownFolder>,
    drives: Vec<DriveInfo>,
    user_pins: Vec<UserPin>,
    recent_locations: Vec<String>,
    folder_views: HashMap<String, String>,
    // When false, entries starting with `.` and entries with the .ini
    // extension (desktop.ini, thumbs.ini, etc.) are filtered out of
    // visible_files in apply_filter. Toggled from the UI show-hidden control.
    show_hidden: bool,
    // Shared progress for the Local AI installer. Background thread writes,
    // UI polling timer reads and pushes into Slint properties.
    ai_progress: Arc<local_ai::InstallProgress>,
    // Shell-extracted system icons. Per-extension covers the common case
    // (every .docx shares one icon). Per-path is used for .exe / .lnk /
    // .ico / .msi where each file may carry its own embedded icon.
    #[cfg(target_os = "windows")]
    system_icon_by_ext: HashMap<String, slint::Image>,
    #[cfg(target_os = "windows")]
    system_icon_by_path: HashMap<String, slint::Image>,
    tags: HashMap<String, String>,
    tag_labels: HashMap<String, String>,
    notes: HashMap<String, String>,
    secondary_path: String,
    secondary_history: Vec<String>,
    secondary_history_pos: usize,
    secondary_sort_by: String,
    secondary_sort_dir: String,
    secondary_files: Vec<FileEntry>,
    secondary_visible_files: Vec<FileEntry>,
    secondary_selected_index: i32,
    secondary_selected_set: std::collections::HashSet<usize>,
    secondary_select_anchor: i32,
    secondary_files_model: Option<ModelRc<FileItem>>,
    active_pane: ActivePane,
    folder_filter: String,
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
    toast_current_kind: String,
    toast_current_message: String,
    toast_last_shown: Option<std::time::Instant>,
    toast_timer: Option<slint::Timer>,
    git_status_ready: Arc<std::sync::atomic::AtomicBool>,
    pending_git_status: Arc<Mutex<Option<Arc<GitStatusMap>>>>,
    operation_ready: Arc<std::sync::atomic::AtomicBool>,
    pending_operation_result: Arc<Mutex<Option<NativeOperationResult>>>,
    directory_ready: Arc<std::sync::atomic::AtomicBool>,
    pending_directory_result: Arc<Mutex<Option<NativeDirectoryResult>>>,
    search_ready: Arc<std::sync::atomic::AtomicBool>,
    pending_search_result: Arc<Mutex<Option<NativeSearchResult>>>,
    pending_deferred_startup: Arc<Mutex<Option<DeferredStartupData>>>,
    /// Deferred shell-icon + thumbnail disk reads after the first file list paints.
    enrich_visible_pending: bool,
    compare_hide_same: bool,
    compare_left: String,
    compare_right: String,
    compare_all_rows: Vec<FolderCompareEntry>,
    dupe_groups_cache: Vec<(String, Vec<String>)>,
    shortcut_draft: HashMap<String, String>,
    recent_commands: std::collections::VecDeque<String>,
    // Async file preview. Reading a file body (PDF text extraction, archive
    // listing, large text) can be slow, so it runs on a background thread and
    // the result is applied by the poll tick. `preview_generation` stamps each
    // request; stale results (the user already clicked something else) are
    // dropped so the pane never flashes an out-of-date body.
    preview_generation: Arc<AtomicU64>,
    preview_ready: Arc<AtomicBool>,
    pending_preview_result: Arc<Mutex<Option<PreviewResult>>>,
}

#[derive(Clone)]
struct PreviewResult {
    generation: u64,
    body: String,
    meta: String,
    rendered: String,
    // Rasterized PDF first page (RGBA + dimensions), built off the UI thread
    // and turned into a Slint image when the poll tick applies the result.
    pdf_image: Option<Arc<(Vec<u8>, u32, u32)>>,
}

#[derive(Clone)]
struct NativeOperationResult {
    message: String,
    kind: String,
    refresh: bool,
    /// After a cut/move, refresh both dual-pane listings (source pane often still
    /// showed moved files and looked like a copy).
    refresh_both_panes: bool,
    secondary_refresh_path: Option<String>,
    clear_clipboard: bool,
    /// Directory paths whose listing cache must be dropped after paste/move.
    invalidate_dirs: Vec<String>,
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

#[derive(Debug, Serialize, Deserialize, Default)]
struct StorageScanCacheFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    results: HashMap<String, StorageScanResult>,
}

fn read_storage_scan_cache() -> HashMap<String, StorageScanResult> {
    let path = native_data_file("storage_scan_cache.json");
    let Ok(data) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    if let Ok(file) = serde_json::from_str::<StorageScanCacheFile>(&data) {
        let mut out = file.results;
        for result in out.values_mut() {
            migrate_storage_scan_result(result);
        }
        return out;
    }
    // Legacy single-result cache file.
    if let Ok(mut result) = serde_json::from_str::<StorageScanResult>(&data) {
        migrate_storage_scan_result(&mut result);
        let root = result.root.clone();
        let mut map = HashMap::new();
        map.insert(root, result);
        return map;
    }
    HashMap::new()
}

fn trim_storage_scan_cache(map: &mut HashMap<String, StorageScanResult>) {
    while map.len() > STORAGE_CACHE_MAX_DRIVES {
        let oldest = map
            .iter()
            .min_by_key(|(_, r)| r.scanned_at)
            .map(|(k, _)| k.clone());
        if let Some(key) = oldest {
            map.remove(&key);
        } else {
            break;
        }
    }
}

fn write_storage_scan_cache_entry(result: &StorageScanResult) -> Result<(), String> {
    let path = native_data_file("storage_scan_cache.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut map = read_storage_scan_cache();
    map.insert(result.root.clone(), result.clone());
    trim_storage_scan_cache(&mut map);
    let file = StorageScanCacheFile {
        version: 1,
        results: map,
    };
    let data = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    fs::write(path, data).map_err(|e| e.to_string())
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

fn theme_icons_dir() -> PathBuf {
    let dir = native_data_dir().join("theme_icons");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Load a per-theme folder icon PNG from `%APPDATA%\Pathfinder\theme_icons\{id}.png`.
/// Returns the default (empty, width=0) image if the file is missing or unreadable;
/// the Slint UI uses `width > 0px` to decide whether to overlay it on the fallback shape.
///
/// Restricted to themes whose folder visual is intentionally an image (cyberpunk
/// and terminal - they were designed as PNGs with non-removable borders). Every
/// other theme uses the procedural themed folder shape so palette tints flow
/// through cleanly. This prevents stale `%APPDATA%` icon files from leaking into
/// themes the user expects to render via the standard folder.
fn load_theme_folder_icon(id: &str) -> slint::Image {
    if id != "cyberpunk" && id != "terminal" {
        return slint::Image::default();
    }
    let path = theme_icons_dir().join(format!("{id}.png"));
    if path.exists() {
        slint::Image::load_from_path(&path).unwrap_or_default()
    } else {
        slint::Image::default()
    }
}

fn icon_folder_colors(id: &str) -> (Color, Color) {
    // (top-tab / body-bottom). The standard folder shape draws a vertical
    // gradient from icon_folder_1 -> icon_folder_2 across the body, with the
    // tab using icon_folder_1 directly.
    match id {
        "terminal" => (color("#9cffd8"), color("#45c97a")),
        "retro" => (color("#ffee8a"), color("#c02890")),
        "cyberpunk" => (color("#ff6ae0"), color("#d000a0")),
        "sunset" => (color("#ffb87a"), color("#d8421c")),
        "frost" => (color("#cfe6ff"), color("#5ea0d8")),
        "warm" => (color("#e0a070"), color("#9c5818")),
        "paper" => (color("#e6d4a4"), color("#b6915c")),
        "fantasy" => (color("#e8c478"), color("#3a2a14")),
        "flat" => (color("#cfd5dc"), color("#7c8a9c")),
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
        .and_then(|data| serde_json::from_str::<ThemeDefinition>(&data).ok())
        .map(|mut def| {
            normalize_theme_font_presets(&mut def);
            def
        })
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
        .no_window()
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
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAGS_AND_ATTRIBUTES, GetFileAttributesW, SetFileAttributesW,
        };
        use windows::core::PCWSTR;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let pcwstr = PCWSTR(wide.as_ptr());
        unsafe {
            let attrs = GetFileAttributesW(pcwstr);
            if attrs != u32::MAX {
                let _ = SetFileAttributesW(pcwstr, FILE_FLAGS_AND_ATTRIBUTES(attrs | 0x2));
            }
        }
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
    conn.pragma_update(None, "temp_store", "MEMORY")
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

        CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
            path UNINDEXED,
            parent UNINDEXED,
            name,
            extension,
            tokenize = 'unicode61'
        );

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

        CREATE TABLE IF NOT EXISTS path_embeddings (
            path TEXT PRIMARY KEY,
            emb BLOB NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_path_embeddings_prefix ON path_embeddings(path);

        CREATE TABLE IF NOT EXISTS image_dhash (
            path TEXT PRIMARY KEY,
            dhash BLOB NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS image_desc_embeddings (
            path TEXT PRIMARY KEY,
            emb BLOB NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_image_desc_embeddings_prefix ON image_desc_embeddings(path);
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

/// Schedule an index operation with throttling so rapid file system events
/// (e.g., recycle bin cascades or batch delete) don't pile up SQLite writes.
/// Indexes if the path hasn't been indexed within the last 300ms. Also evicts
/// map entries older than 5s on every call so the bookkeeping doesn't grow
/// without bound for long-running sessions.
fn schedule_index_directory_debounced(state: &AppState, parent: String, entries: Vec<FileEntry>) {
    const THROTTLE_MS: u64 = 300;
    const EVICT_AFTER: Duration = Duration::from_secs(5);

    let should_index = {
        // Single lock: read last-index time, evict stale entries, write new
        // timestamp, all in one critical section. Avoids a TOCTOU window
        // where two threads could both decide to index.
        let mut debounce = state
            .index_debounce
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        debounce.retain(|_, t| now.duration_since(*t) < EVICT_AFTER);
        let ok = match debounce.get(&parent).copied() {
            None => true,
            Some(last) => now.duration_since(last) > Duration::from_millis(THROTTLE_MS),
        };
        if ok {
            debounce.insert(parent.clone(), now);
        }
        ok
    };

    if should_index {
        schedule_index_directory(parent, entries);
    }
}

fn index_directory_entries(parent: &str, entries: &[FileEntry]) -> Result<(), String> {
    let parent = cache_key_str(parent);
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
        let mut delete_fts = tx
            .prepare("DELETE FROM files_fts WHERE path = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_fts = tx
            .prepare(
                "
                INSERT INTO files_fts(path, parent, name, extension)
                VALUES(?1, ?2, ?3, ?4)
                ",
            )
            .map_err(|e| e.to_string())?;

        for entry in entries {
            let extension = entry.extension.as_deref().unwrap_or("").to_lowercase();
            upsert
                .execute(params![
                    entry.path,
                    parent,
                    entry.name,
                    extension,
                    i64::from(entry.kind == FileKind::Directory),
                    entry.size as i64,
                    entry.modified as i64,
                    now
                ])
                .map_err(|e| e.to_string())?;
            delete_fts
                .execute(params![entry.path])
                .map_err(|e| e.to_string())?;
            insert_fts
                .execute(params![entry.path, parent, entry.name, extension])
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
        tx.execute("DELETE FROM files_fts WHERE path = ?1", params![path])
            .map_err(|e| e.to_string())?;
        let _ = tx.execute("DELETE FROM path_embeddings WHERE path = ?1", params![path]);
        let _ = tx.execute("DELETE FROM image_dhash WHERE path = ?1", params![path]);
        let _ = tx.execute(
            "DELETE FROM image_desc_embeddings WHERE path = ?1",
            params![path],
        );
    }

    tx.commit().map_err(|e| e.to_string())?;

    // Embeddings and dHashes are expensive (ONNX + disk I/O). Run them after the
    // main FTS transaction commits so readers are not blocked for as long.
    let mut emb_rows: Vec<(String, Vec<u8>, i64)> = Vec::new();
    let mut dhash_rows: Vec<(String, Vec<u8>, i64)> = Vec::new();
    let index_image_desc =
        crate::local_ai::core_models_installed() && crate::inference::image_classifier_available();
    let mut img_desc_label_pairs: Vec<(String, String)> = Vec::new();
    let mut img_desc_clear_paths: Vec<String> = Vec::new();
    for entry in entries {
        if entry.kind == FileKind::Directory {
            continue;
        }
        let extension = entry.extension.as_deref().unwrap_or("").to_lowercase();
        if let Some(vec) = crate::inference::embed_file_label(&entry.name) {
            let mut blob = Vec::with_capacity(vec.len() * 4);
            for x in &vec {
                blob.extend_from_slice(&x.to_le_bytes());
            }
            emb_rows.push((entry.path.clone(), blob, now));
        }
        if matches!(
            extension.as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
        ) && let Some(h) = crate::inference::dhash64(Path::new(&entry.path))
        {
            dhash_rows.push((entry.path.clone(), h.to_le_bytes().to_vec(), now));
        }
        if index_image_desc
            && matches!(
                extension.as_str(),
                "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
            )
        {
            let p = Path::new(&entry.path);
            if let Some(labels) = crate::inference::image_search_label_text(p) {
                img_desc_label_pairs.push((entry.path.clone(), labels));
            } else {
                img_desc_clear_paths.push(entry.path.clone());
            }
        }
    }
    let mut img_desc_rows: Vec<(String, Vec<u8>, i64)> = Vec::new();
    if index_image_desc && !img_desc_label_pairs.is_empty() {
        let label_refs: Vec<&str> = img_desc_label_pairs
            .iter()
            .map(|(_, s)| s.as_str())
            .collect();
        let batch_emb = crate::inference::embed_file_labels_batch(&label_refs);
        for ((path, _), emb_opt) in img_desc_label_pairs.iter().zip(batch_emb) {
            if let Some(vec) = emb_opt {
                let mut blob = Vec::with_capacity(vec.len() * 4);
                for x in &vec {
                    blob.extend_from_slice(&x.to_le_bytes());
                }
                img_desc_rows.push((path.clone(), blob, now));
            } else {
                img_desc_clear_paths.push(path.clone());
            }
        }
    }
    if !emb_rows.is_empty()
        || !dhash_rows.is_empty()
        || !img_desc_rows.is_empty()
        || (index_image_desc && !img_desc_clear_paths.is_empty())
    {
        let tx_ai = conn.transaction().map_err(|e| e.to_string())?;
        for (path, blob, ts) in emb_rows {
            let _ = tx_ai.execute(
                "INSERT INTO path_embeddings(path, emb, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET emb = excluded.emb, updated_at = excluded.updated_at",
                params![path, blob, ts],
            );
        }
        for (path, dh_blob, ts) in dhash_rows {
            let _ = tx_ai.execute(
                "INSERT INTO image_dhash(path, dhash, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET dhash = excluded.dhash, updated_at = excluded.updated_at",
                params![path, dh_blob, ts],
            );
        }
        for (path, blob, ts) in img_desc_rows {
            let _ = tx_ai.execute(
                "INSERT INTO image_desc_embeddings(path, emb, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET emb = excluded.emb, updated_at = excluded.updated_at",
                params![path, blob, ts],
            );
        }
        if index_image_desc {
            for path in img_desc_clear_paths {
                let _ = tx_ai.execute(
                    "DELETE FROM image_desc_embeddings WHERE path = ?1",
                    params![path],
                );
            }
        }
        tx_ai.commit().map_err(|e| e.to_string())?;
    }

    let _ = conn.execute_batch("PRAGMA incremental_vacuum(16); PRAGMA optimize;");
    Ok(())
}

fn like_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn sqlite_limit(max: usize) -> i64 {
    i64::try_from(max).unwrap_or(i64::MAX)
}

fn embedding_blob_to_vec(blob: &[u8]) -> Option<Vec<f32>> {
    if !blob.len().is_multiple_of(4) {
        return None;
    }
    let mut v = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        v.push(f32::from_le_bytes(chunk.try_into().ok()?));
    }
    Some(v)
}

fn semantic_scores_under_root(root: &str, query_emb: &[f32]) -> HashMap<String, f32> {
    let Ok(conn) = open_index_connection() else {
        return HashMap::new();
    };
    let root_prefix = format!("{}%", root.trim_end_matches(['\\', '/']));
    let sql = "SELECT f.path, e.emb FROM files f \
         INNER JOIN path_embeddings e ON e.path = f.path \
         WHERE f.path LIKE ?1 ESCAPE '\\' AND f.is_dir = 0 LIMIT 8000";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return HashMap::new();
    };
    let rows = stmt.query_map(params![root_prefix], |row| {
        let path: String = row.get(0)?;
        let emb: Vec<u8> = row.get(1)?;
        Ok((path, emb))
    });
    let Ok(rows) = rows else {
        return HashMap::new();
    };
    let rows_vec: Vec<(String, Vec<u8>)> = rows.flatten().collect();
    let qdim = query_emb.len();
    rows_vec
        .into_par_iter()
        .filter_map(|(path, blob)| {
            let vec = embedding_blob_to_vec(&blob)?;
            if vec.len() != qdim {
                return None;
            }
            let s = crate::inference::cosine_similarity(query_emb, &vec);
            Some((path, s))
        })
        .collect()
}

fn image_desc_scores_under_root(root: &str, query_emb: &[f32]) -> HashMap<String, f32> {
    let Ok(conn) = open_index_connection() else {
        return HashMap::new();
    };
    let root_prefix = format!("{}%", root.trim_end_matches(['\\', '/']));
    let sql = "SELECT f.path, e.emb FROM files f \
         INNER JOIN image_desc_embeddings e ON e.path = f.path \
         WHERE f.path LIKE ?1 ESCAPE '\\' AND f.is_dir = 0 LIMIT 8000";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return HashMap::new();
    };
    let rows = stmt.query_map(params![root_prefix], |row| {
        let path: String = row.get(0)?;
        let emb: Vec<u8> = row.get(1)?;
        Ok((path, emb))
    });
    let Ok(rows) = rows else {
        return HashMap::new();
    };
    let rows_vec: Vec<(String, Vec<u8>)> = rows.flatten().collect();
    let qdim = query_emb.len();
    rows_vec
        .into_par_iter()
        .filter_map(|(path, blob)| {
            let vec = embedding_blob_to_vec(&blob)?;
            if vec.len() != qdim {
                return None;
            }
            let s = crate::inference::cosine_similarity(query_emb, &vec);
            Some((path, s))
        })
        .collect()
}

/// Re-rank indexed search hits using filename embeddings, image-tag embeddings, or both.
fn apply_semantic_search_ranking_entries(
    root: &str,
    query: &str,
    search_semantic_mode: bool,
    clip_search_enabled: bool,
    entries: &mut [FileEntry],
) {
    let trimmed = query.trim();
    if trimmed.len() < 2
        || trimmed.starts_with("tag:")
        || trimmed.starts_with("smart:")
        || entries.is_empty()
    {
        return;
    }
    if !search_semantic_mode && !clip_search_enabled {
        return;
    }
    let Some(qemb) = crate::inference::embed_query_text(trimmed) else {
        return;
    };
    let text_scores = if search_semantic_mode {
        semantic_scores_under_root(root, &qemb)
    } else {
        HashMap::new()
    };
    let img_scores = if clip_search_enabled {
        image_desc_scores_under_root(root, &qemb)
    } else {
        HashMap::new()
    };
    if text_scores.is_empty() && img_scores.is_empty() {
        return;
    }
    entries.sort_by(|a, b| {
        let ta = text_scores.get(&a.path).copied().unwrap_or(0.0);
        let tb = text_scores.get(&b.path).copied().unwrap_or(0.0);
        let ia = img_scores.get(&a.path).copied().unwrap_or(0.0);
        let ib = img_scores.get(&b.path).copied().unwrap_or(0.0);
        let sa = match (search_semantic_mode, clip_search_enabled) {
            (true, true) => ta.max(ia),
            (true, false) => ta,
            (false, true) => ia,
            (false, false) => 0.0,
        };
        let sb = match (search_semantic_mode, clip_search_enabled) {
            (true, true) => tb.max(ib),
            (true, false) => tb,
            (false, true) => ib,
            (false, false) => 0.0,
        };
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| natural_cmp(&a.name_lower, &b.name_lower))
    });
}

fn scan_image_duplicates_in_folder(folder: &str) -> String {
    let Ok(conn) = open_index_connection() else {
        return "Index database unavailable.".into();
    };
    let prefix = format!("{}%", folder.trim_end_matches(['\\', '/']));
    let sql = "SELECT path, dhash FROM image_dhash WHERE path LIKE ?1 ESCAPE '\\'";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return "Could not query image hashes.".into();
    };
    let rows = stmt.query_map(params![prefix], |row| {
        let path: String = row.get(0)?;
        let dh: Vec<u8> = row.get(1)?;
        Ok((path, dh))
    });
    let Ok(rows) = rows else {
        return "Query failed.".into();
    };
    let mut entries: Vec<(String, u64)> = Vec::new();
    for r in rows {
        let Ok((path, blob)) = r else { continue };
        if blob.len() != 8 {
            continue;
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&blob);
        entries.push((path, u64::from_le_bytes(arr)));
    }
    let mut dup_groups = 0usize;
    let mut seen: HashSet<usize> = HashSet::new();
    for i in 0..entries.len() {
        if seen.contains(&i) {
            continue;
        }
        let mut group = vec![i];
        for j in (i + 1)..entries.len() {
            if seen.contains(&j) {
                continue;
            }
            if crate::inference::hamming64(entries[i].1, entries[j].1) == 0 {
                group.push(j);
            }
        }
        if group.len() > 1 {
            dup_groups += 1;
            for &idx in &group {
                seen.insert(idx);
            }
        }
    }
    if dup_groups == 0 {
        "No exact duplicate image hashes in this folder (index more images first).".into()
    } else {
        format!("Found {dup_groups} group(s) of identical dHash values under this folder.")
    }
}

fn upsert_index_entries(entries: &[FileEntry]) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut conn = open_index_connection()?;
    let now = now_unix_secs() as i64;
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
        let mut delete_fts = tx
            .prepare("DELETE FROM files_fts WHERE path = ?1")
            .map_err(|e| e.to_string())?;
        let mut insert_fts = tx
            .prepare("INSERT INTO files_fts(path, parent, name, extension) VALUES(?1, ?2, ?3, ?4)")
            .map_err(|e| e.to_string())?;

        for entry in entries {
            let parent = Path::new(&entry.path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let extension = entry.extension.as_deref().unwrap_or("").to_lowercase();
            upsert
                .execute(params![
                    entry.path,
                    parent,
                    entry.name,
                    extension,
                    i64::from(entry.kind == FileKind::Directory),
                    entry.size as i64,
                    entry.modified as i64,
                    now
                ])
                .map_err(|e| e.to_string())?;
            delete_fts
                .execute(params![entry.path])
                .map_err(|e| e.to_string())?;
            insert_fts
                .execute(params![entry.path, parent, entry.name, extension])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

fn fts_query_for(query: &str) -> Option<String> {
    let parsed = parse_query(query);
    let mut terms = parsed.terms;
    terms.extend(parsed.name);
    terms.extend(parsed.ext);
    terms.extend(parsed.kind);
    let cleaned: Vec<String> = terms
        .into_iter()
        .flat_map(|term| {
            term.split(|c: char| !c.is_alphanumeric())
                .filter(|part| part.len() >= 2)
                .map(|part| format!("{}*", part.to_lowercase()))
                .collect::<Vec<_>>()
        })
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.join(" AND "))
    }
}

fn index_search_fts(root: &str, query: &str, max: usize) -> Result<Vec<FileEntry>, String> {
    let Some(fts_query) = fts_query_for(query) else {
        return Ok(Vec::new());
    };
    let conn = open_index_connection()?;
    let root_prefix = format!("{}%", like_escape(root.trim_end_matches(['\\', '/'])));
    let mut stmt = conn
        .prepare(
            "
            SELECT f.path, f.name, f.is_dir, f.size, f.modified, f.extension
            FROM files f
            JOIN files_fts ON files_fts.path = f.path
            WHERE LOWER(f.path) LIKE LOWER(?1) ESCAPE '\\'
              AND files_fts MATCH ?2
            ORDER BY rank, f.is_dir DESC, f.name COLLATE NOCASE ASC
            LIMIT ?3
            ",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![root_prefix, fts_query, sqlite_limit(max)], |row| {
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
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn index_search(root: &str, query: &str, max: usize) -> Result<Vec<FileEntry>, String> {
    let query = query.trim();
    if query.len() < 2 {
        return Ok(Vec::new());
    }

    if let Ok(results) = index_search_fts(root, query, max) {
        if !results.is_empty() {
            return Ok(results);
        }
    }

    let conn = open_index_connection()?;
    let root_prefix = format!("{}%", root.trim_end_matches(['\\', '/']));
    let (name_like, path_like, ext_exact) = if let Some(ext) = query.strip_prefix("ext:") {
        (
            "%".to_string(),
            "%".to_string(),
            ext.trim_start_matches('.').to_lowercase(),
        )
    } else if let Some(name) = query.strip_prefix("name:") {
        (
            format!("%{}%", like_escape(name)),
            "%".to_string(),
            String::new(),
        )
    } else {
        let escaped = format!("%{}%", like_escape(query));
        (escaped.clone(), escaped, query.to_lowercase())
    };

    // COLLATE NOCASE on name/path LIKE clauses; LOWER() on the path prefix
    // filter so drive-wide searches match regardless of index path casing.
    let mut stmt = conn
        .prepare(
            "
            SELECT path, name, is_dir, size, modified, extension
            FROM files
            WHERE LOWER(path) LIKE LOWER(?1) ESCAPE '\\'
              AND (
                name LIKE ?2 ESCAPE '\\' COLLATE NOCASE
                OR path LIKE ?3 ESCAPE '\\' COLLATE NOCASE
                OR extension = ?4
              )
            ORDER BY is_dir DESC, name COLLATE NOCASE ASC
            LIMIT ?5
            ",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map(
            params![
                root_prefix,
                name_like,
                path_like,
                ext_exact,
                sqlite_limit(max)
            ],
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
    let prefix = expand_path_query(prefix).trim().to_string();
    if prefix.len() < 2 {
        return Vec::new();
    }
    let Ok(conn) = open_index_connection() else {
        return Vec::new();
    };
    // Match directories whose path starts with the typed prefix (case-insensitive)
    let pattern = format!("{}%", like_escape(&prefix));
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
    index_roots_for_mode_with_drives(settings, None)
}

fn index_roots_for_mode_with_drives(
    settings: &NativeSettings,
    drives: Option<&[DriveInfo]>,
) -> Vec<String> {
    match settings.index_mode.as_str() {
        "balanced" => common_index_roots(),
        "fast" => {
            if settings.index_roots.is_empty() {
                common_index_roots()
            } else {
                settings.index_roots.clone()
            }
        }
        "max" => {
            let drives = drives.map(|d| d.to_vec()).unwrap_or_else(get_drives);
            drives
                .into_iter()
                .filter(|drive| drive.kind == "local")
                .map(|drive| drive.path)
                .collect()
        }
        _ => Vec::new(),
    }
}

fn estimate_index_storage(roots: &[String], mode: &str) -> String {
    match mode {
        "balanced" => format!(
            "Balanced indexes {} common location{}. Typical storage is 50 MB to 250 MB.",
            roots.len(),
            if roots.len() == 1 { "" } else { "s" }
        ),
        "fast" => format!(
            "Fast lookup indexes {} selected root{}. Typical storage is 150 MB to 600 MB.",
            roots.len(),
            if roots.len() == 1 { "" } else { "s" }
        ),
        "max" => format!(
            "Max indexes {} local drive{}. Storage can reach 1 GB or more on large systems.",
            roots.len(),
            if roots.len() == 1 { "" } else { "s" }
        ),
        _ => "Low uses only folders you open, usually under 50 MB.".to_string(),
    }
}

fn index_status_for_settings_with_drives(
    settings: &NativeSettings,
    drives: Option<&[DriveInfo]>,
) -> IndexStatus {
    let roots = index_roots_for_mode_with_drives(settings, drives);
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

fn index_status_for_settings(settings: &NativeSettings) -> IndexStatus {
    index_status_for_settings_with_drives(settings, None)
}

fn file_size_or_zero(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn stored_data_item(label: &str, file_name: &str, description: &str) -> StoredDataItem {
    let path = native_data_file(file_name);
    StoredDataItem {
        label: label.to_string(),
        path: path.to_string_lossy().to_string(),
        bytes: file_size_or_zero(&path),
        description: description.to_string(),
    }
}

fn privacy_storage_info_for_state(
    state: &AppState,
    settings: &NativeSettings,
) -> PrivacyStorageInfo {
    let index_path = native_index_file();
    let thumb_dir = thumbnail_cache_dir();
    let directory_cache_entries = state
        .directory_cache
        .lock()
        .map(|cache| cache.len())
        .unwrap_or(0);
    let preview_cache_entries = state
        .preview_cache
        .lock()
        .map(|cache| cache.len())
        .unwrap_or(0);
    let watcher_count = state
        .watchers
        .lock()
        .map(|watchers| watchers.len())
        .unwrap_or(0);
    let status = index_status_for_settings(settings);

    PrivacyStorageInfo {
        data_dir: native_data_dir().to_string_lossy().to_string(),
        cache_dir: native_cache_dir().to_string_lossy().to_string(),
        index_path: index_path.to_string_lossy().to_string(),
        thumbnail_cache_dir: thumb_dir.to_string_lossy().to_string(),
        directory_cache_entries,
        preview_cache_entries,
        watcher_count,
        index_bytes: status.index_bytes,
        thumbnail_cache_bytes: status.thumbnail_bytes,
        thumbnail_cache_limit: status.thumbnail_limit,
        update_checks_enabled: settings.update_checks_enabled,
        network_downloads_enabled: settings.network_downloads_enabled,
        network_uploads_enabled: false,
        stored_items: vec![
            stored_data_item("Settings", "settings.json", "Theme, density, indexing, and privacy preferences."),
            stored_data_item("User Pins", "user_pins.json", "Pinned files and folders shown in the sidebar."),
            stored_data_item("Legacy Bookmarks", "bookmarks.json", "Old bookmark data migrated into user pins when present."),
            stored_data_item("Tags", "tags.json", "File path to tag color mappings."),
            stored_data_item("Tag Labels", "tag_labels.json", "Custom tag display names."),
            stored_data_item("Smart Folder Labels", "smart_folder_labels.json", "Custom smart folder display names."),
            stored_data_item("Notes", "notes.json", "Local file notes keyed by path."),
            stored_data_item("Saved Searches", "searches.json", "Named search queries and scopes."),
            stored_data_item("Session", "session.json", "Open tabs, paths, and view preferences."),
            stored_data_item("Recent Locations", "recent_locations.json", "Condensed local navigation history."),
        ],
        policy: "Pathfinder stores local metadata, thumbnails, and an optional SQLite search index only on this PC. It does not upload files. Update checks are off unless enabled, and update downloads require an explicit user action.".to_string(),
    }
}

#[tauri::command]
fn get_privacy_storage_info(state: State<'_, AppState>) -> PrivacyStorageInfo {
    let settings = read_native_json("settings.json", NativeSettings::default());
    privacy_storage_info_for_state(&state, &settings)
}

#[tauri::command]
fn clear_local_caches(state: State<'_, AppState>) -> Result<PrivacyStorageInfo, String> {
    if let Ok(mut cache) = state.directory_cache.lock() {
        cache.clear();
    }
    if let Ok(mut cache) = state.preview_cache.lock() {
        cache.clear();
    }
    if let Ok(mut cache) = state.git_cache.lock() {
        cache.clear();
    }
    let _ = clear_thumbnail_cache()?;
    let settings = read_native_json("settings.json", NativeSettings::default());
    Ok(privacy_storage_info_for_state(&state, &settings))
}

#[tauri::command]
fn clear_search_index() -> Result<u64, String> {
    let path = native_index_file();
    let bytes = file_size_or_zero(&path);
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(bytes)
}

#[tauri::command]
fn set_update_checks_enabled(_enabled: bool) -> Result<(), String> {
    // No-op. Update checks are mandatory and the setting is ignored.
    // Kept as a tauri command for backward compatibility with any older
    // frontend code that might still try to call it.
    Ok(())
}

fn normalize_version(version: &str) -> String {
    version
        .trim()
        .trim_start_matches('v')
        .trim_start_matches('V')
        .to_string()
}

fn version_numbers(version: &str) -> Vec<u64> {
    normalize_version(version)
        .split(['.', '-', '+'])
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    let mut latest_parts = version_numbers(latest);
    let mut current_parts = version_numbers(current);
    let max = latest_parts.len().max(current_parts.len());
    latest_parts.resize(max, 0);
    current_parts.resize(max, 0);
    latest_parts > current_parts
}

#[allow(dead_code)]
fn update_disabled_result() -> UpdateCheckResult {
    let current = env!("CARGO_PKG_VERSION").to_string();
    UpdateCheckResult {
        available: false,
        current_version: current.clone(),
        latest_version: current,
        release_url: GITHUB_RELEASES_URL.to_string(),
        download_url: String::new(),
        notes: String::new(),
        message: "Update checks are off. Pathfinder will not contact GitHub until you enable or manually run update checks.".to_string(),
    }
}

/// Poll until `slint::run_event_loop()` is active so `invoke_from_event_loop` succeeds.
/// The background updater thread is started before `run_event_loop()` blocks; without this,
/// the first GitHub check can run too early and the update pill never appears.
fn wait_until_slint_event_loop_ready(max_wait: Duration) -> bool {
    const STEP: Duration = Duration::from_millis(25);
    let mut waited = Duration::ZERO;
    while waited <= max_wait {
        if slint::invoke_from_event_loop(|| {}).is_ok() {
            return true;
        }
        std::thread::sleep(STEP);
        waited += STEP;
    }
    false
}

fn github_http_user_agent() -> String {
    format!(
        "Pathfinder/{} (+{})",
        env!("CARGO_PKG_VERSION"),
        GITHUB_RELEASES_URL
    )
}

fn powershell_executable() -> String {
    let preferred = "powershell";
    let fallback = "pwsh";
    if ProcessCommand::new(preferred)
        .arg("-NoProfile")
        .arg("-Command")
        .arg("exit 0")
        .output()
        .is_ok()
    {
        preferred.to_string()
    } else {
        fallback.to_string()
    }
}

/// Append a timestamped line to %APPDATA%\Pathfinder\updater.log so users
/// (and we) can answer "is the auto-update check running?" without attaching
/// a debugger or rebuilding with a console subsystem. Best effort - failures
/// are swallowed because the updater must keep running even if disk is full.
fn updater_log(msg: &str) {
    let path = native_data_file("updater.log");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{now}] {msg}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
    eprintln!("[updater] {msg}");
}

/// Prefer a Windows `.exe` NSIS/setup asset, else `.msi`; skip `.zip` / `.7z` so
/// in-app Install always launches a real installer.
fn pick_release_installer_url(assets: &[serde_json::Value]) -> String {
    for ext in [".exe", ".msi"] {
        for a in assets {
            let Some(name) = a.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            if !name.to_ascii_lowercase().ends_with(ext) {
                continue;
            }
            let Some(url) = a
                .get("browser_download_url")
                .and_then(|v| v.as_str())
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            return url.to_string();
        }
    }
    String::new()
}

fn check_github_release_now() -> Result<UpdateCheckResult, String> {
    updater_log(&format!("GET {GITHUB_LATEST_RELEASE_API} (in-process)"));
    let agent = github_http_user_agent();
    let auth_header = std::env::var("PATHFINDER_GITHUB_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|tok| format!("Bearer {tok}"));
    let mut req = ureq::get(GITHUB_LATEST_RELEASE_API)
        .set("User-Agent", &agent)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28");
    if let Some(ref a) = auth_header {
        req = req.set("Authorization", a);
    }
    let resp = req.call().map_err(|e| {
        let msg = format!("GitHub request failed: {e}");
        updater_log(&msg);
        msg
    })?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        let body = resp.into_string().unwrap_or_default();
        let hint = if status == 403 {
            " (GitHub often returns 403 when rate-limited or blocked; set PATHFINDER_GITHUB_TOKEN for higher limits.)"
        } else {
            ""
        };
        let msg = format!(
            "GitHub HTTP {status}{hint}: {}",
            body.chars().take(400).collect::<String>()
        );
        updater_log(&msg);
        return Err(msg);
    }
    let value: serde_json::Value = resp.into_json().map_err(|e| {
        let msg = format!("GitHub JSON decode failed: {e}");
        updater_log(&msg);
        msg
    })?;
    let latest_raw = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("name").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let latest_version = normalize_version(&latest_raw);
    let release_url = value
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or(GITHUB_RELEASES_URL)
        .to_string();
    let notes = value
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(4_000)
        .collect::<String>();
    let download_url = value
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|assets| pick_release_installer_url(assets))
        .unwrap_or_default();
    if download_url.is_empty() {
        eprintln!("[updater] no .exe or .msi release asset found for in-app install");
    }
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let available =
        !latest_version.is_empty() && version_is_newer(&latest_version, &current_version);
    Ok(UpdateCheckResult {
        available,
        current_version: current_version.clone(),
        latest_version: if latest_version.is_empty() {
            current_version.clone()
        } else {
            latest_version.clone()
        },
        release_url,
        download_url,
        notes,
        message: if available {
            format!("Pathfinder {latest_version} is available.")
        } else {
            "Pathfinder is up to date.".to_string()
        },
    })
}

/// `.msi` vs `.exe` from the download URL path (GitHub `browser_download_url`).
fn installer_suffix_from_url(url: &str) -> &'static str {
    let leaf = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("");
    if leaf.to_ascii_lowercase().ends_with(".msi") {
        ".msi"
    } else {
        ".exe"
    }
}

fn download_and_install_update(url: &str) -> Result<(), String> {
    let suffix = installer_suffix_from_url(url);
    let installer = std::env::temp_dir().join(format!("pathfinder_update{suffix}"));
    let _ = fs::remove_file(&installer);
    // Native HTTPS via ureq instead of PowerShell. GitHub release downloads
    // redirect to objects.githubusercontent.com, and ureq follows redirects
    // by default, so we just stream the final body to the installer path.
    let resp = ureq::get(url)
        .set("User-Agent", &github_http_user_agent())
        .set("Accept", "application/octet-stream")
        .timeout(std::time::Duration::from_secs(180))
        .call()
        .map_err(|e| format!("download HTTP error: {e}"))?;
    if !(200..300).contains(&resp.status()) {
        return Err(format!("download HTTP {}", resp.status()));
    }
    let mut reader = resp.into_reader();
    let mut out =
        fs::File::create(&installer).map_err(|e| format!("create installer file: {e}"))?;
    std::io::copy(&mut reader, &mut out).map_err(|e| format!("write installer file: {e}"))?;
    drop(out);
    let meta = fs::metadata(&installer).map_err(|e| format!("Download not found on disk: {e}"))?;
    if meta.len() < 64 * 1024 {
        let _ = fs::remove_file(&installer);
        return Err(
            "Downloaded file is too small to be a valid installer (possible network or GitHub error)."
                .into(),
        );
    }

    #[cfg(windows)]
    {
        if suffix == ".msi" {
            std::process::Command::new("msiexec.exe")
                .arg("/i")
                .arg(&installer)
                .spawn()
                .map_err(|e| format!("Could not start Windows Installer (msiexec): {e}"))?;
        } else {
            std::process::Command::new(&installer)
                .spawn()
                .map_err(|e| format!("Could not start installer: {e}"))?;
        }
    }
    #[cfg(not(windows))]
    {
        let _ = installer;
        return Err("In-app update install is only supported on Windows.".into());
    }
    Ok(())
}

#[tauri::command]
fn check_for_updates() -> Result<UpdateCheckResult, String> {
    // Update check is mandatory and cannot be disabled. The user always sees
    // a pill in the status bar when a newer version exists; they choose
    // whether to click Install. The setting field still exists in the JSON
    // for backward compatibility but is ignored everywhere.
    check_github_release_now()
}

#[tauri::command]
fn check_for_updates_now() -> Result<UpdateCheckResult, String> {
    check_github_release_now()
}

#[tauri::command]
fn open_update_release(release_url: Option<String>) -> Result<(), String> {
    let url = release_url
        .filter(|url| url.starts_with("https://github.com/"))
        .unwrap_or_else(|| GITHUB_RELEASES_URL.to_string());
    open::that(url).map_err(|e| e.to_string())
}

#[tauri::command]
fn apply_update(release_url: Option<String>) -> Result<(), String> {
    // Deliberately opens the signed GitHub Releases page instead of downloading silently.
    open_update_release(release_url)
}

/// Returns false when the system is on battery with less than 20% charge.
/// Background indexing should pause in that case to avoid draining the battery.
fn indexing_permitted() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut s = SYSTEM_POWER_STATUS::default();
        if unsafe { GetSystemPowerStatus(&mut s) }.is_err() {
            return true;
        }
        // BatteryFlag 128 = no battery (desktop), 255 = status unknown
        let no_battery = s.BatteryFlag & 128 != 0 || s.BatteryFlag == 255;
        let plugged_in = s.ACLineStatus == 1;
        let charge_ok = s.BatteryLifePercent == 255 || s.BatteryLifePercent >= 20;
        no_battery || plugged_in || charge_ok
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

fn schedule_index_roots(roots: Vec<String>) {
    if roots.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
            };
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
        // Give the settings UI a beat to repaint before background I/O starts.
        std::thread::sleep(Duration::from_millis(350));
        // Skip background indexing when on low battery to avoid draining it.
        if !indexing_permitted() {
            return;
        }
        for root in roots {
            // Re-check permission periodically between roots.
            if !indexing_permitted() {
                break;
            }
            let root_path = PathBuf::from(&root);
            if !root_path.is_dir() {
                continue;
            }
            let mut by_parent: HashMap<String, Vec<FileEntry>> = HashMap::new();
            let mut processed = 0usize;
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
                processed += 1;

                if by_parent.len() > 128 {
                    let batch = std::mem::take(&mut by_parent);
                    for (parent, entries) in batch {
                        let _ = index_directory_entries(&parent, &entries);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                } else if processed.is_multiple_of(1000) {
                    std::thread::sleep(Duration::from_millis(3));
                }
            }
            for (parent, entries) in by_parent {
                let _ = index_directory_entries(&parent, &entries);
                std::thread::sleep(Duration::from_millis(5));
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

/// Background JSON writer queue. Lets navigate(), tag-edit, and other hot
/// paths return immediately instead of blocking on disk I/O for files like
/// recent_locations.json that are rewritten on every folder change.
///
/// Implementation: a HashMap keyed by file name holds the latest serialised
/// bytes for each file. A single background thread drains the map every
/// 250 ms and writes the bytes to disk. Repeated writes to the same file in
/// the same window coalesce - only the most recent payload hits disk.
static JSON_WRITE_QUEUE: LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static JSON_WRITER_THREAD: LazyLock<std::thread::JoinHandle<()>> = LazyLock::new(|| {
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let drained: Vec<(String, Vec<u8>)> = match JSON_WRITE_QUEUE.lock() {
                Ok(mut q) => q.drain().collect(),
                Err(_) => continue,
            };
            for (name, bytes) in drained {
                let path = native_data_file(&name);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&path, &bytes);
            }
        }
    })
});

/// Fire-and-forget version of write_native_json. Serialises now (fast),
/// hands off to the background writer (slow disk I/O). Callers that need
/// the bytes flushed immediately (rare - most settings/tag writes are best
/// effort) should still use write_native_json directly.
fn write_native_json_async<T: Serialize>(name: &str, value: &T) {
    // Ensure the drainer thread is alive on first call. LazyLock initialises
    // on first access; the side effect of the closure is the spawned thread.
    let _ = JSON_WRITER_THREAD.thread().id();
    let Ok(bytes) = serde_json::to_vec_pretty(value) else {
        return;
    };
    if let Ok(mut q) = JSON_WRITE_QUEUE.lock() {
        q.insert(name.to_string(), bytes);
    }
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

fn smart_folder_labels() -> HashMap<String, String> {
    read_native_json("smart_folder_labels.json", HashMap::new())
}

fn smart_folders_for_path(current_path: &str) -> Vec<SmartFolder> {
    let labels = smart_folder_labels();
    default_smart_folders(current_path)
        .into_iter()
        .map(|mut folder| {
            if let Some(label) = labels.get(&folder.id) {
                folder.name = label.clone();
            }
            folder
        })
        .collect()
}

#[tauri::command]
fn get_smart_folders(path: String) -> Vec<SmartFolder> {
    smart_folders_for_path(&path)
}

#[tauri::command]
fn rename_smart_folder(id: String, name: String) -> Result<Vec<SmartFolder>, String> {
    let mut labels = smart_folder_labels();
    let name = name.trim();
    if name.is_empty() {
        labels.remove(&id);
    } else {
        labels.insert(id, name.to_string());
    }
    write_native_json("smart_folder_labels.json", &labels)?;
    Ok(smart_folders_for_path(""))
}

#[tauri::command]
fn get_tag_labels() -> HashMap<String, String> {
    read_native_json("tag_labels.json", HashMap::new())
}

#[tauri::command]
fn rename_tag_label(id: String, name: String) -> Result<HashMap<String, String>, String> {
    let mut labels = get_tag_labels();
    let name = name.trim();
    if name.is_empty() {
        labels.remove(&id);
    } else {
        labels.insert(id, name.to_string());
    }
    write_native_json("tag_labels.json", &labels)?;
    Ok(labels)
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

fn load_file_templates() -> Vec<FileTemplate> {
    let saved: Vec<FileTemplate> = read_native_json("file_templates.json", Vec::new());
    if saved.is_empty() {
        let defaults = default_file_templates();
        let _ = write_native_json("file_templates.json", &defaults);
        defaults
    } else {
        saved
    }
}

fn load_automation_rules() -> Vec<AutomationRule> {
    let saved: Vec<AutomationRule> = read_native_json("automation_rules.json", Vec::new());
    if saved.is_empty() {
        let defaults = default_automation_rules();
        let _ = write_native_json("automation_rules.json", &defaults);
        defaults
    } else {
        saved
    }
}

fn save_automation_rules(rules: &[AutomationRule]) -> Result<(), String> {
    write_native_json("automation_rules.json", &rules)
}

fn load_shortcut_overrides() -> HashMap<String, String> {
    read_native_json("shortcuts.json", HashMap::new())
}

fn save_shortcut_overrides(map: &HashMap<String, String>) -> Result<(), String> {
    write_native_json("shortcuts.json", map)
}

fn shortcut_hint_for(command: &str, default_hint: &str) -> String {
    load_shortcut_overrides()
        .get(command)
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default_hint.to_string())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct WorkspaceSession {
    name: String,
    tabs: Vec<SessionTab>,
}

fn load_workspaces() -> Vec<WorkspaceSession> {
    read_native_json("workspaces.json", Vec::new())
}

fn save_workspaces(list: &[WorkspaceSession]) -> Result<(), String> {
    write_native_json("workspaces.json", &list)
}

fn native_save_search(name: String, query: String, scope: String) -> Result<(), String> {
    let mut searches: Vec<SavedSearch> = read_native_json("searches.json", Vec::new());
    searches.retain(|s| s.name != name);
    searches.insert(
        0,
        SavedSearch {
            name,
            query,
            scope,
        },
    );
    if searches.len() > 50 {
        searches.truncate(50);
    }
    write_native_json("searches.json", &searches)
}

fn run_automation_rules_once(
    rules: &[AutomationRule],
    tags: &mut HashMap<String, String>,
) -> Result<(usize, usize), String> {
    let mut tagged = 0usize;
    let mut moved = 0usize;
    for rule in rules.iter().filter(|r| r.enabled) {
        if rule.folder.is_empty() || !Path::new(&rule.folder).is_dir() {
            continue;
        }
        let ext = rule.extension.trim_start_matches('.').to_ascii_lowercase();
        let entries = fs::read_dir(&rule.folder).map_err(|e| e.to_string())?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let file_ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if !ext.is_empty() && file_ext != ext {
                continue;
            }
            let path_str = path.to_string_lossy().to_string();
            if !rule.tag.is_empty() {
                tags.insert(path_str.clone(), rule.tag.clone());
                tagged += 1;
            }
            if let Some(dest_dir) = rule.move_to.as_ref().filter(|d| !d.is_empty()) {
                let dest = PathBuf::from(dest_dir);
                if !dest.is_dir() {
                    let _ = fs::create_dir_all(&dest);
                }
                if dest.is_dir() {
                    let dest_path = dest.join(path.file_name().unwrap_or_default());
                    let dest_final = if dest_path.exists() {
                        keep_both_destination(&dest_path)
                    } else {
                        dest_path
                    };
                    if fs::rename(&path, &dest_final).is_ok() {
                        moved += 1;
                        if let Some(tag) = tags.remove(&path_str) {
                            tags.insert(dest_final.to_string_lossy().to_string(), tag);
                        }
                    }
                }
            }
        }
    }
    Ok((tagged, moved))
}

fn collect_image_duplicate_groups(folder: &str) -> Vec<(String, Vec<String>)> {
    let Ok(conn) = open_index_connection() else {
        return Vec::new();
    };
    let prefix = format!("{}%", folder.trim_end_matches(['\\', '/']));
    let sql = "SELECT path, dhash FROM image_dhash WHERE path LIKE ?1 ESCAPE '\\'";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![prefix], |row| {
        let path: String = row.get(0)?;
        let dh: Vec<u8> = row.get(1)?;
        Ok((path, dh))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut entries: Vec<(String, u64)> = Vec::new();
    for r in rows {
        let Ok((path, blob)) = r else { continue };
        if blob.len() != 8 {
            continue;
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&blob);
        entries.push((path, u64::from_le_bytes(arr)));
    }
    let mut groups = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    for i in 0..entries.len() {
        if seen.contains(&i) {
            continue;
        }
        let mut group = vec![i];
        for j in (i + 1)..entries.len() {
            if seen.contains(&j) {
                continue;
            }
            if crate::inference::hamming64(entries[i].1, entries[j].1) == 0 {
                group.push(j);
            }
        }
        if group.len() > 1 {
            for &idx in &group {
                seen.insert(idx);
            }
            let paths: Vec<String> = group.iter().map(|&idx| entries[idx].0.clone()).collect();
            let title = Path::new(&paths[0])
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| paths[0].clone());
            groups.push((title, paths));
        }
    }
    groups
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
    if path == "recycle://" {
        return vec![ChoiceItem {
            id: ss("recycle://"),
            label: ss("Recycle Bin"),
            description: ss(""),
            color: slint::Color::from_argb_u8(0, 0, 0, 0),
        }];
    }
    if path == "home://" {
        return vec![ChoiceItem {
            id: ss("home://"),
            label: ss("Home"),
            description: ss(""),
            color: slint::Color::from_argb_u8(0, 0, 0, 0),
        }];
    }
    if path == "storage://" {
        return vec![ChoiceItem {
            id: ss("storage://"),
            label: ss("Storage"),
            description: ss(""),
            color: slint::Color::from_argb_u8(0, 0, 0, 0),
        }];
    }
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

fn drive_root_for_path(path: &str) -> String {
    let path = Path::new(path);
    #[cfg(target_os = "windows")]
    {
        if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
            return format!("{}\\", prefix.as_os_str().to_string_lossy());
        }
    }
    path.ancestors()
        .last()
        .filter(|root| !root.as_os_str().is_empty())
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn is_filesystem_root(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        let mut components = path.components();
        matches!(
            (components.next(), components.next(), components.next()),
            (
                Some(std::path::Component::Prefix(_)),
                Some(std::path::Component::RootDir),
                None
            )
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        path.parent().is_none() || path == Path::new("/")
    }
}

fn compact_drive_label(path: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        if let Some(std::path::Component::Prefix(prefix)) = Path::new(path).components().next() {
            let text = prefix.as_os_str().to_string_lossy();
            return text.trim_end_matches('\\').to_string();
        }
    }
    "All".to_string()
}

fn user_facing_error(message: String) -> String {
    let lower = message.to_lowercase();
    if lower.contains("access is denied")
        || lower.contains("access denied")
        || lower.contains("permission denied")
        || lower.contains("requires elevation")
    {
        "Access denied. Windows blocked this item. Try Show More Options, Run as Administrator, or Take Ownership for protected paths.".to_string()
    } else {
        message
    }
}

fn native_bookmarks() -> Vec<Bookmark> {
    native_user_pins()
        .into_iter()
        .map(|pin| Bookmark {
            name: pin.name,
            path: pin.path,
        })
        .collect()
}

fn bookmark_to_pin(bookmark: Bookmark) -> UserPin {
    let kind = if Path::new(&bookmark.path).is_dir() {
        "folder"
    } else {
        "file"
    };
    UserPin {
        name: bookmark.name,
        path: bookmark.path,
        kind: kind.to_string(),
        pinned_at: now_unix_secs(),
    }
}

fn default_user_pins() -> Vec<UserPin> {
    get_known_folders()
        .into_iter()
        .filter(|folder| matches!(folder.id.as_str(), "documents" | "downloads" | "desktop"))
        .map(|folder| Bookmark {
            name: folder.name,
            path: folder.path,
        })
        .map(bookmark_to_pin)
        .collect()
}

fn load_user_pins_raw() -> Vec<UserPin> {
    let saved: Vec<UserPin> = read_native_json("user_pins.json", Vec::new());
    if !saved.is_empty() {
        return saved;
    }

    let legacy: Vec<Bookmark> = read_native_json("bookmarks.json", Vec::new());
    if !legacy.is_empty() {
        let pins = legacy.into_iter().map(bookmark_to_pin).collect::<Vec<_>>();
        let _ = write_native_json("user_pins.json", &pins);
        return pins;
    }

    default_user_pins()
}

fn native_user_pins() -> Vec<UserPin> {
    load_user_pins_raw()
        .into_iter()
        .filter(|pin| Path::new(&pin.path).exists())
        .collect()
}

fn validate_user_pins(pins: Vec<UserPin>) -> Vec<UserPin> {
    pins.into_iter()
        .filter(|pin| Path::new(&pin.path).exists())
        .collect()
}

fn save_native_user_pins(pins: &[UserPin]) -> Result<(), String> {
    write_native_json("user_pins.json", &pins)
}

fn pin_name_for_path(path: &Path, explicit_name: Option<String>) -> String {
    explicit_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn condense_recent_locations(paths: Vec<String>, max_items: usize) -> Vec<String> {
    condense_recent_locations_inner(paths, max_items, true)
}

fn condense_recent_locations_fast(paths: Vec<String>, max_items: usize) -> Vec<String> {
    condense_recent_locations_inner(paths, max_items, false)
}

fn condense_recent_locations_inner(
    paths: Vec<String>,
    max_items: usize,
    require_exists: bool,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut condensed = Vec::new();
    for path in paths {
        if path.trim().is_empty() {
            continue;
        }
        if require_exists && !Path::new(&path).exists() {
            continue;
        }
        let key = cache_key(Path::new(&path));
        if seen.insert(key) {
            condensed.push(path);
        }
        if condensed.len() >= max_items {
            break;
        }
    }
    condensed
}

fn validate_recent_locations(paths: Vec<String>, max_items: usize) -> Vec<String> {
    condense_recent_locations(paths, max_items)
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

fn native_list_directory_page(state: &AppState, path: &str) -> Result<DirectoryPage, String> {
    if let Some(entries) = state.cached_directory(path) {
        return Ok(DirectoryPage {
            entries,
            partial: false,
            skipped_entries: 0,
        });
    }
    list_directory_chunk(Path::new(path), FIRST_DIRECTORY_CHUNK)
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

/// Number of bytes the preview pane reads. Larger than the old 4 KB so text and
/// HTML/Markdown source show meaningful content; `read_preview_uncached` still
/// caps text at 64 KB internally, so this never reads an entire large file.
const PREVIEW_READ_BYTES: usize = 64 * 1024;

/// True if a preview for `path` is already sitting in the in-memory cache, so
/// the caller can render it synchronously without a background thread hop.
/// Only does a cheap stat (needed for the cache key) — never reads the body.
fn preview_is_cached(state: &AppState, path: &str) -> bool {
    let path_buf = PathBuf::from(path);
    let Ok(metadata) = fs::metadata(&path_buf) else {
        return false;
    };
    let key = format!(
        "{}|{}|{}",
        cache_key(&path_buf),
        unix_secs(metadata.modified()),
        PREVIEW_READ_BYTES
    );
    state.preview(&key).is_some()
}

/// Read a file preview and format the pane's body, meta block, and (for
/// HTML/Markdown) a rendered plain-text version. This is the slow part of a
/// selection change, so it runs off the UI thread.
fn build_preview_display(
    state: &AppState,
    path: &str,
    ext: &str,
    type_label: &str,
    size: u64,
    modified: u64,
) -> (String, String, String) {
    match native_read_preview(state, path, Some(PREVIEW_READ_BYTES)) {
        Ok(preview) => {
            let body = match preview.kind.as_str() {
                "image" | "folder" => String::new(),
                "text" | "svg" | "archive" | "pdf" | "font" | "media"
                | "image-too-large" | "image-metadata" => preview.text.unwrap_or_default(),
                other => format!("{other} file"),
            };
            let truncated_note = if preview.truncated { " | truncated" } else { "" };
            let meta = format!(
                "Path:     {}\nType:     {}\nSize:     {}\nModified: {}{}",
                path,
                type_label,
                format_size_short(size),
                format_modified(modified),
                truncated_note,
            );
            let rendered = match ext {
                "html" | "htm" => render_html_to_text(&body),
                "md" | "markdown" => render_markdown_to_text(&body),
                _ => String::new(),
            };
            (body, meta, rendered)
        }
        Err(error) => (String::from("Preview unavailable"), error, String::new()),
    }
}

/// Decode the handful of HTML entities common in real documents. Kept small and
/// allocation-light; the preview only needs readable text, not a full parse.
fn decode_html_entities(input: &str) -> String {
    let mut out = input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "\u{2014}")
        .replace("&ndash;", "\u{2013}")
        .replace("&hellip;", "\u{2026}")
        .replace("&bull;", "\u{2022}")
        .replace("&copy;", "\u{00a9}")
        .replace("&reg;", "\u{00ae}")
        .replace("&trade;", "\u{2122}");
    // Numeric entities like &#8217; — resolve the decimal ones we can.
    if out.contains("&#") {
        let re = &NUMERIC_ENTITY_RE;
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                caps.get(1)
                    .and_then(|m| m.as_str().parse::<u32>().ok())
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            })
            .into_owned();
    }
    out
}

/// Collapse runs of blank lines to at most one and trim trailing whitespace so
/// a stripped document reads cleanly in the narrow preview column.
fn tidy_rendered_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0u32;
    for line in input.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Convert an HTML document into readable plain text: script/style content is
/// dropped, block tags become line breaks, list items get bullets, and entities
/// are decoded. This is the built-in "rendered" view — there is no WebView in
/// the app by design, so this gives a fast, dependency-free approximation.
fn render_html_to_text(html: &str) -> String {
    let chars: Vec<char> = html.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    let mut in_skip = false; // inside <script> / <style>
    while i < n {
        let c = chars[i];
        if c == '<' {
            let mut j = i + 1;
            let closing = j < n && chars[j] == '/';
            if closing {
                j += 1;
            }
            let mut name = String::new();
            while j < n && (chars[j].is_ascii_alphanumeric()) {
                name.push(chars[j].to_ascii_lowercase());
                j += 1;
            }
            let mut k = i + 1;
            while k < n && chars[k] != '>' {
                k += 1;
            }
            let tag_end = if k < n { k + 1 } else { n };
            match name.as_str() {
                "script" | "style" => in_skip = !closing,
                "br" => out.push('\n'),
                "li" => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    if !closing {
                        out.push_str("\u{2022} ");
                    }
                }
                "td" | "th" if !closing => {
                    out.push('\t');
                }
                "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "main"
                | "ul" | "ol" | "table" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                | "blockquote" | "pre" | "hr" | "figure" | "figcaption"
                    if !out.ends_with('\n') =>
                {
                    out.push('\n');
                }
                _ => {}
            }
            i = tag_end;
            continue;
        }
        if !in_skip {
            out.push(c);
        }
        i += 1;
    }
    tidy_rendered_text(&decode_html_entities(&out))
}

/// Lightly render Markdown to plain text: strip heading markers and emphasis,
/// turn `-`/`*`/`+` bullets into real bullets, and rewrite `[text](url)` links
/// as `text (url)`.
fn render_markdown_to_text(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for raw in md.lines() {
        let line = raw.trim_end();
        // Fenced code fences: drop the ``` marker lines entirely.
        if line.trim_start().starts_with("```") {
            continue;
        }
        let mut l = line.to_string();
        // Headings: strip a leading run of '#'.
        let hashes = l.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && hashes <= 6 && l.chars().nth(hashes) == Some(' ') {
            l = l[hashes + 1..].trim_start().to_string();
        }
        // Bullets, preserving indentation.
        let lt = l.trim_start();
        if let Some(rest) = lt
            .strip_prefix("- ")
            .or_else(|| lt.strip_prefix("* "))
            .or_else(|| lt.strip_prefix("+ "))
        {
            let indent_len = l.len() - lt.len();
            l = format!("{}\u{2022} {}", &l[..indent_len], rest);
        }
        out.push_str(&l);
        out.push('\n');
    }
    // Inline cleanup: links first, then emphasis / code markers.
    let linked = MD_LINK_RE.replace_all(&out, "$1 ($2)").into_owned();
    let cleaned = linked
        .replace("**", "")
        .replace("__", "")
        .replace('`', "");
    tidy_rendered_text(&cleaned)
}

static NUMERIC_ENTITY_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"&#(\d+);").unwrap());
static MD_LINK_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[([^\]]+)\]\(([^)\s]+)\)").unwrap());

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
        .no_window()
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
    native_delete_inner(state, path, true)
}

/// Recycle-bin delete without walking the tree for queue byte totals — keeps
/// the UI thread responsive when the user confirms deleting huge folders.
fn native_delete_fast(state: &AppState, path: &str) -> Result<(), String> {
    native_delete_inner(state, path, false)
}

fn native_delete_inner(
    state: &AppState,
    path: &str,
    measure_folder_bytes: bool,
) -> Result<(), String> {
    if state.queue_cancel_requested() {
        return Err(OP_CANCELLED.to_string());
    }
    if state.queue_is_paused() {
        return Err("Operation queue is paused.".to_string());
    }
    let path_buf = PathBuf::from(path);
    if !path_buf.exists() {
        return Err(format!("Path does not exist: {path}"));
    }
    let total = if measure_folder_bytes {
        folder_size_quick(&path_buf, 25_000)
    } else if path_buf.is_file() {
        fs::metadata(&path_buf).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let op_id = state.queue_start("delete", path, None, total);
    let started = Instant::now();
    trash::delete(&path_buf).map_err(|e| e.to_string())?;
    let trash_id = trash::os_limited::list().ok().and_then(|items| {
        items.into_iter().find(|item| {
            same_path_string(
                &item.original_path().to_string_lossy(),
                &path_buf.to_string_lossy(),
            )
        })
    });
    let trash_id_str = trash_id.map(|item| item.id.to_string_lossy().into_owned());
    state.invalidate_path(&path_buf);
    state.log_op_with_trash("delete", path, None, trash_id_str.as_deref());
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

fn native_create_file(state: &AppState, path: &str) -> Result<(), String> {
    if state.queue_is_paused() {
        return Err("Operation queue is paused.".to_string());
    }
    let path_buf = PathBuf::from(path);
    if path_buf.exists() {
        return Err(format!("File already exists: {}", path_buf.display()));
    }
    if let Some(parent) = path_buf.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    File::create(&path_buf).map_err(|e| e.to_string())?;
    state.invalidate_path(&path_buf);
    if let Some(parent) = path_buf.parent() {
        state.invalidate_path(parent);
    }
    Ok(())
}

fn native_copy(state: &AppState, from: &str, to: &str) -> Result<(), String> {
    if state.queue_cancel_requested() {
        return Err(OP_CANCELLED.to_string());
    }
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
        copy_dir_recursive(state, &src, &dst)
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
    if state.queue_cancel_requested() {
        return Err(OP_CANCELLED.to_string());
    }
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
            copy_dir_recursive(state, &src, &dst)?;
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

/// Return (free_bytes, total_bytes) for the volume that contains `path`.
/// Returns None on non-Windows builds or if the OS call fails.
fn drive_space_cache_key(path: &str) -> String {
    let p = Path::new(path);
    let mut components = p.components();
    #[cfg(target_os = "windows")]
    {
        use std::path::Component;
        if let Some(Component::Prefix(prefix)) = components.next()
            && matches!(components.next(), Some(Component::RootDir))
        {
            return format!("{}\\", prefix.as_os_str().to_string_lossy());
        }
    }
    path.to_string()
}

#[cfg(target_os = "windows")]
fn drive_free_space(path: &str) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::PCWSTR;

    if path.is_empty() {
        return None;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut free_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free_total: u64 = 0;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_caller),
            Some(&mut total),
            Some(&mut free_total),
        )
        .ok()?;
    }
    Some((free_caller, total))
}

#[cfg(not(target_os = "windows"))]
fn drive_free_space(_path: &str) -> Option<(u64, u64)> {
    None
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

/// Bucket a modified timestamp into a coarse Explorer-style label. Used by the
/// details view to draw section headers above each new bucket (Today, Yesterday,
/// This week, Last week, Earlier this month, Earlier this year, Older). The
/// boundaries use 24-hour windows from now rather than wall-clock midnight,
/// which is simpler and still matches the user's mental model close enough.
fn date_group_label(secs: u64) -> &'static str {
    if secs == 0 {
        return "Unknown date";
    }
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(secs);
    // If somehow modified is in the future (clock skew / unsynced file), still
    // bucket it as Today so it doesn't end up at the very bottom under "Older".
    if secs > now {
        return "Today";
    }
    const DAY: u64 = 86_400;
    if diff < DAY {
        "Today"
    } else if diff < 2 * DAY {
        "Yesterday"
    } else if diff < 7 * DAY {
        "This week"
    } else if diff < 14 * DAY {
        "Last week"
    } else if diff < 30 * DAY {
        "Earlier this month"
    } else if diff < 365 * DAY {
        "Earlier this year"
    } else {
        "Older"
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
    } else if diff < 172_800 {
        "1 day ago".to_string()
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

fn tag_label(id: &str) -> &'static str {
    match id {
        "red" => "Urgent",
        "orange" => "Important",
        "yellow" => "Review",
        "green" => "Done",
        "blue" => "Personal",
        "violet" => "Code",
        _ => "Tag",
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
        "black" => (
            color("#0b0d10"),
            rgba_u8(11, 13, 16, 0.22),
            color("#1c2128"),
        ),
        "white" => (
            color("#e8ecf2"),
            rgba_u8(232, 236, 242, 0.18),
            color("#ffffff"),
        ),
        "copper" => (
            color("#c46f34"),
            rgba_u8(196, 111, 52, 0.18),
            color("#e89255"),
        ),
        "gold" => (
            color("#d4a83a"),
            rgba_u8(212, 168, 58, 0.18),
            color("#f0c75a"),
        ),
        "indigo" => (
            color("#3b4cb8"),
            rgba_u8(59, 76, 184, 0.18),
            color("#5e72d6"),
        ),
        "crimson" => (
            color("#c0312f"),
            rgba_u8(192, 49, 47, 0.18),
            color("#e25754"),
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
            bg: color("#e4ecf5"),
            bg_soft: color("#eef4fb"),
            panel: rgba_u8(252, 254, 255, 0.82),
            panel_solid: color("#fafcfe"),
            panel_alt: color("#dfe8f4"),
            titlebar: rgba_u8(236, 244, 252, 0.94),
            sidebar: rgba_u8(226, 236, 248, 0.88),
            border: rgba_u8(56, 90, 132, 0.14),
            border_strong: rgba_u8(56, 90, 132, 0.26),
            text: color("#16202f"),
            text_muted: color("#4d5f73"),
            text_faint: color("#758497"),
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
            // Deeper latte - more saturated honey tones, richer shadows
            bg: color("#e8d8c0"),
            bg_soft: color("#f4ead8"),
            panel: rgba_u8(254, 244, 226, 0.92),
            panel_solid: color("#fef0d8"),
            panel_alt: color("#d8c4a0"),
            titlebar: rgba_u8(230, 215, 192, 0.96),
            sidebar: rgba_u8(220, 205, 180, 0.94),
            border: rgba_u8(110, 78, 36, 0.18),
            border_strong: rgba_u8(110, 78, 36, 0.35),
            text: color("#241c10"),
            text_muted: color("#5c4830"),
            text_faint: color("#8a7260"),
            accent: color("#c86010"),
            accent_soft: rgba_u8(200, 96, 16, 0.18),
            accent_strong: color("#a04c08"),
            radius: 8.0,
            radius_small: 5.0,
            ui_font: "Georgia",
            mono_font: "Consolas",
            light_controls: true,
            outer_border: 0.0,
        },
        "flat" => PaletteSpec {
            bg: color("#eef0f3"),
            bg_soft: color("#fafbfc"),
            panel: color("#fdfdfd"),
            panel_solid: color("#ffffff"),
            panel_alt: color("#e6eaef"),
            titlebar: color("#f9fafb"),
            sidebar: color("#eceff3"),
            border: color("#d4dce6"),
            border_strong: color("#b9c6d4"),
            text: color("#161a20"),
            text_muted: color("#526071"),
            text_faint: color("#7d8795"),
            accent: color("#4f6fdc"),
            accent_soft: rgba_u8(79, 111, 220, 0.15),
            accent_strong: color("#3d5ac8"),
            radius: 4.0,
            radius_small: 3.0,
            ui_font: "Calibri",
            mono_font: "Consolas",
            light_controls: true,
            outer_border: 0.0,
        },
        "terminal" => PaletteSpec {
            bg: color("#040a08"),
            bg_soft: color("#081510"),
            panel: color("#060e0b"),
            panel_solid: color("#060e0b"),
            panel_alt: color("#0a1e14"),
            titlebar: color("#020705"),
            sidebar: color("#040b08"),
            border: rgba_u8(57, 255, 140, 0.32),
            border_strong: rgba_u8(120, 255, 180, 0.52),
            text: color("#cbffe0"),
            text_muted: color("#6bdc97"),
            text_faint: color("#3f8f5f"),
            accent: color("#7cff9d"),
            accent_soft: rgba_u8(124, 255, 157, 0.12),
            accent_strong: color("#c8ffd8"),
            radius: 0.0,
            radius_small: 0.0,
            ui_font: "Cascadia Mono",
            mono_font: "Cascadia Mono",
            light_controls: false,
            outer_border: 2.0,
        },
        "paper" => PaletteSpec {
            bg: color("#e3d6bc"),
            bg_soft: color("#f2e6d0"),
            panel: color("#efe3cc"),
            panel_solid: color("#efe3cc"),
            panel_alt: color("#d4c29e"),
            titlebar: color("#dbc9a6"),
            sidebar: color("#d6c6a0"),
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
            ui_font: "Times New Roman",
            mono_font: "Courier New",
            light_controls: true,
            outer_border: 0.0,
        },
        "retro" => PaletteSpec {
            bg: color("#1c1850"),
            bg_soft: color("#2a2380"),
            panel: color("#281f70"),
            panel_solid: color("#281f70"),
            panel_alt: color("#3d32a0"),
            titlebar: color("#100d3a"),
            sidebar: color("#181456"),
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
            // Press Start 2P (bundled, OFL) - true 8-bit arcade pixel font.
            ui_font: "Press Start 2P",
            mono_font: "Press Start 2P",
            light_controls: false,
            outer_border: 4.0,
        },
        "cyberpunk" => PaletteSpec {
            bg: color("#080318"),
            bg_soft: color("#12062e"),
            panel: rgba_u8(38, 8, 72, 0.90),
            panel_solid: color("#260848"),
            panel_alt: color("#35106c"),
            titlebar: color("#040210"),
            sidebar: color("#0c0624"),
            border: rgba_u8(0, 255, 242, 0.30),
            border_strong: rgba_u8(255, 40, 200, 0.58),
            text: color("#f6f2ff"),
            text_muted: color("#9cecff"),
            text_faint: color("#ce6cff"),
            accent: color("#ff39bc"),
            accent_soft: rgba_u8(255, 57, 188, 0.18),
            accent_strong: color("#00ecff"),
            radius: 3.0,
            radius_small: 2.0,
            // Bahnschrift Condensed = narrow futuristic sans; Consolas for code.
            ui_font: "Bahnschrift Condensed",
            mono_font: "Consolas",
            light_controls: false,
            outer_border: 0.0,
        },
        "fantasy" => PaletteSpec {
            // Enchanted forest at dusk - deep moss greens with antique gold trim
            // and burnished bronze highlights. Clearly distinct from sunset warm,
            // mica blues, and retro neon.
            bg: color("#0d1a14"),
            bg_soft: color("#142a20"),
            panel: rgba_u8(24, 52, 38, 0.92),
            panel_solid: color("#1a3a2a"),
            panel_alt: color("#234a36"),
            titlebar: rgba_u8(7, 14, 11, 0.96),
            sidebar: rgba_u8(15, 28, 22, 0.95),
            border: rgba_u8(196, 158, 78, 0.28),
            border_strong: rgba_u8(232, 196, 120, 0.46),
            text: color("#f3e7c8"),
            text_muted: color("#c9b785"),
            text_faint: color("#8a7c54"),
            accent: color("#e8c478"),
            accent_soft: rgba_u8(232, 196, 120, 0.18),
            accent_strong: color("#f5dca0"),
            radius: 10.0,
            radius_small: 6.0,
            ui_font: "Cambria",
            mono_font: "Consolas",
            light_controls: false,
            outer_border: 0.0,
        },
        "sunset" => PaletteSpec {
            // Dusk sky: deep aubergine bg, warm amber-to-rose gradient feel
            bg: color("#160a1a"),
            bg_soft: color("#22102a"),
            panel: rgba_u8(48, 18, 52, 0.92),
            panel_solid: color("#301232"),
            panel_alt: color("#451c4c"),
            titlebar: rgba_u8(10, 5, 12, 0.98),
            sidebar: rgba_u8(26, 10, 30, 0.96),
            border: rgba_u8(255, 120, 70, 0.20),
            border_strong: rgba_u8(255, 160, 100, 0.36),
            text: color("#ffe8d8"),
            text_muted: color("#d4907a"),
            text_faint: color("#8a5040"),
            accent: color("#ff7043"),
            accent_soft: rgba_u8(255, 112, 67, 0.16),
            accent_strong: color("#ff9a7a"),
            radius: 10.0,
            radius_small: 6.0,
            // Candara has the warm humanist feel without the missing-glyph
            // problem that Segoe Script ran into on some machines (the
            // script font has limited Unicode coverage, so labels and
            // settings copy could render blank).
            ui_font: "Candara",
            mono_font: "Cascadia Mono",
            light_controls: false,
            outer_border: 0.0,
        },
        // mica-dark and the catch-all default both use the standard dark palette.
        _ => PaletteSpec {
            bg: color("#0c0f13"),
            bg_soft: color("#141920"),
            panel: rgba_u8(34, 40, 50, 0.86),
            panel_solid: color("#232a34"),
            panel_alt: color("#2b3440"),
            titlebar: rgba_u8(10, 12, 15, 0.94),
            sidebar: rgba_u8(26, 30, 37, 0.88),
            border: rgba_u8(120, 150, 190, 0.14),
            border_strong: rgba_u8(170, 195, 230, 0.24),
            text: color("#f0f4fa"),
            text_muted: color("#aab6c9"),
            text_faint: color("#758296"),
            accent: color("#4f9cff"),
            accent_soft: rgba_u8(79, 156, 255, 0.16),
            accent_strong: color("#82bfff"),
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
        "warm" | "terminal" | "paper" | "retro" | "fantasy" | "cyberpunk" | "sunset"
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
    let (accent, accent_soft, accent_strong) = if settings.accent == "custom" {
        let hex = settings
            .custom_accent_hex
            .as_deref()
            .filter(|h| h.len() == 7 && h.starts_with('#'))
            .unwrap_or("#4f9cff");
        let base = color(hex);
        let soft = rgba_u8(base.red(), base.green(), base.blue(), 0.16);
        let strong = lighten_color(base, 0.20);
        (base, soft, strong)
    } else {
        accent_override(&settings.accent)
    };
    palette.accent = accent;
    palette.accent_soft = accent_soft;
    palette.accent_strong = accent_strong;

    let global = ui.global::<ThemePalette>();
    global.set_bg_gradient_enabled(false);
    global.set_bg_gradient_accent_tip(false);
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

    let (folder_1, folder_2) = if let Some(hex) = settings
        .folder_color
        .as_deref()
        .filter(|h| h.len() == 7 && h.starts_with('#'))
    {
        let base = color(hex);
        (lighten_color(base, 0.28), base)
    } else {
        icon_folder_colors(&settings.theme)
    };
    global.set_icon_folder_1(folder_1);
    global.set_icon_folder_2(folder_2);
    global.set_active_theme(ss(&settings.theme));
    global.set_folder_icon_image(load_theme_folder_icon(&settings.theme));

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
    let folder_hex = settings.folder_color.clone().unwrap_or_else(|| {
        let (_, folder_2) = icon_folder_colors(&settings.theme);
        format!(
            "#{:02x}{:02x}{:02x}",
            folder_2.red(),
            folder_2.green(),
            folder_2.blue()
        )
    });
    ui.set_folder_color_hex(ss(&folder_hex));

    let accent_hex = settings
        .custom_accent_hex
        .clone()
        .unwrap_or_else(|| "#4f9cff".to_string());
    ui.set_custom_accent_hex(ss(&accent_hex));
}

fn set_choice_chip_strides(metrics: &AppMetrics, row_h: f32) {
    // ChoiceChip is 46px tall; keep row spacing at least 52px so chips never overlap.
    metrics.set_choice_chip_row_stride((row_h + 6.0).max(52.0));
    metrics.set_index_chip_row_stride((row_h + 16.0).max(52.0));
}

fn apply_density_metrics(metrics: &AppMetrics, density: &str) {
    let row_h = match density {
        "compact" => {
            metrics.set_row_h(26.0);
            metrics.set_grid_w(104.0);
            metrics.set_grid_h(94.0);
            metrics.set_pad(8.0);
            26.0_f32
        }
        "comfortable" => {
            metrics.set_row_h(32.0);
            metrics.set_grid_w(120.0);
            metrics.set_grid_h(108.0);
            metrics.set_pad(12.0);
            32.0_f32
        }
        _ => {
            metrics.set_row_h(38.0);
            metrics.set_grid_w(136.0);
            metrics.set_grid_h(122.0);
            metrics.set_pad(16.0);
            38.0_f32
        }
    };
    set_choice_chip_strides(metrics, row_h);
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
    metrics.set_ui_font(ss(bundled_ui_family_from_preset(
        normalize_ui_font_preset(def.ui_font.as_str()).as_str(),
    )));
    metrics.set_mono_font(ss(bundled_mono_family_from_preset(
        normalize_mono_font_preset(def.mono_font.as_str()).as_str(),
    )));
    metrics.set_light_controls(is_light);

    let base_row_h = 38.0_f32;
    let size_delta = def.font_size_delta as f32 * 4.0;
    let row_h = base_row_h + size_delta;
    metrics.set_row_h(row_h);
    metrics.set_grid_w(136.0 + size_delta * 3.0);
    metrics.set_grid_h(122.0 + size_delta * 3.0);
    set_choice_chip_strides(&metrics, row_h);

    global.set_active_theme(ss("custom"));
    global.set_bg_gradient_enabled(def.gradient_background);
    global.set_bg_gradient_accent_tip(def.gradient_background && def.gradient_accent_tip);
    global.set_folder_icon_image(slint::Image::default());
}

fn sync_editor_state(ui: &MainWindow, def: &ThemeDefinition) {
    let mut d = def.clone();
    normalize_theme_font_presets(&mut d);
    ui.set_ce_name(ss(&d.name));
    ui.set_ce_finish(ss(&d.finish));
    ui.set_ce_radius(d.radius);
    ui.set_ce_anim_speed(d.anim_speed);
    ui.set_ce_ui_font(ss(&d.ui_font));
    ui.set_ce_mono_font(ss(&d.mono_font));
    ui.set_ce_preview_ui_font(ss(bundled_ui_family_from_preset(d.ui_font.as_str())));
    ui.set_ce_preview_mono_font(ss(bundled_mono_family_from_preset(d.mono_font.as_str())));
    ui.set_ce_font_size_delta(d.font_size_delta);
    ui.set_ce_icon_folder_hex(ss(&d.icon_folder_hex));
    ui.set_ce_gradient_background(d.gradient_background);
    ui.set_ce_gradient_accent(d.gradient_accent_tip);
    ui.set_ce_icon_set(ss(""));
    ui.set_ce_selected_token(-1);
    ui.set_ce_token_hex(ss(""));
    ui.set_ce_token_label(ss(""));

    let tokens = [
        &d.bg,
        &d.bg_soft,
        &d.panel,
        &d.border,
        &d.border_strong,
        &d.text,
        &d.text_muted,
        &d.text_faint,
        &d.accent,
        &d.danger,
        &d.success,
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
        gradient_background: ui.get_ce_gradient_background(),
        gradient_accent_tip: ui.get_ce_gradient_accent(),
    }
}

#[cfg(target_os = "windows")]
fn apply_window_finish(ui: &MainWindow, finish: &str) {
    use i_slint_backend_winit::WinitWindowAccessor;
    use i_slint_backend_winit::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};

    const DWMWA_SYSTEMBACKDROP_TYPE_ID: i32 = 38;
    const DWMSBT_MAINWINDOW: i32 = 2;
    const DWMSBT_NONE: i32 = 1;

    let backdrop = match finish {
        "mica-dark" => DWMSBT_MAINWINDOW,
        "mica-light" => DWMSBT_MAINWINDOW,
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
    ("Files", "New File", "", "new-file"),
    ("Files", "Rename", "F2", "rename"),
    ("Files", "Delete", "Del", "delete"),
    ("Files", "Copy", "Ctrl+C", "copy"),
    ("Files", "Cut", "Ctrl+X", "cut"),
    ("Files", "Paste", "Ctrl+V", "paste"),
    ("Files", "Select All", "Ctrl+A", "select-all"),
    ("Files", "Batch Rename", "", "batch-rename"),
    ("Tools", "Checksum", "", "checksum"),
    ("Tools", "File Note", "", "note"),
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
    ("Tools", "Open in Terminal", "", "open-terminal"),
    ("Tools", "Open With", "", "open-with"),
    ("Files", "Restore from Recycle Bin", "", "restore"),
    ("Files", "Permanently Delete", "Shift+Del", "purge"),
    ("Files", "Empty Recycle Bin", "", "empty-trash"),
    ("Tools", "Scan with Microsoft Defender", "", "defender-scan"),
    ("Tools", "Shell Verb Bridge", "", "shell-verbs"),
    ("Tools", "Cloud State", "", "cloud-state"),
    ("Tools", "Run as Administrator", "", "run-as-admin"),
    ("Tools", "Take Ownership", "", "take-ownership"),
    ("Tools", "Previous Versions", "", "previous-versions"),
    ("Tools", "Pin to Taskbar", "", "pin-to-taskbar"),
    ("Tools", "Pin to Start", "", "pin-to-start"),
    ("Tools", "New From Template", "", "new-template"),
    ("Tools", "Power Rename Presets", "", "rename-presets"),
    ("Tools", "Image Tools", "", "image-tools"),
    ("Tools", "Archive Browser", "", "archive-browser"),
    ("Tools", "Extract Here", "", "extract-here"),
    ("Tools", "Create ZIP Archive", "", "create-zip"),
    ("Tools", "Create 7z Archive", "", "create-7z"),
    ("Tools", "Create tar.gz Archive", "", "create-tar-gz"),
    ("Tools", "Compare Folder", "", "compare-folder"),
    ("Tools", "Rules and Automation", "", "rules"),
    ("Tools", "Smart Folders", "", "smart-folders"),
    ("Tools", "Home Page", "", "home-page"),
    ("Tools", "Save Workspace", "", "save-workspace"),
    ("Tools", "Open Workspace", "", "open-workspace"),
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
    ("Tools", "Clear Local Caches", "", "clear-local-caches"),
    ("Tools", "Rebuild Search Index", "", "rebuild-index"),
    (
        "Tools",
        "AI: Suggested tags for selection",
        "MobileNet label -> tag on selected images",
        "ai-suggest-tags",
    ),
    (
        "Tools",
        "Find duplicate images",
        "Same-folder dHash exact matches in the index",
        "find-image-duplicates",
    ),
    (
        "Settings",
        "Search Performance Settings",
        "",
        "performance-settings",
    ),
    ("Settings", "Privacy and Storage", "", "privacy-storage"),
    ("Settings", "Check for Updates", "", "check-updates"),
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

fn command_match_score(group: &str, label: &str, command: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let label_l = label.to_lowercase();
    let group_l = group.to_lowercase();
    let command_l = command.to_lowercase();
    if label_l == query {
        Some(0)
    } else if label_l.starts_with(query) {
        Some(10)
    } else if label_l.contains(query) {
        Some(30)
    } else if command_l.starts_with(query) {
        Some(40)
    } else if command_l.contains(query) {
        Some(55)
    } else if group_l.contains(query) {
        Some(70)
    } else if fuzzy_match(&label_l, query) {
        Some(90 + (label_l.len() as i32 - query.len() as i32).max(0))
    } else if fuzzy_match(&command_l, query) {
        Some(120)
    } else {
        None
    }
}

fn command_items_filtered(query: &str) -> ModelRc<CommandItem> {
    command_items_filtered_with_recent(query, &[])
}

fn command_items_filtered_with_recent(
    query: &str,
    recent: &[String],
) -> ModelRc<CommandItem> {
    let q = query.to_lowercase();
    let overrides = load_shortcut_overrides();
    let mut matches: Vec<(i32, &(&str, &str, &str, &str))> = ALL_COMMANDS
        .iter()
        .filter_map(|item @ (group, label, _, command)| {
            command_match_score(group, label, command, &q).map(|score| (score, item))
        })
        .collect();
    matches.sort_by(|(score_a, a), (score_b, b)| {
        score_a
            .cmp(score_b)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    let mut items: Vec<CommandItem> = Vec::new();
    if q.is_empty() && !recent.is_empty() {
        for cmd in recent.iter().take(5) {
            if let Some((_group, label, hint, command)) =
                ALL_COMMANDS.iter().find(|(_, _, _, c)| *c == cmd.as_str())
            {
                let hint_s = overrides
                    .get(*command)
                    .cloned()
                    .unwrap_or_else(|| (*hint).to_string());
                items.push(CommandItem {
                    group: ss("Recent"),
                    label: ss(*label),
                    hint: ss(hint_s),
                    command: ss(*command),
                });
            }
        }
    }
    items.extend(matches.into_iter().map(|(_, (group, label, hint, command))| {
        let hint_s = overrides
            .get(*command)
            .cloned()
            .unwrap_or_else(|| (*hint).to_string());
        CommandItem {
            group: ss(*group),
            label: ss(*label),
            hint: ss(hint_s),
            command: ss(*command),
        }
    }));
    model_from_vec(items)
}

fn command_items() -> ModelRc<CommandItem> {
    command_items_filtered("")
}

/// Resolves the folder Pathfinder should land on at startup from process args.
/// Accepts every form Windows / other apps pass when they want to "open a
/// folder", so the user doesn't see broken openings from third-party callers:
///
///   - `--path <dir>` / `--path=<dir>` - what the HKCU shell handler we
///     register passes (`"%1"`).
///   - `/select,<file>` or `/select <file>` - the Explorer convention used by
///     "Show in folder" / "Open file location" in many apps (Chrome, Steam,
///     Discord, Slack, Notepad, etc). We open the file's parent directory.
///   - A bare path argument - when an app invokes us as `pathfinder.exe
///     C:\Users\Foo` without any flag. Treats files like /select (opens the
///     parent).
///
/// The first match wins. Returns None if nothing on the command line resolves
/// to an existing filesystem path.
fn parse_cli_startup_folder() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--path" {
            if let Some(next) = args.get(i + 1) {
                return Some(PathBuf::from(next));
            }
        } else if let Some(rest) = a.strip_prefix("--path=") {
            if !rest.is_empty() {
                return Some(PathBuf::from(rest));
            }
        } else if let Some(rest) = a.strip_prefix("/select,") {
            // Explorer-style /select,<file> - open parent and (ideally) select.
            if !rest.is_empty() {
                let f = std::path::PathBuf::from(rest);
                if let Some(parent) = f.parent() {
                    if parent.exists() {
                        return Some(parent.to_path_buf());
                    }
                }
            }
        } else if a == "/select" {
            if let Some(next) = args.get(i + 1) {
                let f = std::path::PathBuf::from(next);
                if let Some(parent) = f.parent() {
                    if parent.exists() {
                        return Some(parent.to_path_buf());
                    }
                }
            }
        } else if !a.starts_with('-') && !a.starts_with('/') {
            // Bare path argument. Some launchers pass just the path without a
            // flag; the registry handler still sees `pathfinder.exe "path"`.
            let p = std::path::PathBuf::from(a);
            if p.exists() {
                return Some(p);
            }
        }
        i += 1;
    }
    None
}

fn resolve_cli_folder_to_string(raw: PathBuf) -> Option<String> {
    let trimmed = raw.to_string_lossy().trim_matches('"').to_string();
    let p = PathBuf::from(trimmed);
    if !p.exists() {
        eprintln!(
            "[pathfinder] Ignoring --path (does not exist): {}",
            p.display()
        );
        return None;
    }
    let folder = if p.is_file() {
        p.parent()?.to_path_buf()
    } else if p.is_dir() {
        p
    } else {
        return None;
    };
    let canon = folder.canonicalize().unwrap_or(folder);
    canon.to_str().map(|s| s.to_string())
}

/// Folders that should always open in detail (list) view regardless
/// of any saved per-folder preference. Documents, Downloads, and any
/// drive root are navigation hubs where users want columns visible.
/// Path is expected lowercased.
fn is_always_list_view_folder(lower: &str) -> bool {
    // Drive root: matches "c:", "c:\", "d:\", "x:", "\\server\share",
    // "/" — short paths with no real folder component below the root.
    let trimmed = lower.trim_end_matches('\\').trim_end_matches('/');
    let is_drive_root = trimmed.len() <= 2 && trimmed.ends_with(':');
    if is_drive_root || trimmed.is_empty() || trimmed == "/" {
        return true;
    }
    // UNC root like \\server\share with no further path.
    if let Some(after) = trimmed.strip_prefix("\\\\") {
        let slashes = after.matches('\\').count();
        if slashes <= 1 {
            return true;
        }
    }
    // Documents / Downloads (any drive, any depth-1 location).
    let segments: Vec<&str> = trimmed
        .split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .collect();
    let Some(last) = segments.last() else {
        return false;
    };
    matches!(*last, "documents" | "downloads")
}

/// App-internal navigation targets (not filesystem paths). These often contain
/// `/` and must never go through `resolve_path_query`.
fn is_virtual_nav_path(path: &str) -> bool {
    path == "recycle://"
        || path == "storage://"
        || path.starts_with(ARCHIVE_SCHEME)
}

fn navigation_display_path(current_path: &str, archive: Option<&ArchiveView>) -> String {
    if let Some(archive) = archive {
        return archive_display_path(&archive.archive_path, &archive.prefix);
    }
    match current_path {
        "recycle://" => "Recycle Bin".to_string(),
        "storage://" => "Storage".to_string(),
        _ => current_path.to_string(),
    }
}

/// True when the search box input is meant as a filesystem path (not a
/// filename/content query). Examples: `%localappdata%\Foo`, `C:\Games`,
/// `Documents\Save`, `~/Downloads`.
fn looks_like_path_query(query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return false;
    }
    if is_virtual_nav_path(q) {
        return false;
    }
    if q.contains('%') || q.contains('\\') || q.contains('/') {
        return true;
    }
    if q.starts_with('~') {
        return true;
    }
    let bytes = q.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':'
}

fn env_var_value_ci(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(name) {
        return Some(v);
    }
    let upper = name.to_ascii_uppercase();
    std::env::var(&upper).ok()
}

/// Expand `%VAR%`, `~`, and normalize slashes for Windows path input.
pub(crate) fn expand_path_query(raw: &str) -> String {
    let mut out = raw.trim().to_string();
    if out.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            let rest = out.trim_start_matches('~').trim_start_matches(['\\', '/']);
            out = if rest.is_empty() {
                home.to_string_lossy().into_owned()
            } else {
                home.join(rest).to_string_lossy().into_owned()
            };
        }
    }
    #[cfg(target_os = "windows")]
    {
        out = out.replace('/', "\\");
    }
    #[cfg(not(target_os = "windows"))]
    {
        out = out.replace('\\', "/");
    }
    while let Some(start) = out.find('%') {
        let Some(rel_end) = out[start + 1..].find('%') else {
            break;
        };
        let end = start + 1 + rel_end;
        let var_name = &out[start + 1..end];
        let Some(value) = env_var_value_ci(var_name) else {
            break;
        };
        out.replace_range(start..=end, &value);
    }
    out
}

/// Resolve a search-box path against `cwd` when the query is relative.
fn resolve_path_query(query: &str, cwd: &str) -> PathBuf {
    let expanded = expand_path_query(query);
    let path = PathBuf::from(&expanded);
    if path.is_absolute() {
        path
    } else {
        PathBuf::from(cwd).join(path)
    }
}

/// Deepest directory along `path` that exists on disk (walks upward from leaf).
fn deepest_existing_directory(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.as_os_str().is_empty() {
            break;
        }
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

fn index_search_paths(root: &str, pattern: &str, max: usize) -> Result<Vec<FileEntry>, String> {
    let pattern = expand_path_query(pattern);
    let pattern = pattern.trim_end_matches(['\\', '/']);
    if pattern.len() < 2 {
        return Ok(Vec::new());
    }
    let conn = open_index_connection()?;
    let root_prefix = format!("{}%", root.trim_end_matches(['\\', '/']));
    let path_like = format!("%{}%", like_escape(pattern));
    let mut stmt = conn
        .prepare(
            "
            SELECT path, name, is_dir, size, modified, extension
            FROM files
            WHERE LOWER(path) LIKE LOWER(?1) ESCAPE '\\'
              AND LOWER(path) LIKE LOWER(?2) ESCAPE '\\'
            ORDER BY is_dir DESC, length(path) ASC, name COLLATE NOCASE ASC
            LIMIT ?3
            ",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            params![root_prefix, path_like, sqlite_limit(max)],
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

/// Attempt to coerce a bogus user-supplied path into a navigable
/// folder. The common case this guards against: "Open With" or shell
/// shortcuts that hand us a comma-joined multi-file string like
/// `C:\Downloads\a.txt,b.txt,c.txt...`. Strategy: take the first
/// comma-separated chunk, then walk up parents until something exists
/// and is a directory. Returns the recovered path string, or None if
/// nothing reasonable can be salvaged.
fn recover_navigable_path(raw: &str) -> Option<String> {
    let first = raw.split(',').next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }
    let p = Path::new(first);
    if p.is_dir() {
        return Some(first.to_string());
    }
    let mut cur = p.parent();
    while let Some(parent) = cur {
        if parent.as_os_str().is_empty() {
            break;
        }
        if parent.is_dir() {
            return Some(parent.to_string_lossy().into_owned());
        }
        cur = parent.parent();
    }
    None
}

/// True when `a` and `b` refer to the same on-disk object (used to skip no-op drops).
fn same_inode_or_canonical_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.exists(), b.exists()) {
        (true, true) => match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(ca), Ok(cb)) => ca == cb,
            _ => false,
        },
        _ => false,
    }
}

impl NativeController {
    fn new(cli_folder: Option<PathBuf>, settings: NativeSettings) -> Self {
        let app_state = AppState::default();
        let known_folders = get_known_folders();
        let home = get_home_directory()
            .ok()
            .or_else(|| known_folders.first().map(|folder| folder.path.clone()))
            .unwrap_or_else(|| ".".to_string());
        // Read all small JSON state files in parallel via rayon. Each file is
        // a few KB on disk but the sequential I/O latency adds up over 7 reads.
        // join lets the OS file cache stream them concurrently and cuts the
        // total startup wall-clock by roughly the number of cores serving I/O.
        let ((tabs_raw, recent_raw), (folder_views_raw, (tags_raw, (tag_labels_raw, (notes_raw, user_pins_raw))))): (
            (Vec<SessionTab>, Vec<String>),
            (HashMap<String, String>, (HashMap<String, String>, (HashMap<String, String>, (HashMap<String, String>, Vec<UserPin>)))),
        ) = rayon::join(
            || {
                rayon::join(
                    || read_native_json::<Vec<SessionTab>>("session.json", Vec::new()),
                    || read_native_json::<Vec<String>>("recent_locations.json", Vec::new()),
                )
            },
            || {
                rayon::join(
                    || read_native_json::<HashMap<String, String>>("folder_views.json", HashMap::new()),
                    || {
                        rayon::join(
                            || read_native_json::<HashMap<String, String>>("tags.json", HashMap::new()),
                            || {
                                rayon::join(
                                    || read_native_json::<HashMap<String, String>>("tag_labels.json", HashMap::new()),
                                    || {
                                        rayon::join(
                                            || read_native_json::<HashMap<String, String>>("notes.json", HashMap::new()),
                                            || read_native_json::<Vec<UserPin>>("user_pins.json", Vec::new()),
                                        )
                                    },
                                )
                            },
                        )
                    },
                )
            },
        );
        let mut tabs: Vec<SessionTab> = tabs_raw;
        if tabs.is_empty() {
            tabs.push(SessionTab {
                path: String::new(),
                view: "grid".to_string(),
                sort_by: "modified".to_string(),
                sort_dir: "desc".to_string(),
            });
        }
        let cli_resolved = cli_folder.and_then(resolve_cli_folder_to_string);
        let session_first = tabs[0].path.clone();
        // Trust the saved session path; `navigate` validates asynchronously so
        // a slow/network `is_dir()` probe does not block the first paint.
        let from_session = (!session_first.is_empty()).then_some(session_first);
        let current_path = cli_resolved
            .clone()
            .or(from_session)
            .unwrap_or_else(|| home.clone());
        tabs[0].path = current_path.clone();

        Self {
            app_state,
            current_path: current_path.clone(),
            files: Vec::new(),
            visible_files: Vec::new(),
            active_archive: None,
            selected_index: -1,
            selected_set: std::collections::HashSet::new(),
            select_anchor: -1,
            files_model: None,
            search_query: String::new(),
            search_all_scope: false,
            history: vec![current_path.clone()],
            history_index: 0,
            path_scroll: HashMap::new(),
            storage_caches: HashMap::new(),
            storage_scan_pending: Arc::new(Mutex::new(None)),
            storage_scan_ready: Arc::new(AtomicBool::new(false)),
            storage_scan_generation: Arc::new(AtomicU64::new(0)),
            storage_scan_active: false,
            storage_show_all_state: false,
            storage_path_before: String::new(),
            storage_progress: Arc::new(StorageScanProgress::default()),
            storage_current_root: String::new(),
            storage_selected_bucket: String::new(),
            storage_drill_search: String::new(),
            storage_sort_mode: "size".to_string(),
            storage_preview_visible_before: false,
            storage_preview_w_before: 326.0,
            storage_subtitle_last_update: Instant::now(),
            nav_generation: Arc::new(AtomicU64::new(0)),
            sidebar_nav_pending: None,
            sidebar_nav_timer: None,
            storage_disk_used: 0,
            drive_space_cache: HashMap::new(),
            tabs,
            active_tab: 0,
            known_folders,
            drives: Vec::new(),
            user_pins: if user_pins_raw.is_empty() {
                load_user_pins_raw()
            } else {
                user_pins_raw
            },
            recent_locations: condense_recent_locations_fast(recent_raw, 12),
            folder_views: folder_views_raw,
            show_hidden: false,
            ai_progress: {
                let p = Arc::new(local_ai::InstallProgress::new());
                let m = local_ai::read_manifest();
                if let Ok(mut state) = p.state.lock() {
                    *state = m.state;
                }
                p
            },
            #[cfg(target_os = "windows")]
            system_icon_by_ext: HashMap::new(),
            #[cfg(target_os = "windows")]
            system_icon_by_path: HashMap::new(),
            tags: tags_raw,
            tag_labels: tag_labels_raw,
            notes: notes_raw,
            secondary_path: current_path.clone(),
            secondary_history: vec![current_path.clone()],
            secondary_history_pos: 0,
            secondary_sort_by: "modified".to_string(),
            secondary_sort_dir: "desc".to_string(),
            secondary_files: Vec::new(),
            secondary_visible_files: Vec::new(),
            secondary_selected_index: -1,
            secondary_selected_set: std::collections::HashSet::new(),
            secondary_select_anchor: -1,
            secondary_files_model: None,
            active_pane: ActivePane::Primary,
            folder_filter: String::new(),
            git_status: Arc::new(HashMap::new()),
            git_dir_status: HashMap::new(),
            settings,
            ai: AiCapabilities {
                npu_available: false,
                semantic_search: true,
                automatic_summaries: true,
                image_classification: true,
                local_embeddings: true,
                device_name: "CPU Fallback".to_string(),
                acceleration_kind: "CPU".to_string(),
                runtime_configured: false,
                reason: "Detecting...".to_string(),
                gpu_summary: "Detecting GPUs...".to_string(),
            },
            clipboard: None,
            pending_prompt: None,
            // Default sort: most recently modified first. Matches what most
            // people actually want when they open a folder - see the newest
            // download / latest screenshot / freshly built artifact.
            sort_by: "modified".to_string(),
            sort_dir: "desc".to_string(),
            thumbnail_memory: HashMap::new(),
            thumbnail_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thumbnail_timer: None,
            toast_queue: std::collections::VecDeque::new(),
            toast_showing: false,
            toast_current_kind: "info".to_string(),
            toast_current_message: String::new(),
            toast_last_shown: None,
            toast_timer: None,
            git_status_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_git_status: Arc::new(Mutex::new(None)),
            operation_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_operation_result: Arc::new(Mutex::new(None)),
            directory_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_directory_result: Arc::new(Mutex::new(None)),
            search_ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_search_result: Arc::new(Mutex::new(None)),
            pending_deferred_startup: Arc::new(Mutex::new(None)),
            enrich_visible_pending: false,
            compare_hide_same: true,
            compare_left: String::new(),
            compare_right: String::new(),
            compare_all_rows: Vec::new(),
            dupe_groups_cache: Vec::new(),
            shortcut_draft: HashMap::new(),
            recent_commands: std::collections::VecDeque::new(),
            preview_generation: Arc::new(AtomicU64::new(0)),
            preview_ready: Arc::new(AtomicBool::new(false)),
            pending_preview_result: Arc::new(Mutex::new(None)),
        }
    }

    fn initialize_ui(&mut self, ui: &MainWindow) {
        // Surface the compiled-in package version so the Settings header can
        // display it. Single source of truth: Cargo.toml -> CARGO_PKG_VERSION.
        ui.set_app_version(SharedString::from(env!("CARGO_PKG_VERSION")));
        ui.set_theme_choices(choice_items(&[
            (
                "mica-dark",
                "Mica Dark",
                "Cool graphite Fluent - frosted glass panels",
                "#0c0f13",
            ),
            (
                "mica-light",
                "Mica Light",
                "Icy daylight - blue-tint chrome and white glass",
                "#e4ecf5",
            ),
            (
                "warm",
                "Warm Neutral",
                "Latte and oak - sepia UI for long sessions",
                "#d07920",
            ),
            (
                "flat",
                "Flat White",
                "Swiss studio - flat panels, sharp dividers",
                "#eef0f3",
            ),
            (
                "terminal",
                "Terminal",
                "CRT green - phosphor glow, scanline grid, mono type",
                "#7cff9d",
            ),
            (
                "paper",
                "Paper",
                "Ink and cotton - editorial serif warmth",
                "#e3d6bc",
            ),
            (
                "retro",
                "Retro Arcade",
                "Neon cab - purple void, gold marquee",
                "#ffcf3f",
            ),
            (
                "cyberpunk",
                "Cyberpunk",
                "Synth district - magenta rail, cyan haze",
                "#ff39bc",
            ),
            (
                "fantasy",
                "High Fantasy",
                "Moonlit archive - ink glass, aurora teal, arcane violet",
                "#5ee0c8",
            ),
            (
                "sunset",
                "Sunset",
                "Dusk sky - aubergine dark, amber-rose glow",
                "#ff7043",
            ),
        ]));
        ui.set_accent_choices(choice_items(&[
            ("blue", "Blue", "", "#4f9cff"),
            ("amber", "Amber", "", "#d98a24"),
            ("green", "Green", "", "#2aa96b"),
            ("violet", "Violet", "", "#8b6cff"),
            ("rose", "Rose", "", "#e45578"),
            ("teal", "Teal", "", "#1aa6a6"),
            ("copper", "Copper", "", "#c46f34"),
            ("gold", "Gold", "", "#d4a83a"),
            ("indigo", "Indigo", "", "#3b4cb8"),
            ("crimson", "Crimson", "", "#c0312f"),
            ("black", "Black", "", "#0b0d10"),
            ("white", "White", "", "#e8ecf2"),
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
        ui.set_ai_install_size_mb(0);
        ui.set_index_status(ss("Loading index statistics..."));
        ui.set_performance_footprint(ss("Measuring disk usage..."));
        ui.set_performance_intro(ss(concat!(
            "Pathfinder keeps a small local database of the files in folders you visit so the search bar can return matches instantly without rescanning your disk every time. ",
            "Indexing modes change how aggressively that database is grown in the background:\n",
            "\u{2022} Low - only the folders you actually open get added. Lightest on disk and CPU.\n",
            "\u{2022} Balanced - also indexes Desktop, Documents, Downloads, Pictures, and common project folders.\n",
            "\u{2022} Fast - indexes roots you choose, with common folders as fallback.\n",
            "\u{2022} Max - all fixed drives. Best search coverage, uses the most disk while it catches up.\n",
            "Thumbnails are stored separately and the cache is automatically pruned when it hits the budget below."
        )));
        apply_theme(ui, &self.settings);
        ui.set_ai_device(ss(&self.ai.reason));
        ui.set_ai_gpu_status(ss(&self.ai.gpu_summary));
        ui.set_ai_label(ss(ai_status_label(&self.ai)));

        if self.settings.ui_mode.is_empty() {
            ui.set_ui_mode_prompt_visible(true);
        } else {
            ui.set_ui_mode(ss(&self.settings.ui_mode));
        }
        ui.set_side_items_simple(model_from_vec(self.side_items_simple()));

        #[cfg(target_os = "windows")]
        ui.set_show_windows_integration(true);
        #[cfg(not(target_os = "windows"))]
        ui.set_show_windows_integration(false);
        let ai_install_state = self
            .ai_progress
            .state
            .lock()
            .map(|s| *s)
            .unwrap_or(local_ai::InstallState::NotInstalled);
        ui.set_ai_install_state(SharedString::from(ai_install_state.as_slint_str()));
        let semantic_ready = local_ai_semantic_ready_cached();
        let image_ready = local_ai_image_search_ready_cached();
        if !semantic_ready {
            self.settings.search_semantic_mode = false;
        }
        if !image_ready {
            self.settings.clip_search_enabled = false;
        }
        ui.set_semantic_search_available(semantic_ready);
        ui.set_search_semantic_mode(self.settings.search_semantic_mode);
        ui.set_clip_search_enabled(self.settings.clip_search_enabled);
    }

    /// Work deferred until after the first frame is painted so startup feels
    /// instant. Heavy disk/SQLite stats and theme-editor setup run here or on
    /// background threads spawned from here.
    fn finish_startup(&mut self, ui: &MainWindow) {
        let path = self.current_path.clone();
        self.navigate(ui, path, false);
        self.spawn_deferred_startup_data();
        let custom_theme = self.settings.custom_theme.clone();
        let weak_editor = ui.as_weak();
        let editor_timer = slint::Timer::default();
        editor_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(120),
            move || {
                if let Some(ui) = weak_editor.upgrade() {
                    let init_def = if let Some(name) = &custom_theme {
                        load_custom_theme_def(name).unwrap_or_default()
                    } else {
                        ThemeDefinition::default()
                    };
                    sync_editor_state(&ui, &init_def);
                    let saved = list_custom_themes();
                    ui.set_ce_saved_themes(model_from_vec(
                        saved.into_iter().map(SharedString::from).collect(),
                    ));
                }
            },
        );
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let mb = local_ai::approx_total_install_mb() as i32;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_ai_install_size_mb(mb);
                }
            });
        });
    }

    /// Load sidebar drives and the storage scan cache off the critical startup
    /// path so the window can paint before WSL/network drive enumeration finishes.
    fn spawn_deferred_startup_data(&self) {
        let pending = self.pending_deferred_startup.clone();
        let pins_to_validate = self.user_pins.clone();
        let recents_to_validate = self.recent_locations.clone();
        std::thread::spawn(move || {
            let data = DeferredStartupData {
                drives: get_drives(),
                storage_caches: read_storage_scan_cache(),
                user_pins: validate_user_pins(pins_to_validate),
                recent_locations: validate_recent_locations(recents_to_validate, 12),
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(data);
            }
        });
    }

    fn apply_deferred_startup_if_ready(&mut self, ui: &MainWindow) {
        let data = match self.pending_deferred_startup.lock() {
            Ok(mut lock) => lock.take(),
            Err(_) => None,
        };
        let Some(data) = data else {
            return;
        };
        self.drives = data.drives;
        self.storage_caches = data.storage_caches;
        self.user_pins = data.user_pins;
        self.recent_locations = data.recent_locations;
        self.sync_sidebar_models(ui);
        self.spawn_performance_status(ui);
    }

    fn spawn_performance_status(&self, ui: &MainWindow) {
        let settings = self.settings.clone();
        let drives = self.drives.clone();
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let status = index_status_for_settings_with_drives(&settings, Some(&drives));
            let ram_line = match process_memory_stats() {
                Some((ws, private_mb)) => {
                    format!("Memory in use right now: {ws} MB working set ({private_mb} MB private)")
                }
                None => "Memory in use: not available on this platform".to_string(),
            };
            let footprint = format!(
                "{ram_line}\nIndex database: {} on disk\nThumbnail cache: {} of {} budget\n\nLocations:\n  {}\n  {}",
                format_size_short(status.index_bytes),
                format_size_short(status.thumbnail_bytes),
                format_size_short(status.thumbnail_limit),
                native_index_file().display(),
                thumbnail_cache_dir().display(),
            );
            let index_line = format!(
                "{} files indexed | {} on disk | thumbnails {} of {} cap | {}",
                status.indexed_files,
                format_size_short(status.index_bytes),
                format_size_short(status.thumbnail_bytes),
                format_size_short(status.thumbnail_limit),
                status.estimated_storage
            );
            let thumb_line = format!(
                "Thumbnail cache is capped at {} and old thumbnails are removed automatically.",
                format_size_short(status.thumbnail_limit)
            );
            let mode = settings.index_mode.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_active_index_mode(ss(&mode));
                    ui.set_index_status(ss(index_line));
                    ui.set_thumbnail_status(ss(thumb_line));
                    ui.set_performance_footprint(ss(footprint));
                }
            });
        });
    }

    fn show_toast(&mut self, ui: &MainWindow, message: impl Into<String>) {
        self.show_toast_kind(ui, message, "info");
    }

    fn show_toast_kind(&mut self, ui: &MainWindow, message: impl Into<String>, kind: &str) {
        let message = user_facing_error(message.into());
        self.toast_queue.push_back((message, kind.to_string()));
        if !self.toast_showing {
            self.advance_toast_display(ui);
        }
    }

    fn toast_display_duration(kind: &str, message: &str) -> Duration {
        let base_ms: u64 = match kind {
            "error" => 20_000,
            "warning" => 14_000,
            "success" => 5_000,
            _ => 4_500,
        };
        let extra = (message.len() as u64).saturating_mul(40).min(30_000);
        Duration::from_millis(base_ms + extra)
    }

    fn dismiss_toast(&mut self, ui: &MainWindow) {
        ui.set_toast_text(ss(""));
        self.toast_showing = false;
        self.toast_last_shown = None;
        self.toast_current_message.clear();
        self.advance_toast_display(ui);
    }

    fn advance_toast_display(&mut self, ui: &MainWindow) {
        if let Some((msg, kind)) = self.toast_queue.pop_front() {
            ui.set_toast_text(ss(&msg));
            ui.set_toast_kind(ss(&kind));
            self.toast_current_kind = kind;
            self.toast_current_message = msg;
            self.toast_showing = true;
            self.toast_last_shown = Some(std::time::Instant::now());
        } else {
            ui.set_toast_text(ss(""));
            self.toast_showing = false;
            self.toast_last_shown = None;
            self.toast_current_kind = "info".to_string();
            self.toast_current_message.clear();
        }
    }

    fn save_settings(&self) {
        write_native_json_async("settings.json", &self.settings);
    }

    fn sync_performance_status(&self, ui: &MainWindow) {
        let status = index_status_for_settings(&self.settings);
        ui.set_active_index_mode(ss(&self.settings.index_mode));
        ui.set_index_status(ss(format!(
            "{} files indexed | {} on disk | thumbnails {} of {} cap | {}",
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

        let intro = concat!(
            "Pathfinder keeps a small local database of the files in folders you visit so the search bar can return matches instantly without rescanning your disk every time. ",
            "Indexing modes change how aggressively that database is grown in the background:\n",
            "\u{2022} Low - only the folders you actually open get added. Lightest on disk and CPU.\n",
            "\u{2022} Balanced - also indexes Desktop, Documents, Downloads, Pictures, and common project folders.\n",
            "\u{2022} Fast - indexes roots you choose, with common folders as fallback.\n",
            "\u{2022} Max - all fixed drives. Best search coverage, uses the most disk while it catches up.\n",
            "Thumbnails are stored separately and the cache is automatically pruned when it hits the budget below."
        );
        ui.set_performance_intro(ss(intro));

        let ram_line = match process_memory_stats() {
            Some((ws, private_mb)) => {
                format!("Memory in use right now: {ws} MB working set ({private_mb} MB private)")
            }
            None => "Memory in use: not available on this platform".to_string(),
        };
        let footprint = format!(
            "{ram_line}\nIndex database: {} on disk\nThumbnail cache: {} of {} budget\n\nLocations:\n  {}\n  {}",
            format_size_short(status.index_bytes),
            format_size_short(status.thumbnail_bytes),
            format_size_short(status.thumbnail_limit),
            native_index_file().display(),
            thumbnail_cache_dir().display(),
        );
        ui.set_performance_footprint(ss(footprint));
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
        write_native_json_async("session.json", &self.tabs);
    }

    fn search_root(&self) -> String {
        if self.search_all_scope {
            drive_root_for_path(&self.current_path)
        } else {
            self.current_path.clone()
        }
    }

    fn sync_search_scope(&self, ui: &MainWindow) {
        ui.set_search_scope_all(self.search_all_scope);
        ui.set_search_scope_label(ss(if self.search_all_scope {
            compact_drive_label(&self.current_path)
        } else {
            "Folder".to_string()
        }));
        ui.set_search_semantic_mode(self.settings.search_semantic_mode);
        ui.set_clip_search_enabled(self.settings.clip_search_enabled);
    }

    fn toggle_search_scope(&mut self, ui: &MainWindow) {
        self.search_all_scope = !self.search_all_scope;
        self.sync_search_scope(ui);
        if self.search_query.trim().len() >= 2 {
            self.search(ui, self.search_query.clone());
        } else {
            self.show_toast(
                ui,
                if self.search_all_scope {
                    format!("Search scope: {}", drive_root_for_path(&self.current_path))
                } else {
                    "Search scope: current folder".to_string()
                },
            );
        }
    }

    fn active_directory(&self) -> &str {
        if self.active_pane == ActivePane::Secondary && !self.secondary_path.is_empty() {
            &self.secondary_path
        } else {
            &self.current_path
        }
    }

    fn default_secondary_path(&self) -> String {
        if !self.secondary_path.is_empty()
            && Path::new(&self.secondary_path).is_dir()
            && !same_path_string(&self.secondary_path, &self.current_path)
        {
            return self.secondary_path.clone();
        }

        if let Some(parent) = Path::new(&self.current_path).parent() {
            let parent = parent.to_string_lossy().to_string();
            if !parent.is_empty() && !same_path_string(&parent, &self.current_path) {
                return parent;
            }
        }

        for folder in &self.known_folders {
            if Path::new(&folder.path).is_dir()
                && !same_path_string(&folder.path, &self.current_path)
            {
                return folder.path.clone();
            }
        }

        self.current_path.clone()
    }

    fn selected_entry(&self) -> Option<FileEntry> {
        if self.active_pane == ActivePane::Secondary {
            if self.secondary_selected_index >= 0 {
                let idx = self.secondary_selected_index as usize;
                if self.secondary_selected_set.is_empty()
                    || self.secondary_selected_set.contains(&idx)
                {
                    return self.secondary_visible_files.get(idx).cloned();
                }
            }
            return self
                .secondary_selected_set
                .iter()
                .min()
                .and_then(|&i| self.secondary_visible_files.get(i).cloned());
        }

        if self.selected_index >= 0 {
            let idx = self.selected_index as usize;
            if self.selected_set.is_empty() || self.selected_set.contains(&idx) {
                return self.visible_files.get(idx).cloned();
            }
        }
        self.selected_set
            .iter()
            .min()
            .and_then(|&i| self.visible_files.get(i).cloned())
    }

    fn primary_rename_index(&self) -> Option<i32> {
        if !self.selected_set.is_empty() {
            if self.selected_set.len() != 1 {
                return None;
            }
            return self.selected_set.iter().next().copied().map(|i| i as i32);
        }
        if self.selected_index >= 0 {
            return Some(self.selected_index);
        }
        None
    }

    fn secondary_rename_index(&self) -> Option<i32> {
        if !self.secondary_selected_set.is_empty() {
            if self.secondary_selected_set.len() != 1 {
                return None;
            }
            return self
                .secondary_selected_set
                .iter()
                .next()
                .copied()
                .map(|i| i as i32);
        }
        if self.secondary_selected_index >= 0 {
            return Some(self.secondary_selected_index);
        }
        None
    }

    fn cancel_inline_rename(&self, ui: &MainWindow) {
        if ui.get_rename_index() >= 0 {
            ui.set_rename_index(-1);
        }
    }

    fn active_selected_indices(&self) -> Vec<usize> {
        if self.active_pane == ActivePane::Secondary {
            if !self.secondary_selected_set.is_empty() {
                let mut sorted: Vec<usize> =
                    self.secondary_selected_set.iter().copied().collect();
                sorted.sort_unstable();
                return sorted;
            }
            if self.secondary_selected_index >= 0 {
                return vec![self.secondary_selected_index as usize];
            }
            return Vec::new();
        }

        if !self.selected_set.is_empty() {
            let mut sorted: Vec<usize> = self.selected_set.iter().copied().collect();
            sorted.sort_unstable();
            return sorted;
        }
        if self.selected_index >= 0 {
            return vec![self.selected_index as usize];
        }
        Vec::new()
    }

    fn active_visible_files(&self) -> &[FileEntry] {
        if self.active_pane == ActivePane::Secondary {
            &self.secondary_visible_files
        } else {
            &self.visible_files
        }
    }

    fn selected_entries(&self) -> Vec<FileEntry> {
        self.active_selected_indices()
            .into_iter()
            .filter_map(|i| self.active_visible_files().get(i).cloned())
            .collect()
    }

    fn selected_paths(&self) -> Vec<String> {
        self.selected_entries()
            .into_iter()
            .map(|entry| entry.path)
            .collect()
    }

    fn active_path_is_recycle_bin(&self) -> bool {
        if self.active_pane == ActivePane::Secondary {
            self.secondary_path == "recycle://"
        } else {
            self.current_path == "recycle://"
        }
    }

    fn apply_sort(&mut self) {
        sort_entries_by(&mut self.visible_files, &self.sort_by, &self.sort_dir);
    }

    fn apply_secondary_sort(&mut self) {
        sort_entries_by(
            &mut self.secondary_visible_files,
            &self.secondary_sort_by,
            &self.secondary_sort_dir,
        );
    }

    fn apply_folder_filter(&mut self) {
        let filter = self.folder_filter.trim().to_lowercase();
        if filter.is_empty() {
            return;
        }
        self.visible_files
            .retain(|e| e.name_lower.contains(&filter));
    }

    /// Returns true if an entry should be hidden from view unless show_hidden
    /// is enabled. Currently matches: names starting with `.` (Unix dotfiles
    /// like `.git` and `.DS_Store`) and any file with the `.ini` extension
    /// (Windows shell metadata files such as `desktop.ini` and `thumbs.ini`).
    fn is_hidden_entry(entry: &FileEntry) -> bool {
        if entry.name.starts_with('.') {
            return true;
        }
        if entry
            .extension
            .as_deref()
            .map(|e| e.eq_ignore_ascii_case("ini"))
            .unwrap_or(false)
        {
            return true;
        }
        false
    }

    fn entries_for_tag(&self, tag_id: &str) -> Vec<FileEntry> {
        let mut entries: Vec<FileEntry> = self
            .tags
            .iter()
            .filter(|(_, tag)| tag.eq_ignore_ascii_case(tag_id))
            .filter_map(|(path, _)| {
                let path_buf = PathBuf::from(path);
                let metadata = fs::metadata(&path_buf).ok()?;
                let entry = path_to_entry(&path_buf, &metadata);
                (self.show_hidden || !Self::is_hidden_entry(&entry)).then_some(entry)
            })
            .collect();
        sort_entries(&mut entries);
        entries
    }

    fn apply_filter(&mut self) {
        let query = self.search_query.trim().to_lowercase();
        self.visible_files.clear();
        if query.is_empty() {
            if self.show_hidden {
                self.visible_files.extend_from_slice(&self.files);
            } else {
                self.visible_files.extend(
                    self.files
                        .iter()
                        .filter(|e| !Self::is_hidden_entry(e))
                        .cloned(),
                );
            }
            self.apply_sort();
            self.apply_folder_filter();
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
                        let in_downloads = dirs::download_dir()
                            .map(|d| {
                                same_path_string(
                                    &self.current_path,
                                    &d.to_string_lossy(),
                                )
                            })
                            .unwrap_or(false);
                        entry.kind != FileKind::Directory
                            && in_downloads
                            && entry.modified < now.saturating_sub(30 * 24 * 60 * 60)
                    }
                    "screenshots" => {
                        matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp")
                            && entry.name_lower.contains("screenshot")
                    }
                    "git-untracked" => self.git_for_entry(entry) == "untracked",
                    _ => false,
                };
                if matched && (self.show_hidden || !Self::is_hidden_entry(entry)) {
                    self.visible_files.push(entry.clone());
                }
            }
            self.apply_sort();
            return;
        }
        if let Some(expected) = query.strip_prefix("tag:") {
            self.visible_files = self.entries_for_tag(expected);
            self.apply_sort();
            return;
        }
        for entry in &self.files {
            let ext = entry.extension.as_deref().unwrap_or("").to_lowercase();
            let matched = if let Some(expected) = query.strip_prefix("ext:") {
                ext == expected.trim_start_matches('.')
            } else if let Some(expected) = query.strip_prefix("name:") {
                entry.name_lower.contains(expected)
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
                entry.name_lower.contains(&query)
                    || entry.path.to_lowercase().contains(&query)
                    || ext.contains(&query)
            };
            if matched && (self.show_hidden || !Self::is_hidden_entry(entry)) {
                self.visible_files.push(entry.clone());
            }
        }
        self.apply_sort();
    }

    fn drive_space_for_status(&mut self, path: &str) -> Option<(u64, u64)> {
        let key = drive_space_cache_key(path);
        if let Some((free, total, loaded_at)) = self.drive_space_cache.get(&key).copied()
            && loaded_at.elapsed() < DRIVE_SPACE_CACHE_TTL
        {
            return Some((free, total));
        }
        let space = drive_free_space(path)?;
        self.drive_space_cache
            .insert(key, (space.0, space.1, Instant::now()));
        Some(space)
    }

    fn update_status(&mut self, ui: &MainWindow) {
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
                    "{d} folder{} | {f} file{}",
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
            format!("{sel_count} selected | {}", format_size_short(sel_size))
        };

        ui.set_status_left(ss(left));

        // Status right shows path + free space on the current drive when available.
        let shown_path = self
            .active_archive
            .as_ref()
            .map(|archive| archive_display_path(&archive.archive_path, &archive.prefix))
            .unwrap_or_else(|| self.current_path.clone());
        let current_path = self.current_path.clone();
        let right = match self.drive_space_for_status(&current_path) {
            Some((free, total)) => format!(
                "{} | {} free of {}",
                shown_path,
                format_size_short(free),
                format_size_short(total)
            ),
            None => shown_path,
        };
        ui.set_status_right(ss(right));
    }

    /// Keep Slint `selected_count` aligned with the active pane's selection sets.
    /// Call after `update_models` / any path that clears or rebuilds selection
    /// without going through `update_selection_in_model`.
    fn selection_has_archive(&self) -> bool {
        if self.active_archive.is_some() {
            return true;
        }
        !self.selected_disk_archives().is_empty()
    }

    fn sync_selection_count_to_ui(&self, ui: &MainWindow) {
        let sel_count = self.active_selected_indices().len();
        ui.set_selected_count(sel_count as i32);
        ui.set_selection_has_archive(self.selection_has_archive());
    }

    fn sync_sidebar_models(&self, ui: &MainWindow) {
        ui.set_side_items(model_from_vec(self.side_items()));
        ui.set_side_items_simple(model_from_vec(self.side_items_simple()));
    }

    fn update_file_models(&mut self, ui: &MainWindow) {
        self.update_file_models_inner(ui, true);
    }

    /// Rebuild the file list without shell-icon probes or thumbnail disk I/O
    /// so the first paint after navigation stays responsive.
    fn update_file_models_quick(&mut self, ui: &MainWindow) {
        self.update_file_models_inner(ui, false);
    }

    fn update_file_models_inner(&mut self, ui: &MainWindow, enrich_visible: bool) {
        if enrich_visible {
            // Populate the shell-icon cache for visible entries.
            #[cfg(target_os = "windows")]
            self.populate_system_icons(32);
        }
        // Pre-load cached thumbnails for the first visible image files in any view.
        if enrich_visible {
            for entry in self.visible_files.iter().take(24) {
                let ext = entry.extension.as_deref().unwrap_or("").to_lowercase();
                if !is_thumbnail_image_ext(&ext) || self.thumbnail_memory.contains_key(&entry.path)
                {
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
        let show_date_groups = self.sort_by == "modified";
        let mut last_group: &'static str = "";
        let items: Vec<FileItem> = self
            .visible_files
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let mut item = self.file_item(entry, self.selected_set.contains(&i));
                if show_date_groups {
                    let group = date_group_label(entry.modified);
                    item.show_date_group_header = group != last_group;
                    item.date_group_text = SharedString::from(group);
                    last_group = group;
                }
                item
            })
            .collect();
        let model = model_from_vec(items);
        ui.set_files(model.clone());
        self.files_model = Some(model);
        let tabs = self.tab_items();
        #[cfg(target_os = "windows")]
        sync_titlebar_hit_regions(&tabs);
        ui.set_tabs(model_from_vec(tabs));
        ui.set_selected_index(self.selected_index);
        self.sync_selection_count_to_ui(ui);
        self.sync_search_scope(ui);
        self.sync_tag_names(ui);
        let shown_path = navigation_display_path(
            &self.current_path,
            self.active_archive.as_ref(),
        );
        ui.set_current_path(ss(&shown_path));
        ui.set_address_text(ss(&shown_path));
        ui.set_search_text(ss(&self.search_query));
        ui.set_breadcrumbs(model_from_vec(
            self.active_archive
                .as_ref()
                .map(|archive| archive_breadcrumbs(&archive.archive_path, &archive.prefix))
                .unwrap_or_else(|| build_breadcrumbs(&self.current_path)),
        ));
        self.update_status(ui);
    }

    fn sync_nav_chrome(&mut self, ui: &MainWindow) {
        let tabs = self.tab_items();
        #[cfg(target_os = "windows")]
        sync_titlebar_hit_regions(&tabs);
        ui.set_tabs(model_from_vec(tabs));
        ui.set_selected_index(self.selected_index);
        self.sync_selection_count_to_ui(ui);
        self.sync_search_scope(ui);
        self.sync_tag_names(ui);
        let shown_path = navigation_display_path(
            &self.current_path,
            self.active_archive.as_ref(),
        );
        ui.set_current_path(ss(&shown_path));
        ui.set_address_text(ss(&shown_path));
        ui.set_search_text(ss(&self.search_query));
        ui.set_breadcrumbs(model_from_vec(
            self.active_archive
                .as_ref()
                .map(|archive| archive_breadcrumbs(&archive.archive_path, &archive.prefix))
                .unwrap_or_else(|| build_breadcrumbs(&self.current_path)),
        ));
        self.update_status(ui);
    }

    fn pump_visible_enrichment(&mut self, ui: &MainWindow) {
        if !self.enrich_visible_pending {
            return;
        }
        self.enrich_visible_pending = false;
        self.update_file_models(ui);
    }

    fn update_models(&mut self, ui: &MainWindow) {
        self.update_file_models(ui);
        self.sync_sidebar_models(ui);
    }

    fn bump_nav_generation(&mut self) -> u64 {
        self.nav_generation.fetch_add(1, Ordering::SeqCst) + 1
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
        self.sync_selection_count_to_ui(ui);
        self.update_status(ui);
    }

    /// Pull system icons for the first `max_entries` visible files. Per-path
    /// extraction is reserved for executables and shortcuts since those carry
    /// unique embedded icons. Everything else falls back to a per-extension
    /// probe (a single SHGetFileInfo call with USEFILEATTRIBUTES is enough
    /// to get the registered type icon without touching the disk).
    #[cfg(target_os = "windows")]
    fn populate_system_icons(&mut self, max_entries: usize) {
        let mut needed_extensions: Vec<String> = Vec::new();
        let mut needed_paths: Vec<String> = Vec::new();
        for entry in self.visible_files.iter().take(max_entries) {
            if entry.kind == FileKind::Directory {
                continue;
            }
            let ext = entry
                .extension
                .as_deref()
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            let is_per_path = matches!(ext.as_str(), "exe" | "lnk" | "ico" | "msi");
            if is_per_path {
                if needed_paths.len() < 8 && !self.system_icon_by_path.contains_key(&entry.path) {
                    needed_paths.push(entry.path.clone());
                }
            } else if !ext.is_empty() && !self.system_icon_by_ext.contains_key(&ext) {
                if !needed_extensions.iter().any(|e| e == &ext) {
                    needed_extensions.push(ext);
                }
            }
        }
        for ext in needed_extensions {
            // A synthetic name like `_pathfinder_probe.docx` plus
            // USEFILEATTRIBUTES makes the shell return the registered icon
            // for that extension without touching the disk.
            let probe = format!("_pathfinder_probe.{ext}");
            if let Some(img) = file_icons::extract_icon_rgba(&probe, false) {
                self.system_icon_by_ext.insert(ext, img);
            }
        }
        for path in needed_paths {
            if let Some(img) = file_icons::icon_for(&path) {
                self.system_icon_by_path.insert(path, img);
            }
            // Cap path-keyed cache at 512 entries to bound memory.
            if self.system_icon_by_path.len() > 512 {
                if let Some(k) = self.system_icon_by_path.keys().next().cloned() {
                    self.system_icon_by_path.remove(&k);
                }
            }
        }
    }

    fn file_item(&self, entry: &FileEntry, selected: bool) -> FileItem {
        let in_recycle = self.current_path == "recycle://";
        let tag_id = self.tags.get(&entry.path).cloned().unwrap_or_default();
        let git_status = self.git_for_entry(entry);
        let (has_thumbnail, thumbnail) = self
            .thumbnail_memory
            .get(&entry.path)
            .map(|img| (true, img.clone()))
            .unwrap_or((false, slint::Image::default()));

        // System icon: prefer a per-path icon when available (.exe / .lnk /
        // .ico / .msi), otherwise fall back to a per-extension cache.
        #[cfg(target_os = "windows")]
        let (has_system_icon, system_icon) = if entry.kind == FileKind::Directory {
            (false, slint::Image::default())
        } else {
            let path_icon = self.system_icon_by_path.get(&entry.path);
            if let Some(img) = path_icon {
                (true, img.clone())
            } else {
                let ext_lower = entry
                    .extension
                    .as_deref()
                    .map(|e| e.to_ascii_lowercase())
                    .unwrap_or_default();
                if let Some(img) = self.system_icon_by_ext.get(&ext_lower) {
                    (true, img.clone())
                } else {
                    (false, slint::Image::default())
                }
            }
        };
        #[cfg(not(target_os = "windows"))]
        let (has_system_icon, system_icon) = (false, slint::Image::default());
        let rename_label = if entry.kind == FileKind::Directory {
            entry.name.clone()
        } else {
            let p = std::path::Path::new(&entry.name);
            if entry.name.starts_with('.') && entry.name.matches('.').count() <= 1 {
                entry.name.clone()
            } else {
                p.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| entry.name.clone())
            }
        };
        FileItem {
            name: ss(&entry.name),
            file_path: ss(&entry.path),
            is_dir: entry.kind == FileKind::Directory,
            size_text: ss(if entry.kind == FileKind::Directory {
                String::new()
            } else {
                format_size_short(entry.size)
            }),
            modified_text: ss(if in_recycle {
                // In the recycle view, the `modified` field carries the
                // deletion timestamp (set by list_recycle_bin_entries).
                format!("Deleted {}", format_modified(entry.modified))
            } else {
                format_modified(entry.modified)
            }),
            type_text: ss(if in_recycle {
                let original = entry.path.strip_prefix("recycle://").unwrap_or(&entry.path);
                std::path::Path::new(original)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            } else {
                entry_type(entry).to_string()
            }),
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
            has_system_icon,
            system_icon,
            // Group fields are filled in after the items are zipped with the
            // computed group order in apply_visible_files. Defaults here so the
            // rest of file_item callers (single-row updates) don't have to know
            // about grouping.
            date_group_text: SharedString::new(),
            show_date_group_header: false,
            is_cut_pending: self.entry_is_cut_pending(&entry.path),
            rename_label: ss(&rename_label),
        }
    }

    fn entry_is_cut_pending(&self, path: &str) -> bool {
        let Some(clip) = &self.clipboard else {
            return false;
        };
        if !clip.cut {
            return false;
        }
        clip.paths.iter().any(|p| same_path_string(p, path))
    }

    fn is_clipboardable_path(path: &str) -> bool {
        !path.is_empty()
            && !path.starts_with("recycle://")
            && !is_virtual_nav_path(path)
            && Path::new(path).exists()
    }

    fn paste_invalidate_dirs(dest_dir: &str, sources: &[String], cut: bool) -> Vec<String> {
        let mut dirs = vec![dest_dir.to_string()];
        if cut {
            for src in sources {
                if let Some(parent) = Path::new(src).parent() {
                    let parent = parent.to_string_lossy().to_string();
                    if !parent.is_empty()
                        && !dirs.iter().any(|d| same_path_string(d, &parent))
                    {
                        dirs.push(parent);
                    }
                }
            }
        }
        dirs.retain(|d| !is_virtual_nav_path(d) && Path::new(d).is_dir());
        dirs
    }

    fn invalidate_paste_directories(&self, dirs: &[String]) {
        for dir in dirs {
            self.app_state
                .invalidate_directory_path(Path::new(dir));
        }
    }

    #[cfg(target_os = "windows")]
    fn sync_shell_clipboard_if_needed(&mut self) {
        if self.clipboard.is_some() {
            return;
        }
        if let Some((paths, cut)) = file_drag::try_read_shell_files_clipboard() {
            if !paths.is_empty() {
                self.clipboard = Some(NativeClipboard { paths, cut });
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn sync_shell_clipboard_if_needed(&mut self) {}

    fn refresh_clipboard_visuals(&mut self, ui: &MainWindow) {
        self.update_models(ui);
        if ui.get_dual_pane() {
            self.update_secondary_models(ui);
        }
    }

    /// Reload the pane(s) that show pasted files after copy or move.
    fn refresh_after_paste(
        &mut self,
        ui: &MainWindow,
        cut: bool,
        pasted_sources: &[String],
    ) {
        let dest_dir = self.active_directory().to_string();
        let invalidate_dirs = Self::paste_invalidate_dirs(&dest_dir, pasted_sources, cut);
        self.invalidate_paste_directories(&invalidate_dirs);

        if cut {
            self.clipboard = None;
            #[cfg(target_os = "windows")]
            let _ = file_drag::clear_shell_files_clipboard();
            self.refresh_clipboard_visuals(ui);
        }
        if ui.get_dual_pane() {
            self.invalidate_and_refresh_both_panes(ui);
        } else if self.active_pane == ActivePane::Secondary {
            let path = self.secondary_path.clone();
            self.secondary_navigate(ui, path);
        } else {
            self.refresh(ui);
        }
    }

    fn paste_skips_dest(&self, src: &str, dest: &Path, cut: bool) -> bool {
        if !same_path_string(src, &dest.to_string_lossy()) {
            return false;
        }
        cut
    }

    fn paste_has_real_conflict(&self, src: &str, dest: &Path) -> bool {
        dest.exists() && !same_path_string(src, &dest.to_string_lossy())
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

    /// Pick a sidebar icon name for a path. Tries (in order):
    ///   1. exact match against any known-folder path -> that folder's icon
    ///   2. case-insensitive match of the basename against well-known names
    ///      (Documents/Music/Videos/Pictures/Downloads/Desktop/Home)
    ///   3. fallback to generic "folder"
    fn icon_for_path(&self, path: &str, label: &str) -> &'static str {
        for kf in &self.known_folders {
            if same_path_string(&kf.path, path) {
                return match kf.id.as_str() {
                    "home" => "home",
                    "downloads" => "download",
                    "pictures" => "image",
                    "documents" | "desktop" => "documents",
                    "music" => "music",
                    "videos" | "video" => "video",
                    _ => "folder",
                };
            }
        }
        match label.to_lowercase().as_str() {
            "documents" | "desktop" | "onedrive" | "icloud drive" => "documents",
            "downloads" => "download",
            "pictures" | "screenshots" | "camera roll" => "image",
            "music" => "music",
            "videos" | "movies" => "video",
            "home" => "home",
            _ => "folder",
        }
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
        // Home dashboard — virtual landing for drives, pins, and saved searches.
        items.push(SideItem {
            label: ss("Home"),
            path: ss("home://"),
            icon: ss("home"),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: false,
            active: self.current_path == "home://",
        });
        for folder in &self.known_folders {
            // The Home landing (home://) above already represents the user's
            // home location, so skip the redundant "home" known-folder entry.
            // Otherwise the sidebar shows two identical "Home" rows.
            if folder.id == "home" {
                continue;
            }
            items.push(SideItem {
                label: ss(&folder.name),
                path: ss(&folder.path),
                icon: ss(match folder.id.as_str() {
                    "home" => "home",
                    "downloads" => "download",
                    "pictures" => "image",
                    "documents" | "desktop" => "documents",
                    "music" => "music",
                    "videos" | "video" => "video",
                    _ => "folder",
                }),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: false,
                active: same_path_string(&self.current_path, &folder.path),
            });
        }

        // Recycle Bin entry - virtual `recycle://` path. Click to browse,
        // right-click items inside to restore or delete permanently.
        items.push(SideItem {
            label: ss("Recycle Bin"),
            path: ss("recycle://"),
            icon: ss("trash"),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: false,
            active: self.current_path == "recycle://",
        });

        // Storage analyzer - virtual `storage://` path. Click swaps the main
        // pane to the categorized storage view (Apps, Documents, Pictures,
        // etc.) plus a ranked-by-size list of biggest items.
        items.push(SideItem {
            label: ss("Storage"),
            path: ss("storage://"),
            icon: ss("storage"),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: false,
            active: self.current_path == "storage://",
        });

        if !self.user_pins.is_empty() {
            items.push(SideItem {
                label: ss("PINNED"),
                path: ss(""),
                icon: ss(""),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: true,
                active: false,
            });
            for pin in self.user_pins.iter().take(12) {
                items.push(SideItem {
                    label: ss(&pin.name),
                    path: ss(&pin.path),
                    icon: ss(if pin.kind == "file" {
                        self.icon_for_path(&pin.path, &pin.name)
                    } else {
                        "folder"
                    }),
                    count: ss(""),
                    color: rgba_u8(0, 0, 0, 0.0),
                    is_header: false,
                    active: same_path_string(&self.current_path, &pin.path),
                });
            }
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
                let label_str = Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone());
                items.push(SideItem {
                    label: ss(&label_str),
                    path: ss(path),
                    icon: ss(self.icon_for_path(path, &label_str)),
                    count: ss(""),
                    color: rgba_u8(0, 0, 0, 0.0),
                    is_header: false,
                    active: same_path_string(&self.current_path, path),
                });
            }
        }

        let smart = smart_folders_for_path(&self.current_path);
        items.push(SideItem {
            label: ss("SMART FOLDERS"),
            path: ss(""),
            icon: ss(""),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: true,
            active: false,
        });
        for folder in smart {
            items.push(SideItem {
                label: ss(&folder.name),
                path: ss(format!("smart:{}", folder.id)),
                icon: ss("folder"),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: false,
                active: self.search_query == format!("smart:{}", folder.id)
                    || self.search_query == folder.query,
            });
        }

        let saved = read_native_json::<Vec<SavedSearch>>("searches.json", Vec::new());
        if !saved.is_empty() {
            items.push(SideItem {
                label: ss("SAVED SEARCHES"),
                path: ss(""),
                icon: ss(""),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: true,
                active: false,
            });
            for search in saved.into_iter().take(8) {
                let encoded = format!("search:{}", search.name);
                items.push(SideItem {
                    label: ss(&search.name),
                    path: ss(&encoded),
                    icon: ss("search"),
                    count: ss(""),
                    color: rgba_u8(0, 0, 0, 0.0),
                    is_header: false,
                    active: self.search_query == search.query,
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
        for (id, default_label) in [
            ("red", "Urgent"),
            ("orange", "Important"),
            ("yellow", "Review"),
            ("green", "Done"),
            ("blue", "Personal"),
            ("violet", "Code"),
        ] {
            let label = self
                .tag_labels
                .get(id)
                .map(|s| s.as_str())
                .unwrap_or(default_label);
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

    fn side_items_simple(&self) -> Vec<SideItem> {
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
        items.push(SideItem {
            label: ss("Home"),
            path: ss("home://"),
            icon: ss("home"),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: false,
            active: self.current_path == "home://",
        });
        for folder in &self.known_folders {
            // The Home landing (home://) above already represents the user's
            // home location, so skip the redundant "home" known-folder entry.
            // Otherwise the sidebar shows two identical "Home" rows.
            if folder.id == "home" {
                continue;
            }
            items.push(SideItem {
                label: ss(&folder.name),
                path: ss(&folder.path),
                icon: ss(match folder.id.as_str() {
                    "home" => "home",
                    "downloads" => "download",
                    "pictures" => "image",
                    "documents" | "desktop" => "documents",
                    "music" => "music",
                    "videos" | "video" => "video",
                    _ => "folder",
                }),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: false,
                active: same_path_string(&self.current_path, &folder.path),
            });
        }
        if !self.user_pins.is_empty() {
            items.push(SideItem {
                label: ss("PINNED"),
                path: ss(""),
                icon: ss(""),
                count: ss(""),
                color: rgba_u8(0, 0, 0, 0.0),
                is_header: true,
                active: false,
            });
            for pin in self.user_pins.iter().take(6) {
                items.push(SideItem {
                    label: ss(&pin.name),
                    path: ss(&pin.path),
                    icon: ss(if pin.kind == "file" {
                        self.icon_for_path(&pin.path, &pin.name)
                    } else {
                        "folder"
                    }),
                    count: ss(""),
                    color: rgba_u8(0, 0, 0, 0.0),
                    is_header: false,
                    active: same_path_string(&self.current_path, &pin.path),
                });
            }
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
            label: ss("Recycle Bin"),
            path: ss("recycle://"),
            icon: ss("trash"),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: false,
            active: self.current_path == "recycle://",
        });
        items.push(SideItem {
            label: ss("Storage"),
            path: ss("storage://"),
            icon: ss("storage"),
            count: ss(""),
            color: rgba_u8(0, 0, 0, 0.0),
            is_header: false,
            active: self.current_path == "storage://",
        });
        items
    }

    fn tab_items(&self) -> Vec<TabItem> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let title = match tab.path.as_str() {
                    "recycle://" => "Recycle Bin".to_string(),
                    "storage://" => "Storage".to_string(),
                    "home://" => "Home".to_string(),
                    _ => Path::new(&tab.path)
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| tab.path.clone()),
                };
                TabItem {
                    title: ss(title),
                    path: ss(&tab.path),
                    active: index == self.active_tab,
                }
            })
            .collect()
    }

    fn open_archive_view(
        &mut self,
        ui: &MainWindow,
        archive_path: String,
        prefix: String,
        push_history: bool,
    ) {
        self.active_pane = ActivePane::Primary;
        let prefix = normalize_archive_prefix(&prefix);
        match list_archive_virtual_dir(&archive_path, &prefix) {
            Ok(files) => {
                let return_path = self
                    .active_archive
                    .as_ref()
                    .map(|archive| archive.return_path.clone())
                    .unwrap_or_else(|| self.current_path.clone());
                let virtual_path = archive_virtual_path(&archive_path, &prefix);
                self.active_archive = Some(ArchiveView {
                    archive_path: archive_path.clone(),
                    prefix: prefix.clone(),
                    return_path,
                });
                self.current_path = virtual_path.clone();
                self.files = files;
                self.visible_files.clear();
                self.search_query.clear();
                self.selected_index = -1;
                self.selected_set.clear();
                self.select_anchor = -1;
                self.files_model = None;
                self.git_status = Arc::new(GitStatusMap::new());
                self.git_dir_status.clear();
                ui.set_view_mode(ss("list"));
                ui.set_empty_state(ss(""));
                self.apply_filter();
                self.update_models(ui);
                self.update_preview(ui);
                if push_history {
                    self.history.truncate(self.history_index + 1);
                    self.history.push(virtual_path);
                    self.history_index = self.history.len().saturating_sub(1);
                }
            }
            Err(error) => self.show_toast_kind(ui, error, "error"),
        }
    }

    fn prepare_navigate_loading(&mut self, ui: &MainWindow, path: &str, push_history: bool) {
        self.active_archive = None;
        ui.set_in_recycle_bin(false);
        let prev_path = self.current_path.clone();
        if !prev_path.is_empty() {
            let y = ui.get_primary_list_scroll_y();
            self.path_scroll.insert(prev_path.clone(), y);
            self.folder_views
                .insert(format!("{prev_path}:view"), ui.get_view_mode().to_string());
            self.folder_views
                .insert(format!("{prev_path}:sort_by"), self.sort_by.clone());
            self.folder_views
                .insert(format!("{prev_path}:sort_dir"), self.sort_dir.clone());
        }
        self.current_path = path.to_string();
        self.thumbnail_memory.retain(|k, _| k.starts_with(path));
        self.recent_locations
            .retain(|p| !same_path_string(p, path));
        self.recent_locations.insert(0, path.to_string());
        if self.recent_locations.len() > 12 {
            self.recent_locations.truncate(12);
        }
        write_native_json_async("recent_locations.json", &self.recent_locations);
        if push_history {
            self.history.truncate(self.history_index + 1);
            self.history.push(path.to_string());
            self.history_index = self.history.len().saturating_sub(1);
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.path = path.to_string();
        }
        self.files.clear();
        self.visible_files.clear();
        self.search_query.clear();
        self.selected_index = -1;
        self.selected_set.clear();
        self.select_anchor = -1;
        self.files_model = None;
        ui.set_empty_state(ss("Loading folder..."));
        ui.set_primary_list_scroll_y(0.0);
        ui.set_files(model_from_vec(Vec::<FileItem>::new()));
        self.sync_nav_chrome(ui);
        self.sync_sidebar_models(ui);
    }

    fn apply_directory_listing(
        &mut self,
        ui: &MainWindow,
        path: String,
        page: DirectoryPage,
        push_history: bool,
        path_already_committed: bool,
    ) {
        self.active_archive = None;
        ui.set_in_recycle_bin(false);
        let partial = page.partial;
        let skipped_entries = page.skipped_entries;
        let files = page.entries;

        if !path_already_committed {
            let prev_path = self.current_path.clone();
            if !prev_path.is_empty() {
                let y = ui.get_primary_list_scroll_y();
                self.path_scroll.insert(prev_path.clone(), y);
                self.folder_views
                    .insert(format!("{prev_path}:view"), ui.get_view_mode().to_string());
                self.folder_views
                    .insert(format!("{prev_path}:sort_by"), self.sort_by.clone());
                self.folder_views
                    .insert(format!("{prev_path}:sort_dir"), self.sort_dir.clone());
            }
            self.current_path = path.clone();
            self.thumbnail_memory.retain(|k, _| k.starts_with(&path));
            self.recent_locations
                .retain(|p| !same_path_string(p, &path));
            self.recent_locations.insert(0, path.clone());
            if self.recent_locations.len() > 12 {
                self.recent_locations.truncate(12);
            }
            write_native_json_async("recent_locations.json", &self.recent_locations);
            if push_history {
                self.history.truncate(self.history_index + 1);
                self.history.push(path.clone());
                self.history_index = self.history.len().saturating_sub(1);
            }
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.path = path.clone();
            }
        }

        let lower = path.to_ascii_lowercase();
        let force_list = is_always_list_view_folder(&lower);
        if force_list {
            ui.set_view_mode(ss("list"));
            self.folder_views
                .insert(format!("{path}:view"), "list".to_string());
        } else if let Some(view) = self.folder_views.get(&format!("{path}:view")).cloned() {
            ui.set_view_mode(ss(&view));
        } else {
            let is_media_folder = lower.contains("\\pictures")
                || lower.contains("/pictures")
                || lower.contains("\\videos")
                || lower.contains("/videos")
                || lower.contains("\\camera roll")
                || lower.contains("/camera roll")
                || lower.contains("\\screenshots")
                || lower.contains("/screenshots");
            let default_view = if is_media_folder { "gallery" } else { "list" };
            ui.set_view_mode(ss(default_view));
            self.folder_views
                .insert(format!("{path}:view"), default_view.to_string());
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
        if self.files.len() <= LARGE_DIRECTORY_GIT_CAP {
            let ready = self.git_status_ready.clone();
            let pending = self.pending_git_status.clone();
            let state = self.app_state.clone();
            let p = path.clone();
            std::thread::spawn(move || {
                if !is_inside_git_worktree(Path::new(&p)) {
                    return;
                }
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
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.view = ui.get_view_mode().to_string();
        }

        ui.set_empty_state(ss(if partial {
            format!(
                "Showing the first {} items while the full folder loads.",
                self.files.len()
            )
        } else {
            String::new()
        }));

        self.apply_filter();
        self.update_file_models_quick(ui);
        self.sync_sidebar_models(ui);
        self.enrich_visible_pending = true;
        self.update_preview(ui);
        self.save_session();

        if skipped_entries > 0 {
            self.show_toast_kind(
                ui,
                format!(
                    "{skipped_entries} item{} could not be listed (access denied)",
                    if skipped_entries == 1 { "" } else { "s" }
                ),
                "warning",
            );
        }

        let restore_y = self.path_scroll.get(&path).copied().unwrap_or(0.0);
        ui.set_primary_list_scroll_y(restore_y);

        if partial {
            self.schedule_full_directory_load(path.clone());
        }

        let image_entries: Vec<(String, u64)> = self
            .visible_files
            .iter()
            .filter(|e| is_thumbnail_image_ext(e.extension.as_deref().unwrap_or("")))
            .take(96)
            .map(|e| (e.path.clone(), e.modified))
            .collect();
        if !image_entries.is_empty() {
            let ready_flag = self.thumbnail_ready.clone();
            THUMBNAIL_POOL.spawn(move || {
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
                            if t
                                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
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

        let subdir_paths: Vec<String> = self
            .visible_files
            .iter()
            .filter(|e| e.kind == FileKind::Directory)
            .take(if APP_STARTED.elapsed() < Duration::from_secs(5) {
                4
            } else {
                8
            })
            .map(|e| e.path.clone())
            .collect();
        if !subdir_paths.is_empty() && APP_STARTED.elapsed() >= Duration::from_secs(2) {
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
    }

    fn start_async_directory_load(&mut self, ui: &MainWindow, path: String, push_history: bool) {
        self.prepare_navigate_loading(ui, &path, push_history);
        let token = self.nav_generation.load(Ordering::SeqCst);
        let state = self.app_state.clone();
        let ready = self.directory_ready.clone();
        let pending = self.pending_directory_result.clone();
        let generation = self.nav_generation.clone();
        std::thread::spawn(move || {
            let page = match list_directory_chunk(Path::new(&path), FIRST_DIRECTORY_CHUNK) {
                Ok(page) => page,
                Err(err) => {
                    if generation.load(Ordering::SeqCst) != token {
                        return;
                    }
                    if let Ok(mut lock) = pending.lock() {
                        *lock = Some(NativeDirectoryResult {
                            path: path.clone(),
                            entries: Vec::new(),
                            generation: token,
                            partial: false,
                            skipped_entries: 0,
                            error: Some(err),
                        });
                    }
                    ready.store(true, Ordering::Release);
                    return;
                }
            };
            if generation.load(Ordering::SeqCst) != token {
                return;
            }
            if !page.partial {
                state.store_directory(&path, page.entries.clone());
                schedule_index_directory(path.clone(), page.entries.clone());
            }
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(NativeDirectoryResult {
                    path: path.clone(),
                    entries: page.entries,
                    generation: token,
                    partial: page.partial,
                    skipped_entries: page.skipped_entries,
                    error: None,
                });
            }
            ready.store(true, Ordering::Release);
        });
    }

    fn queue_sidebar_navigate(
        &mut self,
        ui: &MainWindow,
        path: String,
        controller: Rc<RefCell<NativeController>>,
    ) {
        if same_path_string(&self.current_path, &path) {
            return;
        }
        self.sidebar_nav_pending = Some(path);
        let weak = ui.as_weak();
        if self.sidebar_nav_timer.is_none() {
            self.sidebar_nav_timer = Some(slint::Timer::default());
        }
        let timer = self
            .sidebar_nav_timer
            .as_ref()
            .expect("sidebar_nav_timer just initialized");
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(90),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let path = controller.borrow_mut().sidebar_nav_pending.take();
                    if let Some(path) = path {
                        controller.borrow_mut().navigate(&ui, path, true);
                    }
                }
            },
        );
    }

    fn side_activate_action(&mut self, ui: &MainWindow, index: i32) -> SidebarActivateAction {
        let items = if ui.get_ui_mode().as_str() == "simple" {
            self.side_items_simple()
        } else {
            self.side_items()
        };
        let Some(item) = items.get(index as usize) else {
            return SidebarActivateAction::None;
        };
        let path = item.path.to_string();
        if let Some(tag) = path.strip_prefix("tag:") {
            self.search_query = format!("tag:{tag}");
            self.apply_filter();
            self.selected_index = -1;
            self.update_models(ui);
            return SidebarActivateAction::None;
        }
        if let Some(name) = path.strip_prefix("search:") {
            let saved = read_native_json::<Vec<SavedSearch>>("searches.json", Vec::new());
            if let Some(search) = saved.into_iter().find(|s| s.name == name) {
                if !search.scope.is_empty() && !same_path_string(&self.current_path, &search.scope) {
                    return SidebarActivateAction::NavigateThenSearch {
                        path: search.scope,
                        query: search.query,
                    };
                }
                self.search_query = search.query;
                ui.set_search_text(ss(&self.search_query));
                self.apply_filter();
                self.selected_index = -1;
                self.update_models(ui);
                // Trigger full search for non-local filters.
                self.search(ui, self.search_query.clone());
            }
            return SidebarActivateAction::None;
        }
        if let Some(smart) = path.strip_prefix("smart:") {
            if smart == "old-downloads" {
                if let Some(downloads) = dirs::download_dir() {
                    let target = downloads.to_string_lossy().to_string();
                    let query = smart_folders_for_path(&self.current_path)
                        .into_iter()
                        .find(|f| f.id == smart)
                        .map(|f| f.query)
                        .unwrap_or_else(|| format!("smart:{smart}"));
                    if !same_path_string(&self.current_path, &target) {
                        return SidebarActivateAction::NavigateThenSearch {
                            path: target,
                            query,
                        };
                    }
                }
            }
            // Prefer the smart folder's explicit query when available.
            let query = smart_folders_for_path(&self.current_path)
                .into_iter()
                .find(|f| f.id == smart)
                .map(|f| f.query)
                .unwrap_or_else(|| format!("smart:{smart}"));
            self.search_query = query.clone();
            ui.set_search_text(ss(&query));
            self.apply_filter();
            self.selected_index = -1;
            self.update_models(ui);
            self.search(ui, self.search_query.clone());
            return SidebarActivateAction::None;
        }
        if path.is_empty() {
            return SidebarActivateAction::None;
        }
        if same_path_string(&self.current_path, &path) {
            return SidebarActivateAction::None;
        }
        SidebarActivateAction::Navigate(path)
    }

    fn navigate(&mut self, ui: &MainWindow, path: String, push_history: bool) {
        self.cancel_inline_rename(ui);
        self.active_pane = ActivePane::Primary;
        let raw = if path.trim().is_empty() {
            self.current_path.clone()
        } else {
            path
        };
        // Virtual namespaces must be handled before path-query resolution.
        // `storage://` and `recycle://` contain `/` and would otherwise be
        // joined against the current folder as bogus relative paths.
        if raw == "home://" {
            self.open_home_view(ui, push_history);
            return;
        }
        if raw == "recycle://" {
            if ui.get_is_storage_view() {
                self.close_storage_view(ui);
            }
            self.open_recycle_bin_view(ui, push_history);
            return;
        }
        if raw == "storage://" {
            self.open_storage_view(ui, push_history);
            return;
        }
        let path = if looks_like_path_query(&raw) {
            resolve_path_query(&raw, &self.current_path)
                .to_string_lossy()
                .into_owned()
        } else {
            raw
        };
        // Virtual recycle-bin namespace - content comes from trash::os_limited.
        if path == "home://" {
            self.open_home_view(ui, push_history);
            return;
        }
        if path == "recycle://" {
            if ui.get_is_storage_view() {
                self.close_storage_view(ui);
            }
            self.open_recycle_bin_view(ui, push_history);
            return;
        }
        // Virtual storage-analyzer namespace.
        if path == "storage://" {
            self.open_storage_view(ui, push_history);
            return;
        }
        // Any non-storage navigation closes the storage view if it was open.
        if ui.get_is_storage_view() {
            self.close_storage_view(ui);
        }
        if let Some((archive_path, prefix)) = parse_archive_virtual_path(&path) {
            self.open_archive_view(ui, archive_path, prefix, push_history);
            return;
        }
        // Only synchronously probe comma-joined paste mistakes. Normal folder
        // opens validate on a worker thread so slow/network paths never block
        // the UI thread during startup.
        if !path.is_empty() && path.contains(',') {
            if let Some(target) = recover_navigable_path(&path) {
                self.navigate(ui, target, push_history);
                return;
            }
            if !Path::new(&path).is_dir() {
                ui.set_empty_state(ss(format!("Cannot open \"{}\"", path)));
                return;
            }
        }
        if !same_path_string(&self.current_path, &path) {
            self.bump_nav_generation();
        }
        if let Some(entries) = self.app_state.cached_directory(&path) {
            let page = DirectoryPage {
                entries,
                partial: false,
                skipped_entries: 0,
            };
            self.apply_directory_listing(ui, path, page, push_history, false);
            return;
        }
        if let Some(entries) = list_directory_from_index(&path) {
            let page = DirectoryPage {
                entries,
                partial: true,
                skipped_entries: 0,
            };
            self.apply_directory_listing(ui, path, page, push_history, false);
            return;
        }
        self.start_async_directory_load(ui, path, push_history);
    }

    /// Switch the primary view to a virtual listing of the OS recycle bin.
    /// Right-click an item to restore (move back to original path) or delete
    /// permanently. The view is read-only otherwise - paste/new-file disabled.
    fn open_recycle_bin_view(&mut self, ui: &MainWindow, push_history: bool) {
        let entries = list_recycle_bin_entries();
        self.active_archive = None;
        if ui.get_is_storage_view() {
            self.close_storage_view(ui);
        }
        self.current_path = "recycle://".to_string();
        self.files = entries;
        self.search_query.clear();
        self.selected_index = -1;
        self.selected_set.clear();
        self.select_anchor = -1;
        self.files_model = None;
        if push_history {
            self.history.truncate(self.history_index + 1);
            self.history.push("recycle://".to_string());
            self.history_index = self.history.len().saturating_sub(1);
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.path = "recycle://".to_string();
        }
        ui.set_empty_state(ss(if self.files.is_empty() {
            "Recycle Bin is empty.".to_string()
        } else {
            String::new()
        }));
        ui.set_in_recycle_bin(true);
        self.apply_filter();
        self.update_models(ui);
        self.update_preview(ui);
    }

    /// Restore the currently selected recycle-bin items back to their original
    /// paths. Looks up each file by its original_path against `trash::os_limited::list()`.
    fn restore_from_recycle_bin(&mut self, ui: &MainWindow) {
        let virtual_paths = self.selected_paths();
        let target_originals: Vec<String> = virtual_paths
            .iter()
            .filter_map(|p| p.strip_prefix("recycle://").map(|s| s.to_string()))
            .collect();
        if target_originals.is_empty() {
            return;
        }
        let items = match trash::os_limited::list() {
            Ok(it) => it,
            Err(e) => {
                self.show_toast_kind(ui, format!("Cannot read trash: {e}"), "error");
                return;
            }
        };
        let to_restore: Vec<trash::TrashItem> = items
            .into_iter()
            .filter(|item| {
                let orig = item.original_path().to_string_lossy().into_owned();
                target_originals.iter().any(|t| t == &orig)
            })
            .collect();
        let n = to_restore.len();
        if n == 0 {
            self.show_toast_kind(ui, "Items not found in trash.", "error");
            return;
        }
        match trash::os_limited::restore_all(to_restore) {
            Ok(()) => {
                self.show_toast_kind(ui, format!("Restored {n} item(s)"), "success");
                self.open_recycle_bin_view(ui, false);
            }
            Err(e) => self.show_toast_kind(ui, format!("Restore failed: {e}"), "error"),
        }
    }

    /// Permanently delete the currently selected recycle-bin items.
    fn purge_from_recycle_bin(&mut self, ui: &MainWindow) {
        let virtual_paths = self.selected_paths();
        let target_originals: Vec<String> = virtual_paths
            .iter()
            .filter_map(|p| p.strip_prefix("recycle://").map(|s| s.to_string()))
            .collect();
        if target_originals.is_empty() {
            return;
        }
        let items = match trash::os_limited::list() {
            Ok(it) => it,
            Err(e) => {
                self.show_toast_kind(ui, format!("Cannot read trash: {e}"), "error");
                return;
            }
        };
        let to_purge: Vec<trash::TrashItem> = items
            .into_iter()
            .filter(|item| {
                let orig = item.original_path().to_string_lossy().into_owned();
                target_originals.iter().any(|t| t == &orig)
            })
            .collect();
        let n = to_purge.len();
        if n == 0 {
            return;
        }
        match trash::os_limited::purge_all(to_purge) {
            Ok(()) => {
                self.show_toast_kind(ui, format!("Permanently deleted {n} item(s)"), "success");
                self.open_recycle_bin_view(ui, false);
            }
            Err(e) => self.show_toast_kind(ui, format!("Purge failed: {e}"), "error"),
        }
    }

    /// Empty the entire OS recycle bin (all users see the same trash on Windows).
    fn empty_recycle_bin(&mut self, ui: &MainWindow) {
        let items = match trash::os_limited::list() {
            Ok(it) => it,
            Err(e) => {
                self.show_toast_kind(ui, format!("Cannot read trash: {e}"), "error");
                return;
            }
        };
        let n = items.len();
        if n == 0 {
            self.show_toast(ui, "Recycle Bin is already empty.");
            return;
        }
        match trash::os_limited::purge_all(items) {
            Ok(()) => {
                self.show_toast_kind(ui, format!("Emptied recycle bin ({n} items)"), "success");
                self.open_recycle_bin_view(ui, false);
            }
            Err(e) => self.show_toast_kind(ui, format!("Empty failed: {e}"), "error"),
        }
    }

    fn schedule_full_directory_load(&mut self, path: String) {
        let state = self.app_state.clone();
        let ready = self.directory_ready.clone();
        let pending = self.pending_directory_result.clone();
        let generation = self.nav_generation.load(Ordering::SeqCst);
        std::thread::spawn(move || {
            // Stream the load: read DirEntries first (fast), then fetch metadata
            // in parallel chunks of 2000, publishing each chunk to the UI so very
            // large folders fill in progressively rather than freezing on a final
            // single replace. Each chunk is sorted and the running total is
            // resorted before publishing - keeps the displayed order stable.
            let dir = Path::new(&path);
            let dir_entries: Vec<fs::DirEntry> = match fs::read_dir(dir) {
                Ok(rd) => rd.filter_map(Result::ok).collect(),
                Err(_) => return,
            };
            let total = dir_entries.len();
            let chunk_size = 2000usize;
            let mut accumulated: Vec<FileEntry> = Vec::with_capacity(total);
            for chunk in dir_entries.chunks(chunk_size) {
                let entries: Vec<FileEntry> = chunk
                    .par_iter()
                    .filter_map(|entry| {
                        let path = entry.path();
                        entry.metadata().ok().map(|m| path_to_entry(&path, &m))
                    })
                    .collect();
                accumulated = merge_sorted_file_entries(accumulated, entries);
                if let Ok(mut lock) = pending.lock() {
                    *lock = Some(NativeDirectoryResult {
                        path: path.clone(),
                        entries: accumulated.clone(),
                        generation,
                        partial: true,
                        skipped_entries: 0,
                        error: None,
                    });
                }
                ready.store(true, Ordering::Release);
            }
            // Final cache + index population once everything is in.
            state.store_directory(&path, accumulated.clone());
            let _ = index_directory_entries(&path, &accumulated);
        });
    }

    fn refresh(&mut self, ui: &MainWindow) {
        self.sync_active_pane(ui);
        if self.active_pane == ActivePane::Secondary {
            let path = self.secondary_path.clone();
            self.secondary_navigate(ui, path);
            return;
        }
        if self.current_path == "storage://" && ui.get_is_storage_view() {
            self.rescan_storage(ui);
            return;
        }
        if self.current_path == "recycle://" {
            self.open_recycle_bin_view(ui, false);
            return;
        }
        if let Some(archive) = self.active_archive.clone() {
            self.open_archive_view(ui, archive.archive_path, archive.prefix, false);
            return;
        }
        self.app_state
            .invalidate_directory_path(Path::new(&self.current_path));
        let path = self.current_path.clone();
        self.navigate(ui, path, false);
    }

    /// Compute which file indices fall inside the marquee rectangle and update
    /// the selection. Coordinates are pane-local logical pixels from Slint.
    /// `commit_preview` is true on pointer-up so we avoid reloading the preview
    /// pane on every mouse-move during the drag.
    fn marquee_select(
        &mut self,
        ui: &MainWindow,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        commit_preview: bool,
    ) {
        if w < 2.0 && h < 2.0 {
            return;
        }
        let metrics = ui.global::<AppMetrics>();
        let pad = metrics.get_pad();
        let row_h = metrics.get_row_h();
        let grid_w = metrics.get_grid_w();
        let grid_h = metrics.get_grid_h();

        let mx1 = x.min(x + w);
        let my1 = y.min(y + h);
        let mx2 = x.max(x + w);
        let my2 = y.max(y + h);

        let view = ui.get_view_mode();
        let new_set = if view.as_str() == "list" {
            let list_top = pad + 32.0;
            marquee_selection_list(
                &self.visible_files,
                &self.sort_by,
                mx1,
                my1,
                mx2,
                my2,
                list_top,
                ui.get_primary_list_scroll_y(),
                row_h,
            )
        } else {
            let file_area_w = (ui.get_primary_pane_w() - pad * 2.0 - 14.0).max(1.0);
            let compact = view.as_str() == "compact";
            let cell_w_target = if compact { 200.0 } else { grid_w };
            let cols = (file_area_w / cell_w_target).floor().max(1.0) as usize;
            let grid_cell_w = file_area_w / cols as f32;
            let grid_item_h = match view.as_str() {
                "gallery" => 154.0_f32,
                "compact" => 32.0_f32,
                _ => grid_h,
            };
            let grid_gap = if compact { 2.0 } else { 8.0 };
            marquee_selection_grid(
                self.visible_files.len(),
                cols,
                mx1,
                my1,
                mx2,
                my2,
                pad,
                ui.get_primary_grid_scroll_y(),
                grid_cell_w,
                grid_item_h,
                grid_gap,
            )
        };

        if new_set == self.selected_set {
            return;
        }
        let old = self.selected_set.clone();
        self.selected_set = new_set;
        self.selected_index = self
            .selected_set
            .iter()
            .min()
            .copied()
            .map(|i| i as i32)
            .unwrap_or(-1);
        self.active_pane = ActivePane::Primary;
        let changed: Vec<usize> = old
            .symmetric_difference(&self.selected_set)
            .copied()
            .collect();
        if changed.is_empty() {
            return;
        }
        self.update_selection_in_model(ui, &changed);
        if commit_preview {
            self.update_preview(ui);
        }
    }

    fn sync_active_pane(&self, ui: &MainWindow) {
        let s = if self.active_pane == ActivePane::Secondary {
            "secondary"
        } else {
            "primary"
        };
        ui.set_active_pane(SharedString::from(s));
        // Drive the recycle context menus from whichever pane is active so
        // right-clicking a file in the non-recycle pane shows the regular
        // menu even if the other pane is currently showing the trash.
        let active_path = if self.active_pane == ActivePane::Secondary {
            &self.secondary_path
        } else {
            &self.current_path
        };
        ui.set_in_recycle_bin(active_path == "recycle://");
    }

    fn select(&mut self, ui: &MainWindow, index: i32) {
        self.select_with_modifiers(ui, index, false, false);
    }

    fn select_with_modifiers(&mut self, ui: &MainWindow, index: i32, ctrl: bool, shift: bool) {
        if ui.get_rename_index() >= 0 && index >= 0 && index != ui.get_rename_index() {
            self.cancel_inline_rename(ui);
        }
        self.active_pane = ActivePane::Primary;
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
        if self.active_pane == ActivePane::Secondary {
            let n = self.secondary_visible_files.len();
            if n == 0 {
                return;
            }
            self.secondary_selected_set = (0..n).collect();
            self.secondary_selected_index = 0;
            self.secondary_select_anchor = 0;
            let changed: Vec<usize> = (0..n).collect();
            self.update_secondary_selection_in_model(ui, &changed);
            self.update_status(ui);
            return;
        }

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
        self.active_pane = ActivePane::Primary;
        if index < 0 {
            return;
        }
        let Some(entry) = self.visible_files.get(index as usize).cloned() else {
            return;
        };
        if let Some(archive) = self.active_archive.clone() {
            if entry.kind == FileKind::Directory {
                if let Some((_, prefix)) = parse_archive_virtual_path(&entry.path) {
                    self.open_archive_view(ui, archive.archive_path, prefix, true);
                }
            } else {
                ui.set_preview_title(ss(&entry.name));
                ui.set_preview_body(ss(format!(
                    "{}\n\nUse Extract Here to unpack files from this archive.",
                    archive_display_path(&archive.archive_path, &archive.prefix)
                )));
                ui.set_preview_meta(ss(format!(
                    "Archive item | {} | {}",
                    entry_type(&entry),
                    format_size_short(entry.size)
                )));
            }
            return;
        }
        if let Some(name) = entry.path.strip_prefix("search:") {
            let saved = read_native_json::<Vec<SavedSearch>>("searches.json", Vec::new());
            if let Some(search) = saved.into_iter().find(|s| s.name == name) {
                if !search.scope.is_empty() {
                    self.search_query = search.query.clone();
                    ui.set_search_text(ss(&search.query));
                    self.navigate(ui, search.scope, true);
                    self.search(ui, self.search_query.clone());
                } else {
                    self.search_query = search.query.clone();
                    ui.set_search_text(ss(&search.query));
                    self.search(ui, self.search_query.clone());
                }
            }
            return;
        }
        if entry.path == "storage://" || entry.path == "recycle://" || entry.path == "home://" {
            self.navigate(ui, entry.path, true);
            return;
        }
        if entry.kind == FileKind::Directory {
            self.navigate(ui, entry.path, true);
        } else if is_archive_ext(entry.extension.as_deref().unwrap_or("")) {
            self.open_archive_view(ui, entry.path, String::new(), true);
        } else if let Err(error) = open_file(entry.path) {
            self.show_toast(ui, error);
        }
    }

    fn open_selected(&mut self, ui: &MainWindow) {
        let entries = self.selected_entries();
        if entries.is_empty() {
            self.show_toast(ui, "Select a file first.");
            return;
        }
        if entries.len() == 1 {
            if self.active_pane == ActivePane::Secondary {
                self.secondary_file_opened(ui, self.secondary_selected_index);
            } else {
                self.open_index(ui, self.selected_index);
            }
            return;
        }

        let mut opened_files = 0usize;
        let mut opened_dirs = 0usize;
        let mut first_error: Option<String> = None;
        for entry in entries {
            if let Some(archive) = self.active_archive.clone() {
                if entry.kind == FileKind::Directory {
                    if let Some((_, prefix)) = parse_archive_virtual_path(&entry.path) {
                        self.open_archive_view(ui, archive.archive_path, prefix, true);
                        opened_dirs += 1;
                    }
                }
                continue;
            }
            if entry.kind == FileKind::Directory {
                if opened_dirs == 0 {
                    if self.active_pane == ActivePane::Secondary {
                        self.secondary_navigate(ui, entry.path);
                    } else {
                        self.navigate(ui, entry.path, true);
                    }
                    opened_dirs += 1;
                }
                continue;
            }
            if is_archive_ext(entry.extension.as_deref().unwrap_or("")) {
                self.open_archive_view(ui, entry.path, String::new(), true);
                opened_files += 1;
                continue;
            }
            match open_file(entry.path) {
                Ok(()) => opened_files += 1,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if let Some(error) = first_error {
            self.show_toast_kind(ui, error, "error");
        } else if opened_files + opened_dirs > 0 {
            self.show_toast_kind(
                ui,
                format!(
                    "Opened {} item{}",
                    opened_files + opened_dirs,
                    if opened_files + opened_dirs == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                "success",
            );
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
            ui.set_preview_is_html(false);
            ui.set_preview_is_media(false);
            ui.set_preview_is_pdf(false);
            ui.set_preview_can_render(false);
            ui.set_preview_show_rendered(false);
            ui.set_preview_rendered(ss(""));
            ui.set_preview_has_pdf_page(false);
            ui.set_quick_look_path(ss(""));
            return;
        };

        ui.set_preview_title(ss(&entry.name));
        let ext_for_html = entry
            .extension
            .as_deref()
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        ui.set_preview_is_html(matches!(
            ext_for_html.as_str(),
            "html" | "htm" | "md" | "markdown" | "svg"
        ));
        // Only real markup gets the in-app Code/Rendered toggle (SVG stays
        // source-only). Reset the toggle to source on every selection change.
        ui.set_preview_can_render(matches!(
            ext_for_html.as_str(),
            "html" | "htm" | "md" | "markdown"
        ));
        ui.set_preview_show_rendered(false);
        ui.set_preview_rendered(ss(""));

        if let Some(archive) = &self.active_archive {
            ui.set_preview_is_image(false);
            ui.set_preview_body(ss(if entry.kind == FileKind::Directory {
                "Archive folder".to_string()
            } else {
                "Archive file. Use Extract Here to unpack it.".to_string()
            }));
            ui.set_preview_meta(ss(format!(
                "Archive: {}\nInside: {}\nSize: {}\nType: {}",
                archive.archive_path,
                parse_archive_virtual_path(&entry.path)
                    .map(|(_, prefix)| prefix)
                    .unwrap_or_default(),
                format_size_short(entry.size),
                entry_type(&entry)
            )));
            return;
        }

        // Try to load image preview from thumbnail cache
        let ext = entry.extension.as_deref().unwrap_or("").to_lowercase();
        ui.set_preview_is_pdf(ext == "pdf");
        ui.set_preview_is_media(is_media_ext(&ext));
        ui.set_quick_look_path(ss(&entry.path));

        // PDF: show a previously-rasterized first page instantly (no flicker)
        // if it's cached; otherwise clear it until the async render lands.
        if ext == "pdf" {
            if let Some(rgba) =
                pdf_page_cached_only(&self.app_state, &entry.path, entry.modified, PDF_PREVIEW_WIDTH)
            {
                ui.set_preview_pdf_page(rgba_to_slint_image(&rgba));
                ui.set_preview_has_pdf_page(true);
            } else {
                ui.set_preview_has_pdf_page(false);
            }
        } else {
            ui.set_preview_has_pdf_page(false);
        }
        let is_image = is_thumbnail_image_ext(&ext);
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

        let type_label = entry_type(&entry);
        let base_meta = format!(
            "Path:     {}\nType:     {}\nSize:     {}\nModified: {}",
            entry.path,
            type_label,
            format_size_short(entry.size),
            format_modified(entry.modified),
        );

        // A thumbnail already renders above, so skip the body read entirely.
        // This avoids a full-file base64 encode (and the freeze it caused) when
        // selecting a large image.
        if ui.get_preview_is_image() {
            ui.set_preview_body(ss(""));
            ui.set_preview_meta(ss(base_meta));
            return;
        }

        // Fast path: already cached, so format it inline with no flicker. A
        // cache hit means `build_preview_display` does no disk read beyond the
        // stat we just did, so it stays snappy. PDFs always take the async path
        // so the (slow) page rasterization never runs on the UI thread.
        if ext != "pdf" && preview_is_cached(&self.app_state, &entry.path) {
            let (body, meta, rendered) = build_preview_display(
                &self.app_state,
                &entry.path,
                &ext,
                &type_label,
                entry.size,
                entry.modified,
            );
            ui.set_preview_body(ss(body));
            ui.set_preview_meta(ss(meta));
            ui.set_preview_rendered(ss(rendered));
            return;
        }

        // Cold path: show meta immediately and read the body on a background
        // thread. The poll tick applies the result, dropping it if the
        // selection changed in the meantime. This is what keeps rapid clicks
        // and slow files (PDF, archives) from ever freezing the UI thread.
        ui.set_preview_body(ss(""));
        ui.set_preview_meta(ss(base_meta));

        let generation = self.preview_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let path = entry.path.clone();
        let ext_owned = ext.clone();
        let app_state = self.app_state.clone();
        let ready = self.preview_ready.clone();
        let pending = self.pending_preview_result.clone();
        let gen_check = self.preview_generation.clone();
        let size = entry.size;
        let modified = entry.modified;
        std::thread::spawn(move || {
            let (body, meta, rendered) =
                build_preview_display(&app_state, &path, &ext_owned, &type_label, size, modified);
            // Rasterize the PDF first page (cached) so the pane shows the real
            // document, not just extracted text.
            let pdf_image = if ext_owned == "pdf" {
                pdf_page_rgba_cached(&app_state, &path, modified, PDF_PREVIEW_WIDTH)
            } else {
                None
            };
            if gen_check.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(PreviewResult {
                    generation,
                    body,
                    meta,
                    rendered,
                    pdf_image,
                });
            }
            ready.store(true, Ordering::Release);
        });
    }

    fn go_up(&mut self, ui: &MainWindow) {
        if let Some(archive) = self.active_archive.clone() {
            if archive.prefix.is_empty() {
                self.navigate(ui, archive.return_path, true);
            } else {
                self.open_archive_view(
                    ui,
                    archive.archive_path,
                    archive_parent_prefix(&archive.prefix),
                    true,
                );
            }
            return;
        }
        if self.current_path == "storage://" {
            if !self.storage_selected_bucket.is_empty() || self.storage_show_all_state {
                self.clear_storage_bucket_filter(ui);
                return;
            }
            let target = if !self.storage_path_before.is_empty() {
                self.storage_path_before.clone()
            } else {
                dirs::home_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".to_string())
            };
            self.navigate(ui, target, true);
            return;
        }
        if self.current_path == "recycle://" {
            let target = if self.history_index > 0 {
                self.history
                    .get(self.history_index.saturating_sub(1))
                    .cloned()
                    .unwrap_or_else(|| {
                        dirs::home_dir()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "/".to_string())
                    })
            } else {
                dirs::home_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".to_string())
            };
            self.navigate(ui, target, true);
            return;
        }
        if let Some(parent) = Path::new(&self.current_path).parent() {
            self.navigate(ui, parent.to_string_lossy().to_string(), true);
        }
    }

    fn go_back(&mut self, ui: &MainWindow) {
        // Storage drill-in: Back / mouse back returns to the bucket overview
        // before leaving the storage view entirely.
        if self.current_path == "storage://"
            && (!self.storage_selected_bucket.is_empty() || self.storage_show_all_state)
        {
            self.clear_storage_bucket_filter(ui);
            return;
        }
        // Leave storage view → folder the user had open before Storage.
        if self.current_path == "storage://" {
            let target = if !self.storage_path_before.is_empty() {
                self.storage_path_before.clone()
            } else {
                dirs::home_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".to_string())
            };
            self.navigate(ui, target, false);
            return;
        }
        if self.history_index > 0 {
            self.history_index -= 1;
            if let Some(path) = self.history.get(self.history_index).cloned() {
                self.navigate(ui, path, false);
            }
            return;
        }
        // No history to walk (typical when the app was launched with --path or
        // from a "Show in folder" shell verb). Fall back to navigating to the
        // parent folder so Back is never a dead button.
        if let Some(parent) = Path::new(&self.current_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            && !parent.is_empty()
            && parent != self.current_path
        {
            self.navigate(ui, parent, true);
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
        write_native_json_async("folder_views.json", &self.folder_views);
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

    fn toggle_quick_look(&mut self, ui: &MainWindow) {
        let visible = !ui.get_quick_look_visible();
        if visible {
            if self.selected_entry().is_none() {
                self.show_toast(ui, "Select a file first.");
                return;
            }
            if !ui.get_preview_visible() {
                ui.set_preview_visible(true);
            }
            self.update_preview(ui);
            ui.set_quick_look_visible(true);
        } else {
            ui.set_quick_look_visible(false);
        }
    }

    /// Path search: expand env vars, navigate to folders, or match paths in
    /// the index / current listing (not file names).
    fn search_by_path(&mut self, ui: &MainWindow, query: &str, navigate_if_dir: bool) -> bool {
        let resolved = resolve_path_query(query, &self.current_path);
        let resolved_str = resolved.to_string_lossy().into_owned();

        if navigate_if_dir && resolved.is_dir() {
            self.search_query.clear();
            ui.set_search_text(ss(""));
            self.navigate(ui, resolved_str, true);
            return true;
        }

        if navigate_if_dir && resolved.is_file() {
            let file_path = resolved_str.clone();
            if let Some(parent) = resolved.parent() {
                let parent_str = parent.to_string_lossy().into_owned();
                self.search_query.clear();
                ui.set_search_text(ss(""));
                self.navigate(ui, parent_str, true);
                if let Some(idx) = self
                    .visible_files
                    .iter()
                    .position(|e| same_path_string(&e.path, &file_path))
                {
                    self.selected_index = idx as i32;
                    self.selected_set.clear();
                    self.selected_set.insert(idx);
                    self.update_models(ui);
                }
            }
            return true;
        }

        if navigate_if_dir {
            if let Some(existing) = deepest_existing_directory(&resolved) {
                if !same_path_string(&existing.to_string_lossy(), &self.current_path) {
                    self.navigate(
                        ui,
                        existing.to_string_lossy().into_owned(),
                        true,
                    );
                    self.search_query = query.to_string();
                    ui.set_search_text(ss(query));
                }
            }
        }

        let pattern = expand_path_query(query);
        let pattern_lower = pattern.trim_end_matches(['\\', '/']).to_ascii_lowercase();

        let mut matches: Vec<FileEntry> = self
            .files
            .iter()
            .filter(|e| {
                let p = e.path.to_ascii_lowercase();
                p.contains(&pattern_lower)
                    || pattern_lower
                        .split('\\')
                        .filter(|s| !s.is_empty())
                        .all(|seg| p.contains(seg))
            })
            .cloned()
            .collect();

        if matches.is_empty() {
            if let Ok(indexed) =
                index_search_paths(&self.search_root(), query, SEARCH_INDEX_LIMIT)
            {
                matches = indexed;
            }
        }

        if matches.is_empty() {
            return false;
        }

        sort_entries(&mut matches);
        self.visible_files = matches;
        self.apply_sort();
        ui.set_empty_state(ss(""));
        self.update_models(ui);
        self.update_status(ui);
        ui.set_status_right(ss(format!(
            "Path matches in {}",
            compact_drive_label(&self.search_root())
        )));
        true
    }

    fn search(&mut self, ui: &MainWindow, query: String) {
        self.search_query = query;
        self.selected_index = -1;
        self.selected_set.clear();
        self.select_anchor = -1;
        self.files_model = None;
        let trimmed = self.search_query.trim().to_string();
        let trimmed_lower = trimmed.to_ascii_lowercase();
        let search_root = self.search_root();
        let drive_wide =
            self.search_all_scope || is_filesystem_root(std::path::Path::new(&search_root));

        if trimmed.is_empty() {
            self.apply_filter();
            ui.set_empty_state(ss(""));
            self.update_models(ui);
            return;
        }

        if trimmed_lower.starts_with("tag:") || trimmed_lower.starts_with("smart:") {
            self.apply_filter();
            ui.set_empty_state(ss(""));
            self.update_models(ui);
            return;
        }

        if looks_like_path_query(&trimmed) {
            let resolved = resolve_path_query(&trimmed, &self.current_path);
            let ends_with_sep = trimmed.ends_with('\\') || trimmed.ends_with('/');
            // Trailing slash means "open this folder" while typing.
            if resolved.is_dir() && ends_with_sep {
                self.search_by_path(ui, &trimmed, true);
                return;
            }
            if self.search_by_path(ui, &trimmed, false) {
                return;
            }
            if resolved.is_dir() {
                ui.set_empty_state(ss(format!(
                    "Press Enter to open {}",
                    expand_path_query(&trimmed)
                )));
                self.visible_files.clear();
                self.update_models(ui);
                return;
            }
            ui.set_empty_state(ss(format!(
                "No path matches for \"{}\"",
                expand_path_query(&trimmed)
            )));
            self.visible_files.clear();
            self.update_models(ui);
            return;
        }

        if trimmed.len() < 2 && !self.search_all_scope && !drive_wide {
            self.visible_files.clear();
            ui.set_empty_state(ss(""));
            self.update_models(ui);
            return;
        }

        // Index / Windows Search / live scan run on a worker thread so typing
        // in large folders or whole-drive queries never blocks the UI thread.
        self.visible_files.clear();
        ui.set_empty_state(ss(if drive_wide {
            "Searching drive..."
        } else {
            "Searching..."
        }));
        self.update_models(ui);
        self.schedule_background_search(ui, trimmed);
    }

    fn schedule_background_search(&mut self, ui: &MainWindow, query: String) {
        let path = self.search_root();
        let token = self
            .app_state
            .search_generation
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        let state = self.app_state.clone();
        let ready = self.search_ready.clone();
        let pending = self.pending_search_result.clone();
        ui.set_status_right(ss(if self.search_all_scope {
            format!("{path} | searching drive...")
        } else {
            format!("{path} | searching...")
        }));
        let semantic = self.settings.search_semantic_mode && local_ai_semantic_ready();
        let clip = self.settings.clip_search_enabled && local_ai_image_search_ready();
        // Searching from a drive root (or with the search-all-scope toggle on)
        // uses a much larger live-scan ceiling so the first search pulls from
        // the whole drive instead of waiting for a second query after indexing.
        let path_for_limit = path.clone();
        let is_drive_root = is_filesystem_root(std::path::Path::new(&path_for_limit));
        let limit = if self.search_all_scope || is_drive_root {
            SEARCH_DRIVE_SCAN_LIMIT
        } else {
            SEARCH_LIVE_SCAN_LIMIT
        };
        let prefer_live_first = self.search_all_scope || is_drive_root;
        std::thread::spawn(move || {
            let mut results = Vec::new();
            let mut source = String::new();

            let emit = |entries: &[FileEntry], source: &str, partial: bool| {
                if entries.is_empty() && partial {
                    return;
                }
                let mut snapshot = entries.to_vec();
                sort_entries(&mut snapshot);
                publish_search_result(
                    &pending,
                    &ready,
                    &path,
                    &query,
                    snapshot,
                    source.to_string(),
                    partial,
                );
            };

            if prefer_live_first {
                let live = live_search_scan(&state, &path, &query, limit, token);
                if state.search_generation.load(Ordering::SeqCst) != token {
                    return;
                }
                if !live.is_empty() {
                    let _ = upsert_index_entries(&live);
                    merge_search_entries(&mut results, live, limit);
                    source = "live scan".to_string();
                    emit(&results, &source, true);
                }
            }

            if results.len() < limit {
                if let Ok(indexed) =
                    index_search(&path, &query, limit.saturating_sub(results.len()))
                {
                    if !indexed.is_empty() {
                        merge_search_entries(&mut results, indexed, limit);
                        source = if source.is_empty() {
                            "Pathfinder index".to_string()
                        } else {
                            format!("{source} + Pathfinder index")
                        };
                        emit(&results, &source, true);
                    }
                }
            }

            if state.search_generation.load(Ordering::SeqCst) != token {
                return;
            }

            if results.is_empty() {
                if let Ok(windows_results) = windows_index_search_impl(&query, &path, limit) {
                    if !windows_results.is_empty() {
                        let _ = upsert_index_entries(&windows_results);
                        merge_search_entries(&mut results, windows_results, limit);
                        source = "Windows Search".to_string();
                        emit(&results, &source, true);
                    }
                }
            } else if let Ok(windows_results) = windows_index_search_impl(&query, &path, limit) {
                if !windows_results.is_empty() {
                    let _ = upsert_index_entries(&windows_results);
                    merge_search_entries(&mut results, windows_results, limit);
                    source = if source.is_empty() {
                        "Windows Search".to_string()
                    } else {
                        format!("{source} + Windows Search")
                    };
                    emit(&results, &source, true);
                }
            }

            if state.search_generation.load(Ordering::SeqCst) != token {
                return;
            }

            if results.len() < limit && !prefer_live_first {
                let live = live_search_scan(
                    &state,
                    &path,
                    &query,
                    limit.saturating_sub(results.len()),
                    token,
                );
                if state.search_generation.load(Ordering::SeqCst) != token {
                    return;
                }
                if !live.is_empty() {
                    let _ = upsert_index_entries(&live);
                    merge_search_entries(&mut results, live, limit);
                    source = if source.contains("Windows") {
                        "Pathfinder index + Windows Search + live scan".to_string()
                    } else if source.is_empty() {
                        "live scan".to_string()
                    } else {
                        format!("{source} + live scan")
                    };
                    emit(&results, &source, true);
                }
            }

            if state.search_generation.load(Ordering::SeqCst) != token {
                return;
            }
            sort_entries(&mut results);
            apply_semantic_search_ranking_entries(&path, &query, semantic, clip, &mut results);
            publish_search_result(&pending, &ready, &path, &query, results, source, false);
        });
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
        // Skip the work if the user clicks the tab they are already on. Without
        // this guard, rapid double-clicks (or a click on the active tab) trigger
        // a full re-navigate and re-list of the same folder, which is the cause
        // of the visible stutter when switching back and forth between two tabs.
        if idx == self.active_tab {
            return;
        }
        if let Some(tab) = self.tabs.get(idx).cloned() {
            // Only re-navigate when the target tab points at a different folder
            // than the one we are already viewing. Same-folder tab switches just
            // update active_tab + view_mode and skip the directory re-list.
            let same_path = tab.path == self.current_path;
            self.active_tab = idx;
            ui.set_view_mode(ss(&tab.view));
            if !same_path {
                self.navigate(ui, tab.path, false);
            } else {
                let tabs = self.tab_items();
                #[cfg(target_os = "windows")]
                sync_titlebar_hit_regions(&tabs);
                ui.set_tabs(model_from_vec(tabs));
            }
        }
    }

    fn sort_column(&mut self, ui: &MainWindow, col: &str) {
        if col == "reset" {
            self.sort_by = "name".to_string();
            self.sort_dir = "asc".to_string();
        } else if self.sort_by == col {
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
        // While a search is active, deep-search results live only in
        // visible_files (not in self.files), so calling apply_filter() would
        // wipe them and replace with the current folder's local-name matches.
        // Just re-sort the existing visible_files instead.
        if self.search_query.trim().is_empty() {
            self.apply_filter();
        } else {
            self.apply_sort();
        }
        self.update_models(ui);
        self.apply_secondary_sort();
        self.update_secondary_models(ui);
    }

    fn set_selected_tag(&mut self, ui: &MainWindow, tag: &str) {
        let valid = matches!(
            tag,
            "red" | "orange" | "yellow" | "green" | "blue" | "violet" | "clear"
        );
        if !valid {
            self.show_toast(ui, "Unknown tag.");
            return;
        }

        let paths = self.selected_paths();
        if paths.is_empty() {
            self.show_toast(ui, "Select a file first.");
            return;
        }

        for path in &paths {
            if tag == "clear" {
                self.tags.remove(path);
            } else {
                self.tags.insert(path.clone(), tag.to_string());
            }
        }
        let _ = write_native_json("tags.json", &self.tags);
        self.apply_filter();
        self.update_models(ui);
        self.update_secondary_models(ui);

        if tag == "clear" {
            self.show_toast_kind(
                ui,
                format!("Cleared tags on {} item(s)", paths.len()),
                "success",
            );
        } else {
            let label = self.tag_effective_label(tag).to_string();
            self.show_toast_kind(
                ui,
                format!("Tagged {} item(s) as {}", paths.len(), label),
                "success",
            );
        }
    }

    fn tag_effective_label<'a>(&'a self, id: &'a str) -> &'a str {
        self.tag_labels
            .get(id)
            .map(|s| s.as_str())
            .unwrap_or_else(|| tag_label(id))
    }

    fn sync_tag_names(&self, ui: &MainWindow) {
        let names: Vec<slint::SharedString> =
            ["red", "orange", "yellow", "green", "blue", "violet"]
                .iter()
                .map(|id| ss(self.tag_effective_label(id)))
                .collect();
        ui.set_tag_names(std::rc::Rc::new(slint::VecModel::from(names)).into());
    }

    fn show_rename_tag_prompt(&mut self, ui: &MainWindow, tag_id: String) {
        let current = self.tag_effective_label(&tag_id).to_string();
        ui.set_prompt_title(ss("Rename Tag"));
        ui.set_prompt_value(ss(&current));
        self.pending_prompt = Some(PendingPrompt::RenameTag(tag_id));
        ui.set_prompt_visible(true);
    }

    fn clear_selection(&mut self, ui: &MainWindow) {
        if self.selected_index < 0
            && self.selected_set.is_empty()
            && self.secondary_selected_index < 0
            && self.secondary_selected_set.is_empty()
        {
            // Rust has no selection, but `selected_count` can stay stale after
            // `update_models` until now - resync so the action bar, Esc, and X
            // behave consistently.
            self.sync_selection_count_to_ui(ui);
            ui.set_selected_index(-1);
            ui.set_selected_name(ss(""));
            self.update_status(ui);
            return;
        }
        let changed: Vec<usize> = self
            .selected_set
            .iter()
            .copied()
            .chain(if self.selected_index >= 0 {
                Some(self.selected_index as usize)
            } else {
                None
            })
            .collect();
        let secondary_changed: Vec<usize> = self
            .secondary_selected_set
            .iter()
            .copied()
            .chain(if self.secondary_selected_index >= 0 {
                Some(self.secondary_selected_index as usize)
            } else {
                None
            })
            .collect();
        self.selected_index = -1;
        self.selected_set.clear();
        self.select_anchor = -1;
        self.secondary_selected_index = -1;
        self.secondary_selected_set.clear();
        self.secondary_select_anchor = -1;
        if self.clipboard.as_ref().is_some_and(|c| c.cut) {
            self.clipboard = None;
            #[cfg(target_os = "windows")]
            let _ = file_drag::clear_shell_files_clipboard();
        }
        self.update_selection_in_model(ui, &changed);
        self.update_secondary_selection_in_model(ui, &secondary_changed);
        ui.set_selected_index(-1);
        // The contextual action bar hides itself when selected_count returns
        // to zero. Push the new count so the X clear button and any other
        // path that ends up here drop the bar immediately.
        self.sync_selection_count_to_ui(ui);
        self.refresh_clipboard_visuals(ui);
        self.update_status(ui);
    }

    fn update_secondary_models(&mut self, ui: &MainWindow) {
        let items: Vec<FileItem> = self
            .secondary_visible_files
            .iter()
            .enumerate()
            .map(|(i, entry)| self.file_item(entry, self.secondary_selected_set.contains(&i)))
            .collect();
        let model = model_from_vec(items);
        ui.set_secondary_files(model.clone());
        ui.set_secondary_path(ss(&self.secondary_path));
        self.secondary_files_model = Some(model);
    }

    fn update_secondary_selection_in_model(&mut self, ui: &MainWindow, changed: &[usize]) {
        if let Some(model) = &self.secondary_files_model {
            use slint::Model;
            for &i in changed {
                if let Some(entry) = self.secondary_visible_files.get(i) {
                    let item = self.file_item(entry, self.secondary_selected_set.contains(&i));
                    if let Some(m) = model.as_any().downcast_ref::<VecModel<FileItem>>() {
                        m.set_row_data(i, item);
                    }
                }
            }
        }
        ui.set_selected_index(self.secondary_selected_index);
        self.sync_selection_count_to_ui(ui);
    }

    fn secondary_navigate(&mut self, ui: &MainWindow, path: String) {
        self.secondary_navigate_impl(ui, path, true);
    }

    fn secondary_navigate_impl(&mut self, ui: &MainWindow, path: String, push_history: bool) {
        if path.is_empty() || !Path::new(&path).is_dir() {
            return;
        }
        if push_history {
            self.secondary_history
                .truncate(self.secondary_history_pos + 1);
            if self.secondary_history.last().map(|p| p.as_str()) != Some(&path) {
                self.secondary_history.push(path.clone());
                self.secondary_history_pos = self.secondary_history.len() - 1;
            }
        }
        self.active_pane = ActivePane::Secondary;
        self.secondary_path = path.clone();
        self.secondary_selected_index = -1;
        self.secondary_selected_set.clear();
        self.secondary_select_anchor = -1;
        match fs::read_dir(&path) {
            Ok(rd) => {
                let entries: Vec<FileEntry> = rd
                    .filter_map(Result::ok)
                    .filter_map(|e| {
                        let p = e.path();
                        fs::metadata(&p).ok().map(|m| path_to_entry(&p, &m))
                    })
                    .collect();
                self.secondary_files = entries.clone();
                self.secondary_visible_files = entries;
                self.apply_secondary_sort();
            }
            Err(_) => {
                self.secondary_files = Vec::new();
                self.secondary_visible_files = Vec::new();
            }
        }
        self.update_secondary_models(ui);
        self.sync_selection_count_to_ui(ui);
    }

    fn secondary_go_back(&mut self, ui: &MainWindow) {
        if self.secondary_history_pos > 0 {
            self.secondary_history_pos -= 1;
            let path = self.secondary_history[self.secondary_history_pos].clone();
            self.secondary_navigate_impl(ui, path, false);
        }
    }

    fn sort_secondary_column(&mut self, ui: &MainWindow, col: &str) {
        if self.secondary_sort_by == col {
            self.secondary_sort_dir = if self.secondary_sort_dir == "asc" {
                "desc".to_string()
            } else {
                "asc".to_string()
            };
        } else {
            self.secondary_sort_by = col.to_string();
            self.secondary_sort_dir = "asc".to_string();
        }
        ui.set_secondary_sort_by(ss(&self.secondary_sort_by));
        ui.set_secondary_sort_dir(ss(&self.secondary_sort_dir));
        self.apply_secondary_sort();
        self.update_secondary_models(ui);
    }

    fn set_folder_filter(&mut self, ui: &MainWindow, text: String) {
        self.folder_filter = text;
        self.apply_filter();
        self.update_models(ui);
    }

    fn secondary_file_selected(&mut self, ui: &MainWindow, index: i32, ctrl: bool, shift: bool) {
        if ui.get_rename_index() >= 0 && index >= 0 && index != ui.get_rename_index() {
            self.cancel_inline_rename(ui);
        }
        self.active_pane = ActivePane::Secondary;
        let n = self.secondary_visible_files.len();
        let mut changed: Vec<usize> = Vec::new();

        if shift && self.secondary_select_anchor >= 0 && index >= 0 {
            let lo = (self.secondary_select_anchor as usize).min(index as usize);
            let hi = (self.secondary_select_anchor as usize).max(index as usize);
            let old = std::mem::take(&mut self.secondary_selected_set);
            for i in 0..n {
                if (i >= lo && i <= hi) || i == self.secondary_select_anchor as usize {
                    self.secondary_selected_set.insert(i);
                    if !old.contains(&i) {
                        changed.push(i);
                    }
                } else if old.contains(&i) {
                    changed.push(i);
                }
            }
        } else if ctrl && index >= 0 {
            let i = index as usize;
            if self.secondary_selected_set.contains(&i) {
                self.secondary_selected_set.remove(&i);
            } else if i < n {
                self.secondary_selected_set.insert(i);
                self.secondary_select_anchor = index;
            }
            changed.push(i);
        } else {
            let old = std::mem::take(&mut self.secondary_selected_set);
            for i in old {
                changed.push(i);
            }
            if index >= 0 && (index as usize) < n {
                self.secondary_selected_set.insert(index as usize);
                changed.push(index as usize);
                self.secondary_select_anchor = index;
            }
        }

        self.secondary_selected_index = index;
        self.update_secondary_selection_in_model(ui, &changed);
    }

    fn secondary_file_opened(&mut self, ui: &MainWindow, index: i32) {
        self.active_pane = ActivePane::Secondary;
        if let Some(entry) = self.secondary_visible_files.get(index as usize).cloned() {
            if entry.kind == FileKind::Directory {
                self.secondary_navigate(ui, entry.path);
            } else if is_archive_ext(entry.extension.as_deref().unwrap_or("")) {
                self.open_archive_view(ui, entry.path, String::new(), true);
            } else {
                if let Err(error) = open_file(entry.path) {
                    self.show_toast(ui, error);
                }
            }
        }
    }

    fn secondary_go_up(&mut self, ui: &MainWindow) {
        self.active_pane = ActivePane::Secondary;
        let parent = Path::new(&self.secondary_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string());
        if let Some(parent_path) = parent {
            if !parent_path.is_empty() {
                self.secondary_navigate(ui, parent_path);
            }
        }
    }

    fn command(&mut self, ui: &MainWindow, command: &str) {
        if let Some(tag) = command.strip_prefix("tag-") {
            self.set_selected_tag(ui, tag);
            return;
        }
        if let Some(action) = command
            .strip_prefix("image-")
            .and_then(ImageToolAction::from_command)
        {
            self.run_image_tool(ui, action);
            return;
        }

        match command {
            "new-tab" => self.new_tab(ui),
            "close-tab" => self.close_tab(ui, -1),
            "refresh" => self.refresh(ui),
            "settings" => {
                ui.set_settings_visible(true);
                self.spawn_performance_status(ui);
            }
            "command-palette" => ui.set_command_visible(true),
            "view-grid" => self.set_view(ui, "grid"),
            "view-list" => self.set_view(ui, "list"),
            "view-gallery" => self.set_view(ui, "gallery"),
            "toggle-preview" => self.set_preview_visible(ui, !ui.get_preview_visible()),
            "toggle-dual" => {
                let was_dual = ui.get_dual_pane();
                ui.set_dual_pane(!was_dual);
                if !was_dual {
                    let path = self.default_secondary_path();
                    self.secondary_navigate(ui, path);
                } else {
                    self.active_pane = ActivePane::Primary;
                    self.sync_active_pane(ui);
                }
            }
            "open" => self.open_selected(ui),
            "rename" => self.prompt_rename(ui),
            "delete" if self.active_path_is_recycle_bin() => self.purge_from_recycle_bin(ui),
            "delete" => self.prompt_delete(ui),
            "new-folder" => self.prompt_new_folder(ui),
            "new-file" => self.prompt_new_file(ui),
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
                            self.active_directory().to_string()
                        }
                    })
                    .unwrap_or_else(|| self.active_directory().to_string());
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
                self.app_state.clear_queue_cancel();
                self.show_toast(ui, "Queue resumed.");
            }
            "queue-cancel" => {
                self.app_state.request_queue_cancel();
                self.app_state
                    .search_generation
                    .fetch_add(1, Ordering::SeqCst);
                if let Ok(mut queue) = self.app_state.operation_queue.lock() {
                    for item in queue.iter_mut().filter(|i| i.status == "running") {
                        item.status = "cancelled".to_string();
                        item.detail = "Cancelled by user".to_string();
                        item.finished_at = Some(now_unix_secs());
                    }
                }
                self.show_toast(ui, "Cancelling running operations...");
            }
            "locked-file" => self.show_locked_file(ui),
            "properties" => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.show_toast(ui, "Select a file first.");
                } else {
                    match open_windows_properties(&paths[0]) {
                        Ok(()) => {
                            let msg = if paths.len() == 1 {
                                "Opening Windows Properties".to_string()
                            } else {
                                format!(
                                    "Opening Properties for first of {} items",
                                    paths.len()
                                )
                            };
                            self.show_toast(ui, msg);
                        }
                        Err(error) => self.show_toast(ui, error),
                    }
                }
            }
            "show-more-options" => {
                if let Some(entry) = self.selected_entry() {
                    if let Err(e) = open_more_options(&entry.path, ui) {
                        self.show_toast(ui, e);
                    }
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "open-with" => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.show_toast(ui, "Select a file first.");
                } else if paths.len() > 1 {
                    self.show_toast(
                        ui,
                        "Open With applies to one file — select a single item.",
                    );
                } else {
                    match open_with_dialog(&paths[0], Some(ui)) {
                        Ok(()) => {}
                        Err(error) => self.show_toast_kind(ui, error, "error"),
                    }
                }
            }
            "defender-scan" => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.show_toast(ui, "Select a file first.");
                } else {
                    let mut started = 0usize;
                    let mut first_error: Option<String> = None;
                    for path in &paths {
                        match defender_scan_path(path) {
                            Ok(()) => started += 1,
                            Err(error) => {
                                if first_error.is_none() {
                                    first_error = Some(error);
                                }
                            }
                        }
                    }
                    if let Some(error) = first_error {
                        self.show_toast_kind(ui, error, "error");
                    } else {
                        self.show_toast_kind(
                            ui,
                            format!(
                                "Microsoft Defender scan started for {started} item{}",
                                if started == 1 { "" } else { "s" }
                            ),
                            "success",
                        );
                    }
                }
            }
            "shell-verbs" => {
                if let Some(entry) = self.selected_entry() {
                    ui.set_preview_title(ss("Shell Verb Bridge"));
                    ui.set_preview_body(ss(shell_verb_summary(&entry.path)));
                    ui.set_preview_meta(ss(
                        "Pathfinder keeps common verbs native and delegates special verbs to Windows.",
                    ));
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "run-as-admin" => {
                if let Some(entry) = self.selected_entry() {
                    match run_as_admin(&entry.path) {
                        Ok(()) => {}
                        Err(e) => self.show_toast(ui, e),
                    }
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "take-ownership" => {
                if let Some(entry) = self.selected_entry() {
                    #[cfg(target_os = "windows")]
                    match windows_integration::take_ownership(&entry.path) {
                        Ok(r) => self.show_toast(ui, &r.message),
                        Err(e) => self.show_toast(ui, e),
                    }
                    #[cfg(not(target_os = "windows"))]
                    self.show_toast(ui, "Take Ownership is only available on Windows.");
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "previous-versions" => {
                if let Some(entry) = self.selected_entry() {
                    let versions = list_previous_versions(&entry.path);
                    if versions.is_empty() {
                        ui.set_preview_title(ss("Previous Versions"));
                        ui.set_preview_body(ss("No shadow copies found for this drive.\n\n\
                             Enable File History, a restore point, or Volume Shadow \
                             Copy Service (VSS) snapshots to create previous versions."));
                    } else {
                        ui.set_preview_title(ss("Previous Versions"));
                        ui.set_preview_body(ss(versions.join("\n")));
                    }
                    ui.set_preview_meta(ss(&entry.path));
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "pin-to-taskbar" => {
                if let Some(entry) = self.selected_entry() {
                    #[cfg(target_os = "windows")]
                    match windows_integration::pin_to_taskbar(&entry.path) {
                        Ok(r) => self.show_toast(ui, &r.message),
                        Err(e) => self.show_toast(ui, e),
                    }
                    #[cfg(not(target_os = "windows"))]
                    self.show_toast(ui, "Pin to Taskbar is only available on Windows.");
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            }
            "pin-to-start" => {
                if let Some(entry) = self.selected_entry() {
                    #[cfg(target_os = "windows")]
                    match windows_integration::pin_to_start_menu(&entry.path) {
                        Ok(r) => self.show_toast(ui, &r.message),
                        Err(e) => self.show_toast(ui, e),
                    }
                    #[cfg(not(target_os = "windows"))]
                    self.show_toast(ui, "Pin to Start is only available on Windows.");
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
            "extract-here" => self.extract_selected_archive(ui),
            "create-zip" => self.create_archive_from_selection(ui, "zip"),
            "create-7z" => self.create_archive_from_selection(ui, "7z"),
            "create-tar-gz" => self.create_archive_from_selection(ui, "tar.gz"),
            "compare-folder" => self.prompt_compare_folder(ui),
            "rules" => self.show_rules(ui),
            "smart-folders" => self.show_smart_folders(ui),
            "home-page" => self.show_home_page(ui),
            "save-workspace" => self.show_workspaces_overlay(ui, true),
            "open-workspace" => self.show_workspaces_overlay(ui, false),
            "libraries" => self.show_libraries(ui),
            "recent-locations" => self.show_recent_locations(ui),
            "copy-as-path" => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.show_toast(ui, "Select a file first.");
                } else {
                    let text = paths.join("\r\n");
                    match copy_text_to_clipboard(&text) {
                        Ok(()) => self.show_toast_kind(
                            ui,
                            format!(
                                "Copied {} path{}",
                                paths.len(),
                                if paths.len() == 1 { "" } else { "s" }
                            ),
                            "success",
                        ),
                        Err(error) => self.show_toast(ui, error),
                    }
                }
            }
            "copy-as-powershell" => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.show_toast(ui, "Select a file first.");
                } else {
                    let text = paths
                        .iter()
                        .map(|path| format!("'{}'", path.replace('\'', "''")))
                        .collect::<Vec<_>>()
                        .join("\r\n");
                    match copy_text_to_clipboard(&text) {
                        Ok(()) => self.show_toast_kind(
                            ui,
                            format!(
                                "Copied {} PowerShell path{}",
                                paths.len(),
                                if paths.len() == 1 { "" } else { "s" }
                            ),
                            "success",
                        ),
                        Err(error) => self.show_toast(ui, error),
                    }
                }
            }
            "copy-as-uri" => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    self.show_toast(ui, "Select a file first.");
                } else {
                    let text = paths
                        .iter()
                        .map(|path| {
                            format!(
                                "file:///{}",
                                path.replace('\\', "/").replace(' ', "%20")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\r\n");
                    match copy_text_to_clipboard(&text) {
                        Ok(()) => self.show_toast_kind(
                            ui,
                            format!(
                                "Copied {} URI{}",
                                paths.len(),
                                if paths.len() == 1 { "" } else { "s" }
                            ),
                            "success",
                        ),
                        Err(error) => self.show_toast(ui, error),
                    }
                }
            }
            "breadcrumb-siblings" => self.show_breadcrumb_siblings(ui),
            "ai-suggest-tags" => {
                let paths = self.selected_paths();
                let mut n = 0usize;
                for p in paths {
                    let ext = Path::new(&p)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if matches!(
                        ext.as_str(),
                        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
                    ) {
                        if let Some(t) = crate::inference::suggest_image_tag(Path::new(&p)) {
                            self.tags.insert(p, t);
                            n += 1;
                        }
                    }
                }
                let _ = write_native_json("tags.json", &self.tags);
                self.show_toast(ui, format!("Updated AI-suggested tags on {n} image(s)."));
                self.sync_tag_names(ui);
                self.update_models(ui);
            }
            "find-image-duplicates" => {
                self.show_image_duplicates(ui);
            }
            "performance-debug" => self.show_performance_debug(ui),
            "clear-thumbnail-cache" => match clear_thumbnail_cache() {
                Ok(bytes) => {
                    self.sync_performance_status(ui);
                    self.show_toast(ui, format!("Cleared {}", format_size_short(bytes)));
                }
                Err(error) => self.show_toast(ui, error),
            },
            "clear-local-caches" => {
                if let Ok(mut cache) = self.app_state.directory_cache.lock() {
                    cache.clear();
                }
                if let Ok(mut cache) = self.app_state.preview_cache.lock() {
                    cache.clear();
                }
                if let Ok(mut cache) = self.app_state.git_cache.lock() {
                    cache.clear();
                }
                match clear_thumbnail_cache() {
                    Ok(bytes) => {
                        self.thumbnail_memory.clear();
                        self.sync_performance_status(ui);
                        self.show_toast(
                            ui,
                            format!("Cleared local caches ({})", format_size_short(bytes)),
                        );
                    }
                    Err(error) => self.show_toast(ui, error),
                }
            }
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
                self.spawn_performance_status(ui);
            }
            "privacy-storage" => self.show_privacy_storage(ui),
            "open-releases" => {
                let _ = open::that(GITHUB_RELEASES_URL);
            }
            "check-updates" => match check_github_release_now() {
                Ok(result) => {
                    ui.set_preview_title(ss("Updates"));
                    ui.set_preview_body(ss(format!(
                        "{}\nCurrent: {}\nLatest: {}\nRelease: {}\n\n{}",
                        result.message,
                        result.current_version,
                        result.latest_version,
                        result.release_url,
                        result.notes
                    )));
                    ui.set_preview_meta(ss(
                        "No files are downloaded from update checks. Open the release to install manually.",
                    ));
                }
                Err(error) => {
                    self.show_toast_kind(ui, format!("Update check failed: {error}"), "error")
                }
            },
            "shortcut-editor" => self.show_shortcuts(ui),
            "undo" => self.undo(ui),
            "focus-search" => {
                let n = ui.get_toolbar_search_focus_nonce();
                ui.set_toolbar_search_focus_nonce(n.wrapping_add(1));
            }
            "restore" => self.restore_from_recycle_bin(ui),
            "purge" => self.purge_from_recycle_bin(ui),
            "empty-trash" => self.empty_recycle_bin(ui),
            _ => self.show_toast(ui, format!("Command not implemented: {command}")),
        }
    }

    fn prompt_rename(&mut self, ui: &MainWindow) {
        let index = if self.active_pane == ActivePane::Secondary {
            self.secondary_rename_index()
        } else {
            self.primary_rename_index()
        };

        let Some(index) = index else {
            if self.active_pane == ActivePane::Secondary {
                if !self.secondary_selected_set.is_empty() {
                    self.show_toast(ui, "Select a single item to rename.");
                } else {
                    self.show_toast(ui, "Select a file first.");
                }
            } else if !self.selected_set.is_empty() {
                self.show_toast(ui, "Select a single item to rename.");
            } else {
                self.show_toast(ui, "Select a file first.");
            }
            return;
        };
        ui.set_rename_index(index);
    }

    fn prompt_new_folder(&mut self, ui: &MainWindow) {
        self.pending_prompt = Some(PendingPrompt::NewFolder);
        ui.set_prompt_title(ss("New folder"));
        ui.set_prompt_value(ss("New Folder"));
        ui.set_prompt_visible(true);
    }

    fn prompt_new_file(&mut self, ui: &MainWindow) {
        self.pending_prompt = Some(PendingPrompt::NewFile);
        ui.set_prompt_title(ss("New file"));
        ui.set_prompt_value(ss("New Text Document.txt"));
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

    fn batch_rename_params_from_ui(&self, ui: &MainWindow) -> BatchRenameParams {
        BatchRenameParams {
            find: ui.get_batch_rename_find().to_string(),
            replace: ui.get_batch_rename_replace().to_string(),
            prefix: ui.get_batch_rename_prefix().to_string(),
            suffix: ui.get_batch_rename_suffix().to_string(),
            template: ui.get_batch_rename_template().to_string(),
        }
    }

    fn sync_batch_rename_preview(&self, ui: &MainWindow, paths: &[String]) {
        let params = self.batch_rename_params_from_ui(ui);
        let preview = batch_rename_preview_lines(paths, &params, 12);
        let suffix = if paths.len() > 12 {
            format!("\n... and {} more", paths.len() - 12)
        } else {
            String::new()
        };
        ui.set_batch_rename_preview(ss(format!("{preview}{suffix}")));
    }

    fn execute_batch_rename(
        &mut self,
        ui: &MainWindow,
        paths: Vec<String>,
        mut params: BatchRenameParams,
    ) -> Result<usize, String> {
        let template = params.template.trim();
        if template.is_empty()
            && params.find.trim().is_empty()
            && params.prefix.is_empty()
            && params.suffix.is_empty()
        {
            return Err("Enter a template, find/replace pattern, or prefix/suffix.".to_string());
        }

        if !template.is_empty()
            && paths.len() > 1
            && !template.contains("{n}")
            && !template.contains("{n:")
        {
            let width = paths.len().to_string().len().max(2);
            let pad = format!("{{n:0{width}}}");
            params.template = if template.contains("{ext}") {
                template.replace("{ext}", &format!("_{pad}.{{ext}}"))
            } else {
                format!("{template}_{pad}")
            };
        }

        let mut plan: Vec<(String, String)> = Vec::with_capacity(paths.len());
        let mut seen_names = std::collections::HashSet::<String>::new();

        for (index, from) in paths.iter().enumerate() {
            let src = Path::new(from);
            let Some(parent) = src.parent() else {
                continue;
            };
            let new_name = apply_batch_rename_advanced(&params, src, index + 1)?;
            if !seen_names.insert(new_name.clone()) {
                return Err(format!(
                    "Duplicate name '{new_name}'. Add {{n}} to the template."
                ));
            }
            let to = parent.join(&new_name);
            if to.exists() && !same_destination(src, &to) {
                return Err(format!("'{}' already exists", to.display()));
            }
            plan.push((from.clone(), new_name));
        }

        let mut applied: Vec<(String, String)> = Vec::with_capacity(plan.len());
        for (from, new_name) in &plan {
            match native_rename(&self.app_state, from, new_name) {
                Ok(_) => applied.push((from.clone(), new_name.clone())),
                Err(error) => {
                    for (original_from, renamed) in applied.iter().rev() {
                        let src = Path::new(original_from);
                        let Some(parent) = src.parent() else {
                            continue;
                        };
                        let current = parent.join(renamed);
                        let old_name = src
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let _ = native_rename(
                            &self.app_state,
                            &current.to_string_lossy(),
                            &old_name,
                        );
                    }
                    return Err(error);
                }
            }
        }

        let log_ops: Vec<RenameOp> = plan
            .iter()
            .map(|(from, new_name)| {
                let to = Path::new(from).parent().unwrap().join(new_name);
                RenameOp {
                    from: from.clone(),
                    to: to.to_string_lossy().into_owned(),
                }
            })
            .collect();
        self.app_state.log_op_batch_rename(log_ops);

        self.refresh(ui);
        Ok(plan.len())
    }

    fn cancel_prompt(&mut self, ui: &MainWindow) {
        self.pending_prompt = None;
        ui.set_prompt_visible(false);
        ui.set_prompt_kind(ss("text"));
        ui.set_rename_presets_visible(false);
    }

    fn prompt_rename_conflict(&mut self, ui: &MainWindow, path: String, new_name: String) {
        self.pending_prompt = Some(PendingPrompt::RenameConflict { path, new_name: new_name.clone() });
        ui.set_prompt_kind(ss("rename-conflict"));
        ui.set_prompt_title(ss("Name already exists"));
        ui.set_prompt_value(ss(new_name));
        ui.set_prompt_visible(true);
    }

    fn commit_rename(
        &mut self,
        ui: &MainWindow,
        old_path: &str,
        old_name: &str,
        final_name: &str,
        is_dir: bool,
    ) {
        let final_name = finalize_inline_rename_name(old_name, final_name, is_dir);
        if final_name.is_empty() || names_same_for_rename(old_name, &final_name) {
            return;
        }
        let src = Path::new(old_path);
        let parent = match src.parent() {
            Some(p) => p,
            None => {
                self.show_toast_kind(ui, "No parent directory", "error");
                return;
            }
        };
        let dst = parent.join(&final_name);
        if dst.exists() && !same_destination(src, &dst) {
            self.prompt_rename_conflict(ui, old_path.to_string(), final_name);
            return;
        }
        match native_rename(&self.app_state, old_path, &final_name) {
            Ok(_) => {
                if self.active_pane == ActivePane::Secondary && !self.secondary_path.is_empty() {
                    let path = self.secondary_path.clone();
                    self.app_state
                        .invalidate_directory_path(std::path::Path::new(&path));
                    self.secondary_navigate(ui, path);
                } else {
                    self.refresh(ui);
                }
                self.show_toast_kind(ui, "Renamed", "success");
            }
            Err(error) => self.show_toast_kind(ui, error, "error"),
        }
    }

    fn reorder_tab(&mut self, ui: &MainWindow, from: i32, to: i32) {
        if from < 0 || to < 0 {
            return;
        }
        let from = from as usize;
        let to = to as usize;
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let active_path = self.tabs.get(self.active_tab).map(|t| t.path.clone());
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(path) = active_path {
            self.active_tab = self
                .tabs
                .iter()
                .position(|t| t.path == path)
                .unwrap_or(0);
        }
        self.update_models(ui);
        self.save_session();
    }

    fn prompt_batch_rename(&mut self, ui: &MainWindow) {
        let paths = self.selected_paths();
        if paths.len() < 2 {
            self.show_toast(ui, "Select at least two items for batch rename.");
            return;
        }
        ui.set_prompt_kind(ss("batch-rename"));
        ui.set_batch_rename_find(ss(""));
        ui.set_batch_rename_replace(ss(""));
        ui.set_batch_rename_prefix(ss(""));
        ui.set_batch_rename_suffix(ss(""));
        ui.set_batch_rename_template(ss("Renamed_{n:03}.{ext}"));
        self.pending_prompt = Some(PendingPrompt::BatchRename(paths.clone()));
        ui.set_prompt_title(ss("Batch rename"));
        self.sync_batch_rename_preview(ui, &paths);
        ui.set_prompt_visible(true);
    }

    fn prompt_delete(&mut self, ui: &MainWindow) {
        let paths = self.selected_paths();
        let n = paths.len();
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
                let dest_dir = self.active_directory().to_string();
                let path = PathBuf::from(&dest_dir).join(value.trim());
                match native_create_directory(&self.app_state, &path.to_string_lossy()) {
                    Ok(()) => {
                        if self.active_pane == ActivePane::Secondary {
                            self.secondary_navigate(ui, dest_dir);
                        } else {
                            self.refresh(ui);
                        }
                        self.show_toast_kind(ui, "Folder created", "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::NewFile) => {
                let name = value.trim();
                if name.is_empty() {
                    self.show_toast_kind(ui, "Name cannot be empty", "error");
                    return;
                }
                if name.contains('/') || name.contains('\\') {
                    self.show_toast_kind(ui, "Name cannot contain path separators", "error");
                    return;
                }
                let dest_dir = self.active_directory().to_string();
                let path = PathBuf::from(&dest_dir).join(name);
                match native_create_file(&self.app_state, &path.to_string_lossy()) {
                    Ok(()) => {
                        if self.active_pane == ActivePane::Secondary {
                            self.secondary_navigate(ui, dest_dir);
                        } else {
                            self.refresh(ui);
                        }
                        self.show_toast_kind(ui, "File created", "success");
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
            Some(PendingPrompt::ArchivePassword { jobs, conflict }) => {
                let password = if value.trim().is_empty() {
                    None
                } else {
                    Some(value)
                };
                self.start_bulk_archive_extract_async(ui, jobs, password, conflict);
            }
            Some(PendingPrompt::NewTemplate(template)) => {
                let base = value.trim();
                if base.is_empty() {
                    self.show_toast(ui, "Name cannot be empty.");
                    return;
                }
                let dest_dir = self.active_directory().to_string();
                let mut path = PathBuf::from(&dest_dir).join(base);
                if path.extension().is_none() {
                    path.set_extension(&template.extension);
                }
                if path.exists() {
                    path = keep_both_destination(&path);
                }
                match File::create(&path).and_then(|mut f| f.write_all(template.content.as_bytes()))
                {
                    Ok(()) => {
                        if self.active_pane == ActivePane::Secondary {
                            self.secondary_navigate(ui, dest_dir);
                        } else {
                            self.refresh(ui);
                        }
                        self.show_toast_kind(ui, "Template file created", "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error.to_string(), "error"),
                }
            }
            Some(PendingPrompt::BatchRename(paths)) => {
                let params = self.batch_rename_params_from_ui(ui);
                let paths_for_retry = paths.clone();
                match self.execute_batch_rename(ui, paths, params) {
                    Ok(count) => {
                        self.show_toast_kind(ui, format!("Renamed {count} items"), "success");
                    }
                    Err(error) => {
                        self.pending_prompt = Some(PendingPrompt::BatchRename(paths_for_retry.clone()));
                        ui.set_prompt_kind(ss("batch-rename"));
                        self.sync_batch_rename_preview(ui, &paths_for_retry);
                        ui.set_prompt_visible(true);
                        self.show_toast_kind(ui, error, "error");
                    }
                }
            }
            Some(PendingPrompt::RenameConflict { path, new_name }) => {
                match value.as_str() {
                    "skip" => {}
                    "keep" => {
                        let src = Path::new(&path);
                        let Some(parent) = src.parent() else {
                            return;
                        };
                        let dst = keep_both_destination(&parent.join(&new_name));
                        let kept_name = dst
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or(new_name);
                        match native_rename(&self.app_state, &path, &kept_name) {
                            Ok(_) => {
                                self.refresh(ui);
                                self.show_toast_kind(ui, "Renamed (kept both)", "success");
                            }
                            Err(error) => self.show_toast_kind(ui, error, "error"),
                        }
                    }
                    "replace" => match native_rename_replace(&self.app_state, &path, &new_name) {
                        Ok(_) => {
                            self.refresh(ui);
                            self.show_toast_kind(ui, "Renamed (replaced)", "success");
                        }
                        Err(error) => self.show_toast_kind(ui, error, "error"),
                    },
                    _ => {}
                }
                ui.set_prompt_kind(ss("text"));
            }
            Some(PendingPrompt::RenamePreset(paths)) => {
                let preset = value.trim();
                if preset.is_empty() {
                    self.show_toast(ui, "Enter a preset name.");
                    return;
                }
                if preset == "number sequence" || paths.len() >= 2 {
                    self.sync_batch_rename_preview(ui, &paths);
                    self.pending_prompt = Some(PendingPrompt::BatchRename(paths));
                    ui.set_prompt_kind(ss("batch-rename"));
                    ui.set_batch_rename_template(ss("Renamed_{n:03}.{ext}"));
                    ui.set_prompt_title(ss("Batch rename"));
                    ui.set_prompt_visible(true);
                    return;
                }
                let from = &paths[0];
                let file_name = Path::new(from)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let Some(renamed) = apply_rename_preset(preset, &file_name) else {
                    self.show_toast_kind(ui, format!("Unknown preset: {preset}"), "error");
                    return;
                };
                match native_rename(&self.app_state, from, &renamed) {
                    Ok(_) => {
                        self.refresh(ui);
                        self.show_toast_kind(ui, format!("Renamed to {renamed}"), "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::CompareFolder(left)) => {
                let right = value.trim();
                if right.is_empty() {
                    self.show_toast(ui, "Pick a folder path to compare.");
                    return;
                }
                match compare_folders(Path::new(&left), Path::new(right), 2000) {
                    Ok(rows) => {
                        self.compare_left = left;
                        self.compare_right = right.to_string();
                        self.compare_all_rows = rows;
                        self.compare_hide_same = true;
                        self.push_compare_overlay(ui);
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::SaveWorkspace) => {
                let name = value.trim();
                if name.is_empty() {
                    self.show_toast(ui, "Workspace name cannot be empty.");
                    return;
                }
                let mut list = load_workspaces();
                list.retain(|w| w.name != name);
                list.insert(
                    0,
                    WorkspaceSession {
                        name: name.to_string(),
                        tabs: self.tabs.clone(),
                    },
                );
                if list.len() > 30 {
                    list.truncate(30);
                }
                match save_workspaces(&list) {
                    Ok(()) => self.show_toast_kind(ui, format!("Saved workspace '{name}'"), "success"),
                    Err(e) => self.show_toast_kind(ui, e, "error"),
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
                        self.refresh_after_paste(ui, cut, std::slice::from_ref(&src));
                        self.show_toast_kind(ui, "Conflict resolved", "success");
                    }
                    Err(error) => self.show_toast_kind(ui, error, "error"),
                }
            }
            Some(PendingPrompt::RenameTag(tag_id)) => {
                let new_label = value.trim().to_string();
                if let Some(smart_id) = tag_id.strip_prefix("smart:") {
                    match rename_smart_folder(smart_id.to_string(), new_label.clone()) {
                        Ok(_) => {
                            ui.set_side_items(model_from_vec(self.side_items()));
                            self.show_smart_folders(ui);
                            self.show_toast_kind(ui, "Smart folder renamed", "success");
                        }
                        Err(e) => self.show_toast_kind(ui, e, "error"),
                    }
                } else {
                    if new_label.is_empty() {
                        self.tag_labels.remove(&tag_id);
                    } else {
                        self.tag_labels.insert(tag_id, new_label.clone());
                    }
                    let _ = write_native_json("tag_labels.json", &self.tag_labels);
                    self.sync_tag_names(ui);
                    ui.set_side_items(model_from_vec(self.side_items()));
                    ui.set_side_items_simple(model_from_vec(self.side_items_simple()));
                    self.show_toast_kind(ui, "Tag renamed", "success");
                }
            }
            Some(PendingPrompt::Archive) | Some(PendingPrompt::StorageCleanup(_)) | None => {}
        }
    }

    fn confirm_delete(&mut self, ui: &MainWindow) {
        let cleanup_paths = match self.pending_prompt.take() {
            Some(PendingPrompt::StorageCleanup(paths)) => Some(paths),
            other => {
                self.pending_prompt = other;
                None
            }
        };
        let from_storage_cleanup = cleanup_paths.is_some();
        let paths = cleanup_paths.unwrap_or_else(|| self.selected_paths());
        if paths.is_empty() {
            return;
        }
        ui.set_confirm_visible(false);

        let n = paths.len();
        if from_storage_cleanup {
            self.storage_caches.remove(&self.storage_current_root);
        }
        self.selected_set.clear();
        self.secondary_selected_set.clear();
        self.selected_index = -1;
        self.secondary_selected_index = -1;
        self.select_anchor = -1;
        self.secondary_select_anchor = -1;
        self.sync_selection_count_to_ui(ui);
        ui.set_selected_index(-1);

        ui.set_op_drawer_text(ss(if n == 1 {
            "Moving to Recycle Bin...".to_string()
        } else {
            format!("Moving {n} items to Recycle Bin...")
        }));
        ui.set_op_drawer_visible(true);
        ui.set_op_drawer_progress(-1.0);

        let app_state = self.app_state.clone();
        let operation_ready = self.operation_ready.clone();
        let pending_result = self.pending_operation_result.clone();
        std::thread::spawn(move || {
            let mut errors = 0usize;
            let mut first_error: Option<String> = None;
            for path in &paths {
                match native_delete_fast(&app_state, path) {
                    Ok(()) => {}
                    Err(e) => {
                        errors += 1;
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
            }
            let result = if let Some(err) = first_error {
                NativeOperationResult {
                    message: if errors == paths.len() {
                        err
                    } else {
                        format!("{errors} of {n} failed to delete. {err}")
                    },
                    kind: "error".to_string(),
                    refresh: errors < paths.len(),
                    refresh_both_panes: false,
                    secondary_refresh_path: None,
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                }
            } else {
                let msg = if n == 1 {
                    if from_storage_cleanup {
                        "Cleanup item moved to Recycle Bin".to_string()
                    } else {
                        "Moved to Recycle Bin".to_string()
                    }
                } else {
                    format!("{n} items moved to Recycle Bin")
                };
                NativeOperationResult {
                    message: msg,
                    kind: "success".to_string(),
                    refresh: true,
                    refresh_both_panes: false,
                    secondary_refresh_path: None,
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                }
            };
            if let Ok(mut lock) = pending_result.lock() {
                *lock = Some(result);
            }
            operation_ready.store(true, Ordering::Release);
        });
    }

    fn copy_selected(&mut self, cut: bool, ui: &MainWindow) {
        if self.active_path_is_recycle_bin() || is_virtual_nav_path(self.active_directory()) {
            self.show_toast(
                ui,
                if cut {
                    "Cut is not available here."
                } else {
                    "Copy is not available here."
                },
            );
            return;
        }
        let paths: Vec<String> = self
            .selected_paths()
            .into_iter()
            .filter(|p| Self::is_clipboardable_path(p))
            .collect();
        if paths.is_empty() {
            self.show_toast(ui, "Select a file first.");
            return;
        }
        let n = paths.len();
        self.clipboard = Some(NativeClipboard {
            paths: paths.clone(),
            cut,
        });
        #[cfg(target_os = "windows")]
        if let Err(error) = file_drag::set_shell_files_clipboard(&paths, cut) {
            eprintln!("set_shell_files_clipboard: {error}");
            self.show_toast_kind(
                ui,
                "Clipboard sync failed — paste may not work in Explorer.".to_string(),
                "error",
            );
        }
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
        self.refresh_clipboard_visuals(ui);
    }

    fn paste(&mut self, ui: &MainWindow) {
        let Some(clipboard) = self.clipboard.clone() else {
            self.show_toast(ui, "Clipboard is empty.");
            return;
        };
        let dest_dir = self.active_directory().to_string();
        let n = clipboard.paths.len();
        if n > 1 {
            let verb = if clipboard.cut { "Moving" } else { "Copying" };
            ui.set_op_drawer_text(ss(format!("{verb} {n} items...")));
            ui.set_op_drawer_visible(true);
        }
        let mut pasted = 0usize;
        for src in &clipboard.paths {
            let Some(name) = Path::new(src).file_name() else {
                continue;
            };
            let dest = PathBuf::from(&dest_dir).join(name);
            if self.paste_skips_dest(src, &dest, clipboard.cut) {
                continue;
            }
            if self.paste_has_real_conflict(src, &dest) {
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
                ui.set_prompt_title(ss(format!(
                    "{} already exists",
                    Path::new(&dest)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("File")
                )));
                ui.set_prompt_value(ss(""));
                ui.set_prompt_kind(ss("conflict"));
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
        self.refresh_after_paste(ui, clipboard.cut, &clipboard.paths);
        let verb = if clipboard.cut { "Moved" } else { "Pasted" };
        let msg = if pasted == 1 {
            format!("{verb} 1 item")
        } else {
            format!("{verb} {pasted} items")
        };
        self.show_toast_kind(ui, msg, "success");
    }

    fn drop_files_from_drag(
        &mut self,
        ui: &MainWindow,
        paths: Vec<String>,
        is_move: bool,
        dest_dir: String,
    ) {
        file_drag::log(&format!(
            "drop_files_from_drag: dest_dir='{}', is_move={}, paths={:?}",
            dest_dir, is_move, paths
        ));

        if dest_dir.is_empty() || is_virtual_nav_path(&dest_dir) {
            file_drag::log(&format!(
                "drop_files_from_drag: invalid destination '{}', ignoring",
                dest_dir
            ));
            return;
        }

        // Dropping onto the Recycle Bin sidebar entry sends every source path
        // to the OS trash, matching what File Explorer does when you drag onto
        // its Recycle Bin tile. The recycle:// path is a virtual destination
        // and never a real folder, so we have to route it here before the
        // normal move/copy path tries to use it as a parent directory.
        if dest_dir == "recycle://" {
            file_drag::log("drop -> Recycle Bin (trash::delete_all)");
            let count = paths.len();
            let result: Result<(), trash::Error> = trash::delete_all(&paths);
            match result {
                Ok(()) => {
                    self.invalidate_and_refresh_both_panes(ui);
                    let kind = "success";
                    let msg = if count == 1 {
                        "Moved 1 item to Recycle Bin".to_string()
                    } else {
                        format!("Moved {count} items to Recycle Bin")
                    };
                    self.show_toast_kind(ui, msg, kind);
                }
                Err(e) => {
                    self.show_toast_kind(
                        ui,
                        format!("Failed to send items to Recycle Bin: {e}"),
                        "error",
                    );
                }
            }
            return;
        }

        let mut count = 0usize;
        let mut errors = 0usize;
        let mut skipped_self = 0usize;
        let mut last_error: Option<String> = None;
        for src_str in &paths {
            let src = Path::new(src_str);
            let Some(name) = src.file_name() else {
                file_drag::log(&format!(
                    "  '{src_str}' has no file_name component, skipping"
                ));
                continue;
            };
            let dest = PathBuf::from(&dest_dir).join(name);
            let same = same_inode_or_canonical_path(src, &dest);
            // Catch the "drag folder onto itself or into a descendant of itself"
            // case before fs::rename returns an OS error that's hard to parse.
            // Comparing canonicalised paths handles symlinks too.
            let canonical_src = std::fs::canonicalize(src).ok();
            let canonical_dest_parent = std::fs::canonicalize(&dest_dir).ok();
            let is_self_descent = match (canonical_src.as_ref(), canonical_dest_parent.as_ref()) {
                (Some(c_src), Some(c_dest_parent)) => {
                    src.is_dir() && c_dest_parent.starts_with(c_src)
                }
                _ => false,
            };
            file_drag::log(&format!(
                "  src='{}' -> dest='{}' same={} self_descent={}",
                src_str,
                dest.display(),
                same,
                is_self_descent
            ));
            if same || is_self_descent {
                skipped_self += 1;
                continue;
            }
            let result = if is_move {
                native_move(&self.app_state, src_str, &dest.to_string_lossy())
            } else {
                native_copy(&self.app_state, src_str, &dest.to_string_lossy())
            };
            match result {
                Ok(()) => {
                    file_drag::log(&format!("    {} OK", if is_move { "move" } else { "copy" }));
                    count += 1;
                }
                Err(e) => {
                    file_drag::log(&format!(
                        "    {} FAILED: {}",
                        if is_move { "move" } else { "copy" },
                        e
                    ));
                    last_error = Some(e);
                    errors += 1;
                }
            }
        }
        file_drag::log(&format!(
            "drop summary: count={}, errors={}, skipped_self={}",
            count, errors, skipped_self
        ));
        if count == 0 && errors == 0 {
            // Pure no-op drop (file dropped onto its own folder). Silent, like Explorer.
            let _ = skipped_self;
            return;
        }
        // Refresh BOTH panes so files appear/disappear in primary AND secondary.
        // Without this the destination pane (which is NOT the "active" pane in the
        // dual-pane drag-from-A-to-B case) doesn't redraw and the move looks broken.
        self.invalidate_and_refresh_both_panes(ui);
        let verb = if is_move { "Moved" } else { "Copied" };
        let kind = if errors > 0 { "error" } else { "success" };
        let msg = if errors > 0 {
            if count > 0 {
                format!(
                    "{verb} {count}, {errors} failed: {}",
                    last_error.as_deref().unwrap_or("unknown")
                )
            } else {
                format!(
                    "{verb} failed: {}",
                    last_error.as_deref().unwrap_or("unknown")
                )
            }
        } else if count == 1 {
            format!(
                "{verb} 1 item to {}",
                Path::new(&dest_dir)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&dest_dir)
            )
        } else {
            format!(
                "{verb} {count} items to {}",
                Path::new(&dest_dir)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&dest_dir)
            )
        };
        self.show_toast_kind(ui, msg, kind);
    }

    /// Invalidate directory caches for both panes immediately, and schedule the
    /// full UI refresh on the next event loop tick. Running both re-navigates
    /// synchronously inside the Drop callback caused a visible hitch after
    /// every drop because navigate() does a fresh directory list, fires off a
    /// background git status thread, and rebuilds the file model. The drop
    /// completes faster if we let the cursor return first, then refresh.
    fn invalidate_and_refresh_both_panes(&mut self, ui: &MainWindow) {
        // Invalidate caches synchronously so the deferred navigate() doesn't
        // re-use stale entries that still contain the moved file in its old
        // location.
        let primary_path = self.current_path.clone();
        let secondary_path = self.secondary_path.clone();
        self.app_state
            .invalidate_directory_path(Path::new(&primary_path));
        if !secondary_path.is_empty() {
            self.app_state
                .invalidate_directory_path(Path::new(&secondary_path));
        }
        // Defer the heavy re-navigate to the next event tick. The post-drag UI
        // returns immediately and the file list redraws on the next frame. The
        // existing primary refresh callback only refreshes whichever pane is
        // currently active, so we force-refresh primary (preserving the saved
        // active pane) and fire secondary_refresh separately.
        let saved_active = self.active_pane;
        let weak = ui.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                // refresh() routes by active_pane internally; we need both. We
                // dispatch secondary first, then primary, and let the callback
                // handlers borrow_mut the controller in turn.
                if ui.get_dual_pane() {
                    ui.invoke_secondary_refresh();
                }
                ui.invoke_refresh();
            }
        });
        self.active_pane = saved_active;
        self.sync_active_pane(ui);
    }

    fn paste_async(&mut self, ui: &MainWindow) {
        // Prefer Explorer's shell clipboard only when Pathfinder has nothing queued.
        // Reading shell on every paste overwrote a fresh internal cut/copy when
        // set_shell_files_clipboard failed or Explorer still held stale CF_HDROP data.
        self.sync_shell_clipboard_if_needed();
        let Some(clipboard) = self.clipboard.clone() else {
            self.show_toast(ui, "Clipboard is empty.");
            return;
        };

        let dest_dir = self.active_directory().to_string();
        if is_virtual_nav_path(&dest_dir) {
            self.show_toast(ui, "Paste is not available here.");
            return;
        }
        if !Path::new(&dest_dir).is_dir() {
            self.show_toast(ui, "No folder open to paste into.");
            return;
        }
        let dest_is_secondary = self.active_pane == ActivePane::Secondary;
        let dual_pane = ui.get_dual_pane();
        let cut = clipboard.cut;
        for src in &clipboard.paths {
            let Some(name) = Path::new(src).file_name() else {
                continue;
            };
            let dest = PathBuf::from(&dest_dir).join(name);
            if self.paste_skips_dest(src, &dest, cut) {
                continue;
            }
            if self.paste_has_real_conflict(src, &dest) {
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
                ui.set_prompt_title(ss(format!(
                    "{} already exists",
                    Path::new(&dest)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("File")
                )));
                ui.set_prompt_value(ss(""));
                ui.set_prompt_kind(ss("conflict"));
                ui.set_prompt_visible(true);
                return;
            }
        }

        let paths_to_paste: Vec<String> = clipboard
            .paths
            .iter()
            .filter(|src| {
                if src.starts_with("recycle://") || is_virtual_nav_path(src) {
                    return false;
                }
                let Some(name) = Path::new(src).file_name() else {
                    return false;
                };
                let dest = PathBuf::from(&dest_dir).join(name);
                !self.paste_skips_dest(src, &dest, cut) && Path::new(src).exists()
            })
            .cloned()
            .collect();
        let n = paths_to_paste.len();
        if n == 0 {
            self.show_toast(ui, "Nothing to paste here.");
            return;
        }

        let verb = if cut { "Moving" } else { "Copying" };
        ui.set_op_drawer_text(ss(format!(
            "{verb} {n} item{}",
            if n == 1 { "" } else { "s" }
        )));
        ui.set_op_drawer_visible(true);

        let app_state = self.app_state.clone();
        let operation_ready = self.operation_ready.clone();
        let pending_result = self.pending_operation_result.clone();
        let invalidate_dirs_on_success =
            Self::paste_invalidate_dirs(&dest_dir, &paths_to_paste, cut);
        std::thread::spawn(move || {
            let mut completed = 0usize;
            let mut first_error: Option<String> = None;

            for src in &paths_to_paste {
                if app_state.queue_cancel_requested() {
                    first_error = Some(OP_CANCELLED.to_string());
                    break;
                }
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

            let pasted = completed > 0;
            let moved_sources: Vec<String> = paths_to_paste
                .iter()
                .take(completed)
                .cloned()
                .collect();
            let invalidate_dirs = if pasted {
                NativeController::paste_invalidate_dirs(&dest_dir, &moved_sources, cut)
            } else {
                Vec::new()
            };
            let result = if let Some(error) = first_error {
                NativeOperationResult {
                    message: error,
                    kind: "error".to_string(),
                    refresh: pasted && !dual_pane && !dest_is_secondary,
                    refresh_both_panes: pasted && dual_pane,
                    secondary_refresh_path: if pasted && !dual_pane && dest_is_secondary {
                        Some(dest_dir.clone())
                    } else {
                        None
                    },
                    clear_clipboard: false,
                    invalidate_dirs,
                }
            } else {
                let verb_done = if cut { "Moved" } else { "Pasted" };
                NativeOperationResult {
                    message: format!(
                        "{verb_done} {completed} item{}",
                        if completed == 1 { "" } else { "s" }
                    ),
                    kind: "success".to_string(),
                    refresh: pasted && !dual_pane && !dest_is_secondary,
                    refresh_both_panes: pasted && dual_pane,
                    secondary_refresh_path: if pasted && !dual_pane && dest_is_secondary {
                        Some(dest_dir.clone())
                    } else {
                        None
                    },
                    clear_clipboard: cut,
                    invalidate_dirs: invalidate_dirs_on_success,
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
        match build_storage_tree(Path::new(self.active_directory()), 4) {
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
        match find_duplicates(self.active_directory().to_string(), Some(1024)) {
            Ok(groups) => {
                self.dupe_groups_cache = groups
                    .iter()
                    .enumerate()
                    .map(|(i, group)| {
                        let title = group
                            .first()
                            .map(|f| f.name.clone())
                            .unwrap_or_else(|| format!("Group {}", i + 1));
                        (
                            format!("exact-{i}::{title}"),
                            group.iter().map(|f| f.path.clone()).collect(),
                        )
                    })
                    .collect();
                let items: Vec<DupeGroupItem> = self
                    .dupe_groups_cache
                    .iter()
                    .map(|(id_title, paths)| {
                        let title = id_title
                            .split_once("::")
                            .map(|(_, t)| t.to_string())
                            .unwrap_or_else(|| id_title.clone());
                        let size = paths
                            .first()
                            .and_then(|p| fs::metadata(p).ok())
                            .map(|m| m.len())
                            .unwrap_or(0);
                        let reclaim = size.saturating_mul(paths.len().saturating_sub(1) as u64);
                        DupeGroupItem {
                            id: ss(id_title),
                            title: ss(&title),
                            detail: ss(format!("{} identical files", paths.len())),
                            count: paths.len() as i32,
                            reclaimable: ss(format_size_short(reclaim)),
                        }
                    })
                    .collect();
                ui.set_dupe_overlay_title(ss("Duplicate files"));
                ui.set_dupe_overlay_subtitle(ss(if items.is_empty() {
                    "No duplicate files found in this folder."
                } else {
                    "Keep one file per group, or delete the extras to Recycle Bin."
                }));
                ui.set_dupe_groups(model_from_vec(items));
                ui.set_dupe_overlay_visible(true);
            }
            Err(error) => self.show_toast(ui, error),
        }
    }

    fn show_image_duplicates(&mut self, ui: &MainWindow) {
        let groups = collect_image_duplicate_groups(&self.current_path);
        self.dupe_groups_cache = groups
            .into_iter()
            .enumerate()
            .map(|(i, (title, paths))| (format!("img-{i}::{title}"), paths))
            .collect();
        let items: Vec<DupeGroupItem> = self
            .dupe_groups_cache
            .iter()
            .map(|(id_title, paths)| {
                let title = id_title
                    .split_once("::")
                    .map(|(_, t)| t.to_string())
                    .unwrap_or_else(|| id_title.clone());
                let size = paths
                    .first()
                    .and_then(|p| fs::metadata(p).ok())
                    .map(|m| m.len())
                    .unwrap_or(0);
                let reclaim = size.saturating_mul(paths.len().saturating_sub(1) as u64);
                DupeGroupItem {
                    id: ss(id_title),
                    title: ss(&title),
                    detail: ss(format!("{} matching dHash images", paths.len())),
                    count: paths.len() as i32,
                    reclaimable: ss(format_size_short(reclaim)),
                }
            })
            .collect();
        ui.set_dupe_overlay_title(ss("Duplicate images"));
        ui.set_dupe_overlay_subtitle(ss(if items.is_empty() {
            "No exact duplicate image hashes in this folder (index more images first)."
        } else {
            "Keep the first image in each group, or delete the others."
        }));
        ui.set_dupe_groups(model_from_vec(items));
        ui.set_dupe_overlay_visible(true);
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
        self.navigate(ui, "home://".to_string(), true);
    }

    fn open_home_view(&mut self, ui: &MainWindow, push_history: bool) {
        if ui.get_is_storage_view() {
            self.close_storage_view(ui);
        }
        ui.set_in_recycle_bin(false);
        let mut entries = Vec::new();
        let push_dir = |entries: &mut Vec<FileEntry>, name: &str, path: &str| {
            if path.is_empty() || !Path::new(path).exists() {
                return;
            }
            entries.push(FileEntry {
                path: path.to_string(),
                name: name.to_string(),
                name_lower: name.to_ascii_lowercase(),
                kind: FileKind::Directory,
                size: 0,
                modified: 0,
                extension: None,
            });
        };
        for drive in &self.drives {
            let label = if drive.name.is_empty() {
                drive.path.clone()
            } else {
                format!("{} ({})", drive.name, drive.path.trim_end_matches('\\'))
            };
            push_dir(&mut entries, &label, &drive.path);
        }
        for pin in self.user_pins.iter().take(12) {
            let kind = if pin.kind == "file" {
                FileKind::File
            } else {
                FileKind::Directory
            };
            entries.push(FileEntry {
                path: pin.path.clone(),
                name: format!("Pinned · {}", pin.name),
                name_lower: pin.name.to_ascii_lowercase(),
                kind,
                size: 0,
                modified: pin.pinned_at,
                extension: Path::new(&pin.path)
                    .extension()
                    .map(|e| e.to_string_lossy().to_string()),
            });
        }
        for path in self.recent_locations.iter().take(8) {
            if path == "home://" || path == "storage://" || path == "recycle://" {
                continue;
            }
            let label = Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            push_dir(&mut entries, &format!("Recent · {label}"), path);
        }
        for search in read_native_json::<Vec<SavedSearch>>("searches.json", Vec::new())
            .into_iter()
            .take(6)
        {
            entries.push(FileEntry {
                path: format!("search:{}", search.name),
                name: format!("Saved · {}", search.name),
                name_lower: search.name.to_ascii_lowercase(),
                kind: FileKind::Other,
                size: 0,
                modified: 0,
                extension: None,
            });
        }
        if let Some(downloads) = dirs::download_dir() {
            push_dir(
                &mut entries,
                "Downloads",
                &downloads.to_string_lossy(),
            );
        }
        entries.push(FileEntry {
            path: "storage://".to_string(),
            name: "Storage analyzer".to_string(),
            name_lower: "storage analyzer".to_string(),
            kind: FileKind::Directory,
            size: 0,
            modified: 0,
            extension: None,
        });

        let path = "home://".to_string();
        if push_history {
            self.history.truncate(self.history_index + 1);
            self.history.push(path.clone());
            self.history_index = self.history.len().saturating_sub(1);
        }
        self.current_path = path.clone();
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.path = path.clone();
        }
        self.files = entries;
        self.search_query.clear();
        ui.set_search_text(ss(""));
        self.selected_index = -1;
        self.selected_set.clear();
        self.select_anchor = -1;
        self.files_model = None;
        self.active_archive = None;
        ui.set_view_mode(ss("list"));
        ui.set_empty_state(ss(if self.files.is_empty() {
            "Home is empty — pin folders or browse a drive to get started."
        } else {
            ""
        }));
        self.apply_filter();
        self.update_models(ui);
        ui.set_side_items(model_from_vec(self.side_items()));
        ui.set_current_path(ss("Home"));
        ui.set_address_text(ss("Home"));
        ui.set_status_left(ss(format!("{} shortcuts", self.files.len())));
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
        let items: Vec<ToolListItem> = smart_folders_for_path(&self.current_path)
            .into_iter()
            .map(|s| ToolListItem {
                id: ss(&s.id),
                title: ss(&s.name),
                subtitle: ss(&s.description),
                meta: ss(""),
                enabled: true,
                accent: color("#4f9cff"),
            })
            .collect();
        ui.set_tool_overlay_kind(ss("smart-folders"));
        ui.set_tool_overlay_title(ss("Smart Folders"));
        ui.set_tool_overlay_subtitle(ss(
            "Click a smart folder to filter. Labels can be renamed from More.",
        ));
        ui.set_tool_overlay_items(model_from_vec(items));
        ui.set_tool_overlay_visible(true);
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
        let templates = load_file_templates();
        let items: Vec<ToolListItem> = templates
            .into_iter()
            .map(|t| ToolListItem {
                id: ss(&t.name),
                title: ss(&t.name),
                subtitle: ss(format!(".{}", t.extension)),
                meta: ss(""),
                enabled: true,
                accent: color("#e2a934"),
            })
            .collect();
        ui.set_tool_overlay_kind(ss("templates"));
        ui.set_tool_overlay_title(ss("New From Template"));
        ui.set_tool_overlay_subtitle(ss("Pick a template, then name the new file."));
        ui.set_tool_overlay_items(model_from_vec(items));
        ui.set_tool_overlay_visible(true);
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
        let paths = self.selected_paths();
        if paths.is_empty() {
            ui.set_preview_title(ss("Power Rename Presets"));
            ui.set_preview_body(ss(format!(
                "{}\n\nSelect files, then run this command again.",
                presets.join("\n")
            )));
            ui.set_preview_meta(ss("Click a preset to apply it."));
            return;
        }

        self.pending_prompt = Some(PendingPrompt::RenamePreset(paths.clone()));
        let model: Vec<slint::SharedString> = presets.into_iter().map(ss).collect();
        ui.set_rename_preset_names(std::rc::Rc::new(slint::VecModel::from(model)).into());
        ui.set_rename_presets_visible(true);
    }

    fn apply_rename_preset_choice(&mut self, ui: &MainWindow, preset: String) {
        ui.set_rename_presets_visible(false);
        let paths = match self.pending_prompt.take() {
            Some(PendingPrompt::RenamePreset(paths)) => paths,
            _ => self.selected_paths(),
        };
        if paths.is_empty() {
            self.show_toast(ui, "Select files first.");
            return;
        }
        if preset == "number sequence" || paths.len() >= 2 {
            ui.set_prompt_kind(ss("batch-rename"));
            ui.set_batch_rename_template(ss("Renamed_{n:03}.{ext}"));
            self.pending_prompt = Some(PendingPrompt::BatchRename(paths.clone()));
            self.sync_batch_rename_preview(ui, &paths);
            ui.set_prompt_title(ss("Batch rename"));
            ui.set_prompt_visible(true);
            return;
        }
        let from = &paths[0];
        let file_name = Path::new(from)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(renamed) = apply_rename_preset(&preset, &file_name) else {
            self.show_toast_kind(ui, format!("Unknown preset: {preset}"), "error");
            return;
        };
        match native_rename(&self.app_state, from, &renamed) {
            Ok(_) => {
                self.refresh(ui);
                self.show_toast_kind(ui, format!("Renamed to {renamed}"), "success");
            }
            Err(error) => self.show_toast_kind(ui, error, "error"),
        }
    }

    fn selected_image_paths(&self) -> Result<Vec<String>, String> {
        let selected = self.selected_paths();
        if selected.is_empty() {
            return Err("Select an image first.".to_string());
        }

        let images: Vec<String> = selected
            .into_iter()
            .filter(|path| {
                let p = Path::new(path);
                p.is_file() && is_thumbnail_image_ext(&extension(p))
            })
            .collect();

        if images.is_empty() {
            Err("Select a JPG, PNG, GIF, WebP, or BMP image first.".to_string())
        } else {
            Ok(images)
        }
    }

    fn show_image_tools(&mut self, ui: &MainWindow) {
        let images = match self.selected_image_paths() {
            Ok(images) => images,
            Err(error) => {
                self.show_toast(ui, error);
                return;
            }
        };

        let title = if images.len() == 1 {
            Path::new(&images[0])
                .file_name()
                .map(|name| format!("Image Tools - {}", name.to_string_lossy()))
                .unwrap_or_else(|| "Image Tools".to_string())
        } else {
            format!("Image Tools - {} images", images.len())
        };
        let subtitle = if images.len() == 1 {
            "Choose a quick action. Pathfinder creates a new copy next to the original.".to_string()
        } else {
            "Choose a quick action. It will run on every selected image and create safe copies."
                .to_string()
        };

        ui.set_image_tools_title(ss(title));
        ui.set_image_tools_subtitle(ss(subtitle));
        ui.set_image_tools_visible(true);
    }

    fn run_image_tool(&mut self, ui: &MainWindow, action: ImageToolAction) {
        let images = match self.selected_image_paths() {
            Ok(images) => images,
            Err(error) => {
                self.show_toast(ui, error);
                return;
            }
        };

        let label = action.label().to_string();
        let refresh_dir = self.active_directory().to_string();
        let dual_pane = ui.get_dual_pane();
        let dest_is_secondary = self.active_pane == ActivePane::Secondary;
        ui.set_op_drawer_text(ss(format!(
            "{} {} image{}",
            label,
            images.len(),
            if images.len() == 1 { "" } else { "s" }
        )));
        ui.set_op_drawer_visible(true);

        let state = self.app_state.clone();
        let ready = self.operation_ready.clone();
        let pending = self.pending_operation_result.clone();
        std::thread::spawn(move || {
            let mut created = Vec::new();
            let mut first_error: Option<String> = None;

            for source in &images {
                match process_image_tool(Path::new(source), action) {
                    Ok(dest) => {
                        state.invalidate_path(&dest);
                        if let Some(parent) = dest.parent() {
                            state.invalidate_directory_path(parent);
                        }
                        created.push(dest);
                    }
                    Err(error) => {
                        first_error = Some(format!("{source}: {error}"));
                        break;
                    }
                }
            }

            let pasted = !created.is_empty();
            let result = if let Some(error) = first_error {
                NativeOperationResult {
                    message: error,
                    kind: "error".to_string(),
                    refresh: pasted && !dual_pane && !dest_is_secondary,
                    refresh_both_panes: pasted && dual_pane,
                    secondary_refresh_path: if pasted && !dual_pane && dest_is_secondary {
                        Some(refresh_dir)
                    } else {
                        None
                    },
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                }
            } else {
                let count = created.len();
                NativeOperationResult {
                    message: format!(
                        "{} {} image{}",
                        label,
                        count,
                        if count == 1 { "" } else { "s" }
                    ),
                    kind: "success".to_string(),
                    refresh: pasted && !dual_pane && !dest_is_secondary,
                    refresh_both_panes: pasted && dual_pane,
                    secondary_refresh_path: if pasted && !dual_pane && dest_is_secondary {
                        Some(refresh_dir)
                    } else {
                        None
                    },
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                }
            };

            if let Ok(mut lock) = pending.lock() {
                *lock = Some(result);
            }
            ready.store(true, Ordering::Release);
        });
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
        self.open_archive_view(ui, entry.path, String::new(), true);
    }

    fn selected_archive_items(&self) -> Vec<String> {
        let mut selected = Vec::new();
        for entry in self.selected_entries() {
            if let Some((_, prefix)) = parse_archive_virtual_path(&entry.path) {
                selected.push(prefix);
            }
        }
        selected.sort_unstable();
        selected.dedup();
        selected
    }

    fn selected_disk_archives(&self) -> Vec<FileEntry> {
        self.selected_entries()
            .into_iter()
            .filter(|entry| is_archive_ext(entry.extension.as_deref().unwrap_or("")))
            .collect()
    }

    fn archive_extract_dest_for(&self, archive_name: &str) -> String {
        keep_both_destination(
            &PathBuf::from(self.active_directory()).join(
                Path::new(archive_name)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            ),
        )
        .to_string_lossy()
        .to_string()
    }

    fn start_archive_extract_async(
        &mut self,
        ui: &MainWindow,
        source: String,
        dest: String,
        selected: Vec<String>,
        password: Option<String>,
        conflict: String,
    ) {
        self.start_bulk_archive_extract_async(
            ui,
            vec![ArchiveExtractJob {
                source,
                dest,
                selected,
            }],
            password,
            conflict,
        );
    }

    fn start_bulk_archive_extract_async(
        &mut self,
        ui: &MainWindow,
        jobs: Vec<ArchiveExtractJob>,
        password: Option<String>,
        conflict: String,
    ) {
        if jobs.is_empty() {
            return;
        }
        let label = if jobs.len() == 1 {
            Path::new(&jobs[0].source)
                .file_name()
                .map(|name| format!("Extracting {}", name.to_string_lossy()))
                .unwrap_or_else(|| "Extracting archive".to_string())
        } else {
            format!("Extracting {} archives", jobs.len())
        };
        ui.set_op_drawer_text(ss(label));
        ui.set_op_drawer_visible(true);
        let dual_pane = ui.get_dual_pane();
        let dest_is_secondary = self.active_pane == ActivePane::Secondary;
        let state = self.app_state.clone();
        let ready = self.operation_ready.clone();
        let pending = self.pending_operation_result.clone();
        std::thread::spawn(move || {
            let mut completed = 0usize;
            let mut first_error: Option<String> = None;
            let mut last_dest = String::new();

            for job in &jobs {
                last_dest = job.dest.clone();
                match extract_archive_impl(
                    &state,
                    &job.source,
                    &job.dest,
                    &job.selected,
                    password.as_deref(),
                    &conflict,
                ) {
                    Ok(()) => completed += 1,
                    Err(error) => {
                        first_error = Some(error);
                        break;
                    }
                }
            }

            let pasted = completed > 0;
            let result = if let Some(error) = first_error {
                NativeOperationResult {
                    message: if completed > 0 {
                        format!("Extracted {completed} of {}: {error}", jobs.len())
                    } else {
                        error
                    },
                    kind: "error".to_string(),
                    refresh: pasted && !dual_pane && !dest_is_secondary,
                    refresh_both_panes: pasted && dual_pane,
                    secondary_refresh_path: if pasted && !dual_pane && dest_is_secondary {
                        Some(last_dest)
                    } else {
                        None
                    },
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                }
            } else {
                NativeOperationResult {
                    message: if jobs.len() == 1 {
                        format!("Extracted to {last_dest}")
                    } else {
                        format!("Extracted {completed} archives")
                    },
                    kind: "success".to_string(),
                    refresh: pasted && !dual_pane && !dest_is_secondary,
                    refresh_both_panes: pasted && dual_pane,
                    secondary_refresh_path: if pasted && !dual_pane && dest_is_secondary {
                        Some(last_dest)
                    } else {
                        None
                    },
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                }
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(result);
            }
            ready.store(true, Ordering::Release);
        });
    }

    fn extract_selected_archive(&mut self, ui: &MainWindow) {
        if let Some(archive) = self.active_archive.clone() {
            let selected = self.selected_archive_items();
            let selected = if selected.is_empty() && !archive.prefix.is_empty() {
                vec![archive.prefix.clone()]
            } else {
                selected
            };
            let dest = keep_both_destination(
                &PathBuf::from(&archive.return_path).join(
                    Path::new(&archive.archive_path)
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                ),
            );
            let dest = dest.to_string_lossy().to_string();
            let job = ArchiveExtractJob {
                source: archive.archive_path,
                dest,
                selected,
            };
            if archive_has_encrypted_entries(&job.source) {
                self.pending_prompt = Some(PendingPrompt::ArchivePassword {
                    jobs: vec![job],
                    conflict: "keep".to_string(),
                });
                ui.set_prompt_title(ss("Archive password"));
                ui.set_prompt_value(ss(""));
                ui.set_prompt_visible(true);
            } else {
                self.start_bulk_archive_extract_async(ui, vec![job], None, "keep".to_string());
            }
            return;
        }

        let archives = self.selected_disk_archives();
        if archives.is_empty() {
            self.show_toast(ui, "Select one or more archives.");
            return;
        }

        let jobs: Vec<ArchiveExtractJob> = archives
            .iter()
            .map(|entry| ArchiveExtractJob {
                source: entry.path.clone(),
                dest: self.archive_extract_dest_for(&entry.name),
                selected: Vec::new(),
            })
            .collect();

        let encrypted_count = jobs
            .iter()
            .filter(|job| archive_has_encrypted_entries(&job.source))
            .count();
        if encrypted_count > 0 {
            self.pending_prompt = Some(PendingPrompt::ArchivePassword {
                jobs,
                conflict: "keep".to_string(),
            });
            ui.set_prompt_title(ss(if encrypted_count == 1 {
                "Archive password".to_string()
            } else {
                format!(
                    "Archive password ({} encrypted)",
                    encrypted_count
                )
            }));
            ui.set_prompt_value(ss(""));
            ui.set_prompt_visible(true);
        } else {
            self.start_bulk_archive_extract_async(ui, jobs, None, "keep".to_string());
        }
    }

    fn create_archive_from_selection(&mut self, ui: &MainWindow, format: &str) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            self.show_toast(ui, "Select files first.");
            return;
        }
        let dest_dir = self.active_directory().to_string();
        let dest_is_secondary = self.active_pane == ActivePane::Secondary;
        let dest = keep_both_destination(&PathBuf::from(&dest_dir).join(match format {
            "7z" => "Archive.7z",
            "tar.gz" => "Archive.tar.gz",
            _ => "Archive.zip",
        }));
        ui.set_op_drawer_text(ss(format!("Creating {}", dest.display())));
        ui.set_op_drawer_visible(true);
        let state = self.app_state.clone();
        let ready = self.operation_ready.clone();
        let pending = self.pending_operation_result.clone();
        let dest_string = dest.to_string_lossy().to_string();
        std::thread::spawn(move || {
            let result = match create_archive_impl(&state, &paths, &dest_string) {
                Ok(()) => NativeOperationResult {
                    message: format!("Created {}", dest_string),
                    kind: "success".to_string(),
                    refresh: !dest_is_secondary,
                    refresh_both_panes: false,
                    secondary_refresh_path: if dest_is_secondary {
                        Some(dest_dir.clone())
                    } else {
                        None
                    },
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                },
                Err(error) => NativeOperationResult {
                    message: error,
                    kind: "error".to_string(),
                    refresh: false,
                    refresh_both_panes: false,
                    secondary_refresh_path: None,
                    clear_clipboard: false,
                    invalidate_dirs: Vec::new(),
                },
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some(result);
            }
            ready.store(true, Ordering::Release);
        });
    }

    fn prompt_compare_folder(&mut self, ui: &MainWindow) {
        self.pending_prompt = Some(PendingPrompt::CompareFolder(
            self.active_directory().to_string(),
        ));
        ui.set_prompt_title(ss("Compare with folder"));
        ui.set_prompt_value(ss(""));
        ui.set_prompt_visible(true);
    }

    fn show_rules(&mut self, ui: &MainWindow) {
        let rules = load_automation_rules();
        let items: Vec<ToolListItem> = rules
            .into_iter()
            .enumerate()
            .map(|(i, r)| ToolListItem {
                id: ss(i.to_string()),
                title: ss(&r.name),
                subtitle: ss(format!(
                    "ext:{} tag:{} · {}",
                    r.extension,
                    r.tag,
                    if r.folder.is_empty() {
                        "(no folder)".to_string()
                    } else {
                        r.folder.clone()
                    }
                )),
                meta: ss(""),
                enabled: r.enabled,
                accent: if r.enabled {
                    color("#58c77a")
                } else {
                    color("#888888")
                },
            })
            .collect();
        ui.set_tool_overlay_kind(ss("rules"));
        ui.set_tool_overlay_title(ss("Rules and Automation"));
        ui.set_tool_overlay_subtitle(ss(
            "Toggle rules on, then Run enabled. Moves stay opt-in and only run when you ask.",
        ));
        ui.set_tool_overlay_items(model_from_vec(items));
        ui.set_tool_overlay_visible(true);
    }

    fn show_shortcuts(&mut self, ui: &MainWindow) {
        self.shortcut_draft = load_shortcut_overrides();
        let items: Vec<ShortcutEditItem> = ALL_COMMANDS
            .iter()
            .map(|(group, label, hint, command)| ShortcutEditItem {
                command: ss(*command),
                label: ss(*label),
                hint: ss(shortcut_hint_for(command, hint)),
                group: ss(*group),
            })
            .collect();
        ui.set_tool_overlay_kind(ss("shortcuts"));
        ui.set_tool_overlay_title(ss("Shortcut Editor"));
        ui.set_tool_overlay_subtitle(ss(
            "Edit the displayed chord hints. Defaults remain until you Save.",
        ));
        ui.set_shortcut_edit_items(model_from_vec(items));
        ui.set_tool_overlay_visible(true);
    }

    fn show_workspaces_overlay(&mut self, ui: &MainWindow, save_mode: bool) {
        if save_mode {
            self.pending_prompt = Some(PendingPrompt::SaveWorkspace);
            ui.set_prompt_kind(ss("text"));
            ui.set_prompt_title(ss("Save workspace as"));
            ui.set_prompt_value(ss("My workspace"));
            ui.set_prompt_visible(true);
            return;
        }
        let items: Vec<ToolListItem> = load_workspaces()
            .into_iter()
            .map(|w| ToolListItem {
                id: ss(&w.name),
                title: ss(&w.name),
                subtitle: ss(format!("{} tabs", w.tabs.len())),
                meta: ss(""),
                enabled: true,
                accent: color("#4f9cff"),
            })
            .collect();
        ui.set_tool_overlay_kind(ss("workspaces"));
        ui.set_tool_overlay_title(ss("Open Workspace"));
        ui.set_tool_overlay_subtitle(ss(if items.is_empty() {
            "No saved workspaces yet. Use Save Workspace first."
        } else {
            "Click a workspace to restore its tabs."
        }));
        ui.set_tool_overlay_items(model_from_vec(items));
        ui.set_tool_overlay_visible(true);
    }

    fn push_compare_overlay(&mut self, ui: &MainWindow) {
        let hide_same = self.compare_hide_same;
        let items: Vec<CompareRowItem> = self
            .compare_all_rows
            .iter()
            .filter(|r| !(hide_same && r.status == "same"))
            .take(500)
            .map(|r| CompareRowItem {
                path: ss(&r.path),
                status: ss(&r.status),
                left_size: ss(format_size_short(r.left_size)),
                right_size: ss(format_size_short(r.right_size)),
            })
            .collect();
        ui.set_compare_overlay_title(ss("Folder Compare"));
        ui.set_compare_overlay_left(ss(&self.compare_left));
        ui.set_compare_overlay_right(ss(&self.compare_right));
        ui.set_compare_overlay_hide_same(self.compare_hide_same);
        ui.set_compare_rows(model_from_vec(items));
        ui.set_compare_overlay_visible(true);
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
        let op_queue = self
            .app_state
            .operation_queue
            .lock()
            .map(|q| q.len())
            .unwrap_or(0);
        let op_queue_paused = self.app_state.queue_is_paused();
        let battery_ok = indexing_permitted();
        ui.set_preview_title(ss("Performance Debug"));
        ui.set_preview_body(ss(format!(
            "Index mode: {}\n\
             Indexed files: {}\n\
             Index size: {}\n\
             Thumbnail cache: {} / {}\n\
             Directory cache: {} entries\n\
             Preview cache: {} entries\n\
             Active watchers: {} / 8\n\
             Operation queue: {} items{}\n\
             Background indexing: {}\n\
             Current folder: {} items\n\
             Search mode: {}\n\
             Roots:\n{}",
            status.mode,
            status.indexed_files,
            format_size_short(status.index_bytes),
            format_size_short(status.thumbnail_bytes),
            format_size_short(status.thumbnail_limit),
            dir_cache,
            preview_cache,
            watchers,
            op_queue,
            if op_queue_paused { " (paused)" } else { "" },
            if battery_ok {
                "permitted"
            } else {
                "paused (low battery)"
            },
            self.visible_files.len(),
            if self.search_query.is_empty() {
                "browsing"
            } else {
                "filtered"
            },
            if status.roots.is_empty() {
                "Visited folders only".to_string()
            } else {
                status.roots.join("\n")
            }
        )));
        ui.set_preview_meta(ss(status.estimated_storage));
    }

    fn storage_default_root(&self) -> String {
        // C: drive on Windows; on other OSes fall back to the user's home dir.
        #[cfg(target_os = "windows")]
        {
            "C:\\".to_string()
        }
        #[cfg(not(target_os = "windows"))]
        {
            dirs::home_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string())
        }
    }

    fn storage_result(&self) -> Option<&StorageScanResult> {
        self.storage_caches.get(&self.storage_current_root)
    }

    fn storage_result_clone(&self) -> Option<StorageScanResult> {
        self.storage_caches
            .get(&self.storage_current_root)
            .cloned()
    }

    fn open_storage_view(&mut self, ui: &MainWindow, push_history: bool) {
        if self.current_path != "storage://" {
            self.storage_path_before = self.current_path.clone();
            // Storage owns the preview pane while it is active. Save the
            // user's prior preview state once on entry so we can restore it
            // when they leave storage entirely, then hide the pane so the
            // overview grid has enough width to fit every bucket.
            self.storage_preview_visible_before = ui.get_preview_visible();
            self.storage_preview_w_before = ui.get_preview_w_user();
        }
        // The overview itself doesn't need a drill-in open. Reset the
        // bucket selection / show-all flags so re-entering Storage
        // always lands on the overview, not on whatever drill-in the
        // user had open last time.
        self.storage_selected_bucket.clear();
        ui.set_storage_selected_bucket(ss(""));
        ui.set_storage_selected_bucket_name(ss(""));
        self.storage_show_all_state = false;
        self.current_path = "storage://".to_string();
        ui.set_in_recycle_bin(false);
        ui.set_is_storage_view(true);
        ui.set_preview_visible(false);
        ui.set_storage_show_all(false);
        self.clear_selection(ui);
        if push_history {
            self.history.truncate(self.history_index + 1);
            self.history.push("storage://".to_string());
            self.history_index = self.history.len().saturating_sub(1);
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.path = "storage://".to_string();
        }
        self.push_drive_choices(ui);
        if self.storage_current_root.is_empty() {
            self.storage_current_root = self.storage_default_root();
        }
        // Cache is persistent across opens - only an explicit Rescan kicks a
        // new scan. Was 5-minute staleness in v0.9.0; users found that
        // surprising because the data they were looking at could just
        // disappear when reopening the tab.
        if let Some(result) = self.storage_result_clone() {
            self.push_storage_to_ui(ui, &result);
        } else {
            ui.set_storage_root(ss(&self.storage_current_root));
            ui.set_storage_total_text(ss("Preparing scan..."));
            ui.set_storage_subtitle(ss(""));
            self.storage_subtitle_last_update = Instant::now();
            self.start_storage_scan(ui);
        }
        self.update_models(ui);
        ui.set_side_items(model_from_vec(self.side_items()));
    }

    fn close_storage_view(&mut self, ui: &MainWindow) {
        ui.set_is_storage_view(false);
        // Restore the preview pane to whatever the user had before
        // opening storage (visibility + width). The drill-in widened it
        // to 640px while open; revert here so the user's normal layout
        // comes back unchanged.
        ui.set_preview_visible(self.storage_preview_visible_before);
        if self.storage_preview_w_before > 0.0 {
            ui.set_preview_w_user(self.storage_preview_w_before);
        }
        // Drop any drill-in state so the next open lands on the
        // overview cleanly.
        self.storage_selected_bucket.clear();
        ui.set_storage_selected_bucket(ss(""));
        ui.set_storage_selected_bucket_name(ss(""));
        self.storage_show_all_state = false;
        ui.set_storage_show_all(false);
        self.storage_drill_search.clear();
        ui.set_storage_drill_search(ss(""));
        // Cancel any in-flight scan so it stops burning CPU/disk when the
        // user has already moved on. The scan thread checks the cancelled
        // flag every batch and bails out early.
        if self.storage_scan_active {
            self.storage_progress
                .cancelled
                .store(true, Ordering::Relaxed);
            // Let Rescan work immediately after leaving Storage instead of
            // waiting for the background poll to clear the active flag.
            self.storage_scan_active = false;
            ui.set_storage_scanning(false);
        }
    }

    fn push_drive_choices(&self, ui: &MainWindow) {
        // Drive picker buttons in the Storage header. Filter to fixed +
        // removable (skip CD-ROM / RAM disks / cloud / WSL) so the picker
        // stays focused on actual local storage.
        let choices: Vec<StorageEntryUi> = self
            .drives
            .iter()
            .filter(|d| d.kind == "local" || d.kind == "removable")
            .map(|d| {
                let label = if d.name.is_empty() {
                    d.path.clone()
                } else {
                    d.name.clone()
                };
                let (bytes_text, bar_pct) = drive_free_space(&d.path)
                    .map(|(free, total)| {
                        let used_pct = if total > 0 {
                            ((total.saturating_sub(free)) as f64 / total as f64 * 100.0) as f32
                        } else {
                            0.0
                        };
                        (format!("{} free", format_size_short(free)), used_pct)
                    })
                    .unwrap_or_else(|| (String::new(), 0.0));
                StorageEntryUi {
                    name: ss(label),
                    path: ss(&d.path),
                    bytes_text: ss(bytes_text),
                    modified_text: ss(""),
                    bucket: ss(if d.kind == "local" {
                        "Fixed"
                    } else {
                        "Removable"
                    }),
                    is_dir: true,
                    bar_pct,
                    bucket_color: bucket_color_for("other"),
                }
            })
            .collect();
        ui.set_storage_drives(slint::ModelRc::new(slint::VecModel::from(choices)));
    }

    fn start_storage_scan(&mut self, ui: &MainWindow) {
        let root = self.storage_current_root.clone();
        if self.storage_scan_active {
            self.storage_progress
                .cancelled
                .store(true, Ordering::Relaxed);
        }
        ui.set_storage_root(ss(&root));
        ui.set_storage_scanning(true);
        ui.set_storage_progress_files(0);
        ui.set_storage_progress_bytes_text(ss("0 B"));
        ui.set_storage_progress_percent(0.0);
        ui.set_storage_list_total(0);
        ui.set_storage_list_shown(0);
        ui.set_storage_breakdown_text(ss("Scanning drive..."));
        // % is computed against a real denominator. drive_free_space returns
        // (free_to_caller, total_bytes); used = total - free.
        self.storage_disk_used = drive_free_space(&root)
            .map(|(free, total)| total.saturating_sub(free))
            .unwrap_or(0);
        // Fresh progress object so a previous scan's cancelled flag doesn't
        // poison this one.
        self.storage_progress = Arc::new(StorageScanProgress::default());
        self.storage_scan_active = true;
        let generation = self.storage_scan_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let pending = self.storage_scan_pending.clone();
        let ready = self.storage_scan_ready.clone();
        let progress = self.storage_progress.clone();
        std::thread::spawn(move || {
            let result = scan_storage_with_progress(Path::new(&root), 250, Some(progress.clone()));
            let result = if progress.cancelled.load(Ordering::Relaxed) {
                None
            } else {
                Some(result)
            };
            if let Ok(mut lock) = pending.lock() {
                *lock = Some((generation, result));
            }
            ready.store(true, Ordering::Release);
        });
    }

    fn rescan_storage(&mut self, ui: &MainWindow) {
        if self.storage_scan_active {
            self.show_toast_kind(ui, "Storage scan is already running.", "info");
            return;
        }
        // v0.9.18: the Rescan button now ALWAYS triggers a full
        // rewalk. Previously a signature-match fast-path skipped the
        // walker entirely - but the signature only tracks top-level
        // subdir mtimes, so an uninstaller that leaves stale folders
        // behind (Ubisoft Connect, leftover game caches, mod
        // directories) wouldn't shift the signature even though the
        // actual contents had changed. Cache reuse still happens on
        // app-launch / storage-view-reopen via the persisted cache;
        // it's only the explicit Rescan affordance that's been
        // tightened. Quick signature-only mode is no longer surfaced
        // - if users want it back we can add a separate "Reload from
        // cache" button.
        self.storage_caches.remove(&self.storage_current_root);
        self.start_storage_scan(ui);
    }

    fn storage_cleanup_entries_for_action(&self, action: &str) -> Vec<StorageEntry> {
        let Some(result) = self.storage_result() else {
            return Vec::new();
        };
        let now = now_unix_secs() as i64;
        let mut entries: Vec<StorageEntry> = match action {
            "cleanup:old-downloads" => collect_storage_tip_entries(result)
                .into_iter()
                .filter(|e| {
                    e.modified > 0
                        && now.saturating_sub(e.modified) > 90 * 86_400
                        && entry_path_has_marker(e, &["\\downloads\\", "/downloads/"])
                })
                .take(40)
                .collect(),
            "cleanup:stale" => collect_storage_tip_entries(result)
                .into_iter()
                .filter(|e| {
                    e.modified > 0
                        && now.saturating_sub(e.modified) > 365 * 86_400
                        && !entry_is_system_reserved(e)
                        && !entry_path_has_marker(e, &["\\windows\\", "/windows/"])
                })
                .take(40)
                .collect(),
            _ => Vec::new(),
        };
        entries.sort_unstable_by_key(|e| std::cmp::Reverse(e.bytes));
        entries
    }

    fn prepare_storage_tip_action(&mut self, ui: &MainWindow, action: String, _path: String) {
        if action == "duplicates" {
            if let Some(result) = self.storage_result() {
                ui.set_preview_visible(true);
                ui.set_preview_title(ss("Duplicate Finder"));
                ui.set_preview_body(ss(format!(
                    "{} large duplicate file{} across {} group{}.\nEstimated reclaimable: {}\n\nOpen the drive root and run Find Duplicates when you are ready to choose which copies to keep.",
                    result.duplicate_count,
                    if result.duplicate_count == 1 { "" } else { "s" },
                    result.duplicate_groups,
                    if result.duplicate_groups == 1 { "" } else { "s" },
                    format_size_short(result.duplicate_reclaimable_bytes)
                )));
                ui.set_preview_meta(ss(format!(
                    "Minimum duplicate size: {}",
                    format_size_short(STORAGE_DUPLICATE_MIN_SIZE)
                )));
            }
            return;
        }

        let mut entries = self.storage_cleanup_entries_for_action(&action);
        if entries.is_empty() {
            self.show_toast_kind(ui, "No cleanup items found for this tip", "info");
            return;
        }
        entries.sort_unstable_by_key(|e| e.path.len());
        let mut paths: Vec<String> = Vec::new();
        let mut cleanup_total = 0u64;
        for entry in entries {
            if paths
                .iter()
                .any(|parent| path_is_strict_parent(parent, &entry.path) || parent == &entry.path)
            {
                continue;
            }
            cleanup_total = cleanup_total.saturating_add(entry.bytes);
            paths.push(entry.path);
        }
        if paths.is_empty() {
            self.show_toast_kind(ui, "No cleanup items found for this tip", "info");
            return;
        }

        let preview = paths
            .iter()
            .take(8)
            .map(|path| {
                Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone())
            })
            .collect::<Vec<_>>()
            .join("\n");
        let n = paths.len();
        self.pending_prompt = Some(PendingPrompt::StorageCleanup(paths));
        let question = if n == 1 {
            "Delete this item to the Recycle Bin?".to_string()
        } else {
            format!("Delete these {n} items to the Recycle Bin?")
        };
        ui.set_confirm_text(ss(format!(
            "{}\n\nEstimated reclaimable: {}\n\n{}{}",
            question,
            format_size_short(cleanup_total),
            preview,
            if n > 8 { "\n..." } else { "" }
        )));
        ui.set_confirm_visible(true);
    }

    fn switch_storage_root(&mut self, ui: &MainWindow, new_root: String) {
        if new_root.is_empty() || new_root == self.storage_current_root {
            return;
        }
        // Cancel any in-flight scan for the old root.
        if self.storage_scan_active {
            self.storage_progress
                .cancelled
                .store(true, Ordering::Relaxed);
        }
        self.storage_current_root = new_root;
        self.storage_selected_bucket.clear();
        ui.set_storage_selected_bucket(ss(""));
        ui.set_storage_selected_bucket_name(ss(""));
        self.storage_show_all_state = false;
        ui.set_storage_show_all(false);
        ui.set_preview_visible(false);
        // Cached result for the new root?
        if let Some(result) = self.storage_result_clone() {
            self.push_storage_to_ui(ui, &result);
        } else {
            ui.set_storage_root(ss(&self.storage_current_root));
            ui.set_storage_total_text(ss("Preparing scan..."));
            ui.set_storage_subtitle(ss(""));
            self.storage_subtitle_last_update = Instant::now();
            self.start_storage_scan(ui);
        }
    }

    /// Renders either the global top-N or the per-bucket top-N depending on
    /// which mode the UI is in. Called whenever cache, selected bucket, or
    /// show-all toggle changes.
    ///
    /// Fallback ordering when a bucket is selected:
    ///   1. Per-bucket top-N (populated by the scanner's bounded min-heaps)
    ///   2. If empty, filter the global top_items list by bucket id
    ///   3. If still empty, the UI shows the empty-state message
    ///
    /// Step 2 catches the case where the per-bucket heap didn't accumulate
    /// (e.g. scan finished but heaps were unlucky) but the global top-N
    /// still has entries for that bucket.
    fn push_storage_top_items(&self, ui: &MainWindow, result: &StorageScanResult) {
        let entries_src: Vec<StorageEntry> =
            if self.storage_show_all_state || self.storage_selected_bucket.is_empty() {
                refine_storage_largest_list(result.top_items.clone())
            } else {
                // Drill-in: merge folder roll-ups + individual files so
                // users see EVERY app/folder/file in the bucket, sorted
                // by size. Folders go through refine first to drop
                // nested duplicates; files are then appended unless a
                // refined folder already contains them. v0.9.11 stopped
                // gating on "folders.is_empty()" - users complained
                // that buckets with a handful of refined folders showed
                // only ~5 rows when there were dozens of standalone
                // files worth listing alongside them.
                let folders_raw: Vec<StorageEntry> = result
                    .bucket_folder_items
                    .get(&self.storage_selected_bucket)
                    .cloned()
                    .unwrap_or_default();
                let refined_folders =
                    refine_storage_drill_folders(&self.storage_selected_bucket, folders_raw);
                let files: Vec<StorageEntry> = result
                    .bucket_items
                    .get(&self.storage_selected_bucket)
                    .cloned()
                    .unwrap_or_default();
                let mut merged: Vec<StorageEntry> = refined_folders.clone();
                for f in files {
                    if self.storage_selected_bucket == "apps"
                        && is_storage_apps_drill_noise(&f.path)
                    {
                        continue;
                    }
                    let contained = refined_folders.iter().any(|fldr| {
                        path_is_strict_parent(&fldr.path, &f.path) || fldr.path == f.path
                    });
                    if !contained {
                        merged.push(f);
                    }
                }
                // Last-resort fallback: filter top_items if both heaps
                // produced nothing (rare for tiny buckets).
                if merged.is_empty() {
                    merged = refine_storage_largest_list(
                        result
                            .top_items
                            .iter()
                            .filter(|e| {
                                storage_canonical_bucket(&e.bucket)
                                    == self.storage_selected_bucket
                            })
                            .cloned()
                            .collect(),
                    );
                }
                sort_storage_drill_entries(&self.storage_selected_bucket, &mut merged);
                merged
            };
        // v0.9.14: search filter applied AFTER sorting + bucket
        // selection so the user's "show me everything in this bucket"
        // input narrows what's already there, never widens the scope.
        let search_lower = self.storage_drill_search.trim().to_lowercase();
        let mut filtered: Vec<StorageEntry> = if search_lower.is_empty() {
            entries_src
        } else {
            entries_src
                .into_iter()
                .filter(|e| {
                    e.name.to_lowercase().contains(&search_lower)
                        || e.path.to_lowercase().contains(&search_lower)
                })
                .collect()
        };
        // Apply the user's chosen sort mode (size/name/recent). Default "size"
        // keeps the size ranking + apps game-launcher priority.
        apply_storage_sort_mode(
            &self.storage_sort_mode,
            &self.storage_selected_bucket,
            &mut filtered,
        );
        // Share bar is relative to the LARGEST item, computed independently of
        // the current sort order so the bar stays meaningful under name/recent.
        let largest = filtered.iter().map(|e| e.bytes).max().unwrap_or(0).max(1);
        let total_count = filtered.len();
        let limit = if self.storage_selected_bucket.is_empty() && !self.storage_show_all_state {
            STORAGE_TOP_ITEMS_LIMIT
        } else {
            STORAGE_BUCKET_DRILL_LIMIT
        };
        let shown_count = total_count.min(limit);
        let entries: Vec<StorageEntryUi> = filtered
            .into_iter()
            .take(limit)
            .map(|e| {
                let (title, path_line) = storage_entry_display_labels(&e);
                StorageEntryUi {
                    name: ss(&title),
                    path: ss(&path_line),
                    bytes_text: ss(format_size_short(e.bytes)),
                    modified_text: ss(format_storage_entry_age(e.modified)),
                    bucket: ss(bucket_display_name(&e.bucket)),
                    is_dir: e.is_dir,
                    bar_pct: ((e.bytes as f64 / largest as f64) * 100.0) as f32,
                    bucket_color: bucket_color_for(&e.bucket),
                }
            })
            .collect();
        ui.set_storage_list_total(total_count as i32);
        ui.set_storage_list_shown(shown_count as i32);
        ui.set_storage_top_items(slint::ModelRc::new(slint::VecModel::from(entries)));
    }

    fn push_storage_to_ui(&mut self, ui: &MainWindow, result: &StorageScanResult) {
        let total = result.total_bytes.max(1);
        // Order the cards + breakdown bar by size so the biggest categories
        // read first (left to right). "Other" is pinned last regardless of
        // size - it's a catch-all, not an actionable category.
        let mut sorted_buckets: Vec<&StorageBucket> = result.buckets.iter().collect();
        sorted_buckets.sort_by(|a, b| {
            let a_other = a.id == "other";
            let b_other = b.id == "other";
            a_other
                .cmp(&b_other)
                .then_with(|| b.bytes.cmp(&a.bytes))
        });
        let buckets: Vec<StorageBucketUi> = sorted_buckets
            .iter()
            .map(|b| {
                let pct = (b.bytes as f64 / total as f64) * 100.0;
                StorageBucketUi {
                    id: ss(&b.id),
                    name: ss(&b.name),
                    bytes_text: ss(format_size_short(b.bytes)),
                    file_count_text: ss(format!("{} files", b.file_count)),
                    percent: pct as f32,
                    icon: ss(&b.icon),
                    color: parse_hex_color(&b.color),
                }
            })
            .collect();
        ui.set_storage_root(ss(&result.root));
        ui.set_storage_total_text(ss(format!(
            "{} used across {} files",
            format_size_short(result.total_bytes),
            result.scanned_files
        )));
        ui.set_storage_subtitle(ss(format!(
            "8 categories · scanned {} ago in {:.1}s",
            format_relative_time(result.scanned_at),
            (result.elapsed_ms as f64) / 1000.0
        )));
        self.storage_subtitle_last_update = Instant::now();
        ui.set_storage_buckets(slint::ModelRc::new(slint::VecModel::from(buckets)));
        self.push_storage_top_items(ui, result);
        // Disk-wide totals + scanned vs unaccounted breakdown.
        if let Some((free, disk_total)) = drive_free_space(&result.root) {
            let used = disk_total.saturating_sub(free);
            let pct = if disk_total > 0 {
                (used as f64 / disk_total as f64) * 100.0
            } else {
                0.0
            };
            ui.set_storage_disk_summary(ss(format!(
                "{} used of {}  ·  {} free",
                format_size_short(used),
                format_size_short(disk_total),
                format_size_short(free)
            )));
            ui.set_storage_disk_used_pct(pct as f32);
            let unaccounted = used.saturating_sub(result.total_bytes);
            let coverage = if used > 0 {
                (result.total_bytes as f64 / used as f64) * 100.0
            } else {
                100.0
            };
            ui.set_storage_scan_coverage_pct(coverage as f32);
            let breakdown = if unaccounted > 512 * 1024 * 1024 {
                format!(
                    "{} scanned · {} system/other (Windows, pagefile, skipped folders)",
                    format_size_short(result.total_bytes),
                    format_size_short(unaccounted)
                )
            } else if unaccounted > 64 * 1024 * 1024 {
                format!(
                    "{} scanned · {} unaccounted (system files, skipped paths)",
                    format_size_short(result.total_bytes),
                    format_size_short(unaccounted)
                )
            } else {
                format!(
                    "{} scanned across {} files",
                    format_size_short(result.total_bytes),
                    result.scanned_files
                )
            };
            ui.set_storage_breakdown_text(ss(breakdown));
        } else {
            ui.set_storage_disk_summary(ss(""));
            ui.set_storage_disk_used_pct(0.0);
            ui.set_storage_scan_coverage_pct(100.0);
            ui.set_storage_breakdown_text(ss(format!(
                "{} scanned across {} files",
                format_size_short(result.total_bytes),
                result.scanned_files
            )));
        }
        ui.set_storage_scanning(false);
        ui.set_storage_progress_files(result.scanned_files as i32);
        ui.set_storage_progress_bytes_text(ss(format_size_short(result.total_bytes)));
        ui.set_storage_progress_percent(100.0);
        let tips: Vec<StorageTipUi> = build_storage_tips(result)
            .into_iter()
            .take(STORAGE_TIPS_LIMIT)
            .collect();
        ui.set_storage_tips(slint::ModelRc::new(slint::VecModel::from(tips)));
    }

    /// Pump progress counters from the live scan into Slint properties. Cheap:
    /// two relaxed atomic loads per tick. Also pumps a live "scanned X ago"
    /// string update for the cached-but-static case so the subtitle ticks
    /// while the storage view is visible.
    fn pump_storage_progress(&mut self, ui: &MainWindow) {
        if !ui.get_is_storage_view() && !self.storage_scan_active {
            return;
        }
        // Tick the "scanned X ago" subtitle while the storage view is open
        // and there's a cached result. Keep this coarse; doing string format
        // and property writes on every 100ms poll made the idle storage view
        // do needless UI work.
        if ui.get_is_storage_view()
            && !self.storage_scan_active
            && self.storage_subtitle_last_update.elapsed() >= Duration::from_secs(15)
        {
            if let Some(cached) = self.storage_result() {
                ui.set_storage_subtitle(ss(format!(
                    "Scanned {} ago in {:.1}s - click any bucket to drill in",
                    format_relative_time(cached.scanned_at),
                    (cached.elapsed_ms as f64) / 1000.0
                )));
                self.storage_subtitle_last_update = Instant::now();
            }
        }
        if !self.storage_scan_active {
            return;
        }
        let files = self.storage_progress.files.load(Ordering::Relaxed);
        let bytes = self.storage_progress.bytes.load(Ordering::Relaxed);
        ui.set_storage_progress_files(files as i32);
        ui.set_storage_progress_bytes_text(ss(format_size_short(bytes)));
        // Progress denominator: used bytes from GetDiskFreeSpaceExW (queried
        // when the scan started). Real progress vs the drive's actual used
        // space instead of a moving baseline. Cap at 99 until the scan
        // actually returns so we don't sit at 100% with the bar still moving.
        let pct = if self.storage_disk_used > 0 {
            ((bytes as f64 / self.storage_disk_used as f64) * 100.0).min(99.0)
        } else {
            // No disk-used baseline (network drive, exotic FS) - show an
            // animated indeterminate-ish progress that grows slowly.
            (bytes as f64 / 1_073_741_824.0 * 5.0).min(95.0)
        };
        ui.set_storage_progress_percent(pct as f32);
    }

    fn poll_storage_scan(&mut self, ui: &MainWindow) {
        self.pump_storage_progress(ui);
        if !self.storage_scan_ready.swap(false, Ordering::AcqRel) {
            return;
        }
        let pending = {
            let mut lock = match self.storage_scan_pending.lock() {
                Ok(l) => l,
                Err(_) => return,
            };
            lock.take()
        };
        let Some((generation, result)) = pending else {
            return;
        };
        if generation != self.storage_scan_generation.load(Ordering::Acquire) {
            return;
        }
        self.storage_scan_active = false;
        if let Some(result) = result {
            self.storage_caches
                .insert(result.root.clone(), result.clone());
            let _ = write_storage_scan_cache_entry(&result);
            if ui.get_is_storage_view() {
                self.push_storage_to_ui(ui, &result);
            } else {
                ui.set_storage_scanning(false);
            }
        } else {
            ui.set_storage_scanning(false);
        }
    }

    fn select_storage_bucket(&mut self, ui: &MainWindow, bucket_id: String) {
        // Reset the search filter when switching buckets so a query
        // typed in one category doesn't silently narrow the next.
        self.storage_drill_search.clear();
        ui.set_storage_drill_search(ss(""));
        // Reset sort to size so each drill-in opens on the biggest items.
        self.storage_sort_mode = "size".to_string();
        ui.set_storage_sort_mode(ss("size"));
        self.storage_selected_bucket = bucket_id.clone();
        ui.set_storage_selected_bucket(ss(&bucket_id));
        let name = self
            .storage_result()
            .and_then(|r| r.buckets.iter().find(|b| b.id == bucket_id))
            .map(|b| b.name.clone())
            .unwrap_or_else(|| bucket_display_name(&bucket_id).to_string());
        ui.set_storage_selected_bucket_name(ss(&name));
        if let Some(result) = self.storage_result_clone() {
            self.push_storage_top_items(ui, &result);
        }
    }

    fn clear_storage_bucket_filter(&mut self, ui: &MainWindow) {
        self.storage_selected_bucket.clear();
        ui.set_storage_selected_bucket(ss(""));
        ui.set_storage_selected_bucket_name(ss(""));
        // Also drop the "show all" full-list mode so closing the
        // drill-in returns the user to the bucket grid every time
        // (matches user mental model: X closes the drill-in).
        self.storage_show_all_state = false;
        ui.set_storage_show_all(false);
        self.storage_drill_search.clear();
        ui.set_storage_drill_search(ss(""));
        self.storage_sort_mode = "size".to_string();
        ui.set_storage_sort_mode(ss("size"));
        // Return to the overview with the preview pane hidden. The user's
        // original preview visibility/width is restored when storage closes.
        ui.set_preview_visible(false);
        if let Some(result) = self.storage_result_clone() {
            self.push_storage_top_items(ui, &result);
        }
    }

    fn show_privacy_storage(&mut self, ui: &MainWindow) {
        let info = privacy_storage_info_for_state(&self.app_state, &self.settings);
        let stored = info
            .stored_items
            .iter()
            .map(|item| {
                format!(
                    "{}: {} | {}",
                    item.label,
                    format_size_short(item.bytes),
                    item.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        ui.set_preview_title(ss("Privacy and Storage"));
        ui.set_preview_body(ss(format!(
            "{}\n\nData folder: {}\nCache folder: {}\nIndex: {}\nThumbnails: {} / {}\nMemory caches: {} folders, {} previews\nWatchers: {}\nUpdate checks: {}\nNetwork downloads: {}\nNetwork uploads: {}\n\nStored local data:\n{}",
            info.policy,
            info.data_dir,
            info.cache_dir,
            format_size_short(info.index_bytes),
            format_size_short(info.thumbnail_cache_bytes),
            format_size_short(info.thumbnail_cache_limit),
            info.directory_cache_entries,
            info.preview_cache_entries,
            info.watcher_count,
            if info.update_checks_enabled { "enabled" } else { "off" },
            if info.network_downloads_enabled { "explicit only" } else { "off" },
            if info.network_uploads_enabled { "enabled" } else { "off" },
            if stored.is_empty() { "No local metadata yet.".to_string() } else { stored }
        )));
        ui.set_preview_meta(ss("Use Clear Thumbnail Cache or Clear Local Caches to remove generated cache data without deleting your files."));
    }

    fn tool_overlay_activate(&mut self, ui: &MainWindow, id: String) {
        let kind = ui.get_tool_overlay_kind().to_string();
        match kind.as_str() {
            "smart-folders" => {
                ui.set_tool_overlay_visible(false);
                let query = smart_folders_for_path(&self.current_path)
                    .into_iter()
                    .find(|f| f.id == id)
                    .map(|f| f.query)
                    .unwrap_or_else(|| format!("smart:{id}"));
                if id == "old-downloads" {
                    if let Some(downloads) = dirs::download_dir() {
                        self.search_query = query;
                        ui.set_search_text(ss(&self.search_query));
                        self.navigate(ui, downloads.to_string_lossy().to_string(), true);
                        self.search(ui, self.search_query.clone());
                        return;
                    }
                }
                self.search_query = query;
                ui.set_search_text(ss(&self.search_query));
                self.search(ui, self.search_query.clone());
            }
            "templates" => {
                if let Some(template) = load_file_templates().into_iter().find(|t| t.name == id) {
                    ui.set_tool_overlay_visible(false);
                    self.pending_prompt = Some(PendingPrompt::NewTemplate(template.clone()));
                    ui.set_prompt_kind(ss("text"));
                    ui.set_prompt_title(ss(format!("New {} file", template.name)));
                    ui.set_prompt_value(ss(format!("New.{}", template.extension)));
                    ui.set_prompt_visible(true);
                }
            }
            "workspaces" => {
                ui.set_tool_overlay_visible(false);
                if let Some(ws) = load_workspaces().into_iter().find(|w| w.name == id) {
                    if ws.tabs.is_empty() {
                        self.show_toast(ui, "Workspace has no tabs.");
                        return;
                    }
                    self.tabs = ws.tabs;
                    self.active_tab = 0;
                    let path = self.tabs[0].path.clone();
                    self.navigate(ui, path, false);
                    self.update_models(ui);
                    self.show_toast_kind(ui, format!("Opened workspace '{id}'"), "success");
                }
            }
            "rules" => {
                // Activate = focus rule folder if present.
                if let Ok(idx) = id.parse::<usize>() {
                    if let Some(rule) = load_automation_rules().get(idx) {
                        if !rule.folder.is_empty() {
                            ui.set_tool_overlay_visible(false);
                            self.navigate(ui, rule.folder.clone(), true);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn tool_overlay_toggle(&mut self, ui: &MainWindow, id: String) {
        if ui.get_tool_overlay_kind().as_str() != "rules" {
            return;
        }
        let Ok(idx) = id.parse::<usize>() else {
            return;
        };
        let mut rules = load_automation_rules();
        if let Some(rule) = rules.get_mut(idx) {
            rule.enabled = !rule.enabled;
        }
        let _ = save_automation_rules(&rules);
        self.show_rules(ui);
    }

    fn tool_overlay_secondary(&mut self, ui: &MainWindow, id: String) {
        let kind = ui.get_tool_overlay_kind().to_string();
        if kind == "smart-folders" {
            self.pending_prompt = Some(PendingPrompt::RenameTag(format!("smart:{id}")));
            ui.set_prompt_kind(ss("text"));
            ui.set_prompt_title(ss("Rename smart folder"));
            let current = smart_folders_for_path(&self.current_path)
                .into_iter()
                .find(|f| f.id == id)
                .map(|f| f.name)
                .unwrap_or_default();
            ui.set_prompt_value(ss(current));
            ui.set_prompt_visible(true);
        }
    }

    fn tool_overlay_primary_action(&mut self, ui: &MainWindow) {
        let kind = ui.get_tool_overlay_kind().to_string();
        match kind.as_str() {
            "rules" => {
                let rules = load_automation_rules();
                match run_automation_rules_once(&rules, &mut self.tags) {
                    Ok((tagged, moved)) => {
                        let _ = write_native_json("tags.json", &self.tags);
                        self.sync_tag_names(ui);
                        self.update_models(ui);
                        ui.set_side_items(model_from_vec(self.side_items()));
                        self.show_toast_kind(
                            ui,
                            format!("Rules ran: tagged {tagged}, moved {moved}"),
                            "success",
                        );
                    }
                    Err(e) => self.show_toast_kind(ui, e, "error"),
                }
            }
            "shortcuts" => {
                match save_shortcut_overrides(&self.shortcut_draft) {
                    Ok(()) => {
                        ui.set_tool_overlay_visible(false);
                        self.show_toast_kind(ui, "Shortcuts saved", "success");
                    }
                    Err(e) => self.show_toast_kind(ui, e, "error"),
                }
            }
            "workspaces" => {
                // Open first selected isn't available; keep overlay for click-to-open.
                self.show_toast(ui, "Click a workspace to open it.");
            }
            _ => ui.set_tool_overlay_visible(false),
        }
    }

    fn shortcut_hint_changed(&mut self, command: String, hint: String) {
        if hint.trim().is_empty() {
            self.shortcut_draft.remove(&command);
        } else {
            self.shortcut_draft.insert(command, hint);
        }
    }

    fn dupe_group_keep(&mut self, ui: &MainWindow, id: String) {
        self.show_toast_kind(
            ui,
            format!(
                "Keeping first file in {}",
                id.split_once("::").map(|(_, t)| t).unwrap_or(&id)
            ),
            "success",
        );
    }

    fn dupe_group_delete_others(&mut self, ui: &MainWindow, id: String) {
        let Some((_, paths)) = self
            .dupe_groups_cache
            .iter()
            .find(|(k, _)| *k == id)
            .map(|(k, v)| (k.clone(), v.clone()))
        else {
            self.show_toast(ui, "Group not found.");
            return;
        };
        if paths.len() < 2 {
            self.show_toast(ui, "Nothing to delete.");
            return;
        }
        let mut deleted = 0usize;
        for path in paths.iter().skip(1) {
            match trash::delete(Path::new(path)) {
                Ok(()) => deleted += 1,
                Err(e) => {
                    self.show_toast_kind(ui, e.to_string(), "error");
                    break;
                }
            }
        }
        self.dupe_groups_cache.retain(|(k, _)| k != &id);
        let items: Vec<DupeGroupItem> = self
            .dupe_groups_cache
            .iter()
            .map(|(id_title, paths)| {
                let title = id_title
                    .split_once("::")
                    .map(|(_, t)| t.to_string())
                    .unwrap_or_else(|| id_title.clone());
                DupeGroupItem {
                    id: ss(id_title),
                    title: ss(&title),
                    detail: ss(format!("{} files", paths.len())),
                    count: paths.len() as i32,
                    reclaimable: ss(""),
                }
            })
            .collect();
        ui.set_dupe_groups(model_from_vec(items));
        self.refresh(ui);
        self.show_toast_kind(
            ui,
            format!("Moved {deleted} duplicate(s) to Recycle Bin"),
            "success",
        );
    }

    fn search_chip_insert(&mut self, ui: &MainWindow, op: String) {
        let mut text = ui.get_search_text().to_string();
        let token = match op.as_str() {
            "ext" => "ext:",
            "kind" => "kind:",
            "size" => "size:>",
            "modified" => "modified:",
            "tag" => "tag:",
            "content" => "content:",
            _ => return,
        };
        if !text.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(token);
        ui.set_search_text(ss(&text));
        self.search_query = text;
        ui.set_search_help_visible(true);
        ui.set_search_chip_hint(ss(match op.as_str() {
            "ext" => "Example: ext:pdf",
            "kind" => "Example: kind:image",
            "size" => "Example: size:>100mb",
            "modified" => "Example: modified:week",
            "tag" => "Example: tag:yellow",
            "content" => "Example: content:TODO",
            _ => "",
        }));
    }

    fn search_save_current(&mut self, ui: &MainWindow) {
        let query = ui.get_search_text().to_string();
        if query.trim().is_empty() {
            self.show_toast(ui, "Type a search first.");
            return;
        }
        let name = query.chars().take(40).collect::<String>();
        match native_save_search(name.clone(), query, self.current_path.clone()) {
            Ok(()) => {
                ui.set_side_items(model_from_vec(self.side_items()));
                self.show_toast_kind(ui, format!("Saved search '{name}'"), "success");
            }
            Err(e) => self.show_toast_kind(ui, e, "error"),
        }
    }

    fn sync_queue_busy(&self, ui: &MainWindow) {
        let (len, paused) = (
            self.app_state
                .operation_queue
                .lock()
                .map(|q| q.len())
                .unwrap_or(0),
            self.app_state.queue_is_paused(),
        );
        let active = ACTIVE_HEAVY_OPS.load(Ordering::SeqCst);
        let busy = len > 0 || active > 0;
        ui.set_queue_busy(busy);
        if busy {
            ui.set_queue_busy_text(ss(if paused {
                format!("Queue paused ({len})")
            } else if len > 0 {
                format!("{len} queued")
            } else {
                "Working…".to_string()
            }));
        } else {
            ui.set_queue_busy_text(ss(""));
        }
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
        let kind = op.kind.clone();
        let from = op.from.clone();
        let to = op.to.clone();
        let batch_len = op.batch.as_ref().map(|b| b.len()).unwrap_or(0);
        let result = match kind.as_str() {
            "batch_rename" => match op.batch {
                None => Err("Missing batch rename metadata".to_string()),
                Some(ops) => {
                    let mut outcome = Ok(());
                    for item in ops.iter().rev() {
                        let old_name = Path::new(&item.from)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| item.from.clone());
                        if let Err(error) = native_rename(&self.app_state, &item.to, &old_name) {
                            outcome = Err(error);
                            break;
                        }
                    }
                    outcome
                }
            },
            "rename" | "move" => {
                let from_path = to.as_deref().unwrap_or("");
                native_move(&self.app_state, from_path, &from)
            }
            "copy" => to
                .as_deref()
                .map(native_delete_path)
                .unwrap_or_else(|| Err("Missing copied path".to_string())),
            "delete" => undo_delete_from_trash(op.trash_id.as_deref()),
            _ => Err(format!("Cannot undo '{kind}'")),
        };
        match result {
            Ok(()) => {
                self.refresh(ui);
                let detail = match kind.as_str() {
                    "batch_rename" => {
                        format!(
                            "Undone: renamed {batch_len} file{}",
                            if batch_len == 1 { "" } else { "s" }
                        )
                    }
                    "rename" => {
                        let name = Path::new(&from)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| from.clone());
                        format!("Undone: rename → {name}")
                    }
                    "move" => {
                        let name = Path::new(&from)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| from.clone());
                        format!("Undone: moved {name}")
                    }
                    "copy" => {
                        let name = to
                            .as_deref()
                            .and_then(|p| {
                                Path::new(p)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                            })
                            .unwrap_or_else(|| "file".to_string());
                        format!("Undone: deleted copy of {name}")
                    }
                    "delete" => {
                        let name = Path::new(&from)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| from.clone());
                        format!("Undone: restored {name}")
                    }
                    other => format!("Undone: {other}"),
                };
                self.show_toast_kind(ui, detail, "success");
            }
            Err(error) => self.show_toast(ui, error),
        }
    }
}

fn undo_delete_from_trash(trash_id: Option<&str>) -> Result<(), String> {
    let trash_id = trash_id.ok_or("Missing recycle metadata for undo")?;
    let items = trash::os_limited::list().map_err(|e| e.to_string())?;
    let item = items
        .into_iter()
        .find(|i| i.id.to_string_lossy() == trash_id)
        .ok_or("Item is no longer in the Recycle Bin")?;
    trash::os_limited::restore_all(vec![item]).map_err(|e| e.to_string())
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
            let action = c.borrow_mut().side_activate_action(&ui, index);
            match action {
                SidebarActivateAction::Navigate(path) => {
                    c.borrow_mut()
                        .queue_sidebar_navigate(&ui, path, c.clone());
                }
                SidebarActivateAction::NavigateThenSearch { path, query } => {
                    c.borrow_mut()
                        .queue_sidebar_navigate(&ui, path, c.clone());
                    // Apply search after navigation commits on the next tick.
                    let weak2 = weak.clone();
                    let c2 = c.clone();
                    let q = query;
                    let timer = slint::Timer::default();
                    timer.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(80),
                        move || {
                            if let Some(ui) = weak2.upgrade() {
                                ui.set_search_text(ss(&q));
                                c2.borrow_mut().search(&ui, q.clone());
                            }
                        },
                    );
                    std::mem::forget(timer);
                }
                SidebarActivateAction::None => {}
            }
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
            c.borrow().sync_active_pane(&ui);
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
            let should_select = {
                let ctrl = c.borrow();
                index >= 0 && !ctrl.selected_set.contains(&(index as usize))
            };
            if should_select {
                c.borrow_mut().select(&ui, index);
            } else {
                let mut ctrl = c.borrow_mut();
                ctrl.active_pane = ActivePane::Primary;
                if index >= 0 {
                    ctrl.selected_index = index;
                    if !ctrl.selected_set.contains(&(index as usize)) {
                        ctrl.selected_set.clear();
                        ctrl.selected_set.insert(index as usize);
                        ctrl.select_anchor = index;
                    }
                    if let Some(entry) = ctrl.visible_files.get(index as usize) {
                        ui.set_selected_name(ss(&entry.name));
                    }
                    ui.set_selected_index(index);
                    ctrl.sync_selection_count_to_ui(&ui);
                    ctrl.update_selection_in_model(&ui, &[index as usize]);
                    ctrl.update_status(&ui);
                }
            }
            let is_archive = c
                .borrow()
                .selected_entries()
                .iter()
                .any(|entry| is_archive_ext(entry.extension.as_deref().unwrap_or("")));
            ui.set_context_on_file(index >= 0);
            ui.set_context_is_archive(is_archive);
            ui.set_context_visible(true);
        }
    });

    #[cfg(target_os = "windows")]
    {
        let weak = ui.as_weak();
        let c = controller.clone();
        ui.on_start_file_drag(move |index, pane| {
            let pane = pane.to_string();
            let paths: Vec<String> = {
                let ctrl = c.borrow();
                let from_secondary = pane == "secondary";
                let selected_set = if from_secondary {
                    &ctrl.secondary_selected_set
                } else {
                    &ctrl.selected_set
                };
                let visible = if from_secondary {
                    &ctrl.secondary_visible_files
                } else {
                    &ctrl.visible_files
                };
                if selected_set.contains(&(index as usize)) && !selected_set.is_empty() {
                    selected_set
                        .iter()
                        .filter_map(|&i| visible.get(i).map(|e| e.path.clone()))
                        .collect()
                } else if index >= 0 {
                    visible
                        .get(index as usize)
                        .map(|e| e.path.clone())
                        .into_iter()
                        .collect()
                } else {
                    vec![]
                }
            };
            if !paths.is_empty() {
                let count = paths.len() as i32;
                // Compose a short, human-friendly label for the drag ghost.
                // Single file: "Photo.png". Many: "Photo.png + 4 more".
                let first_name = std::path::Path::new(&paths[0])
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| paths[0].clone());
                let label = if paths.len() == 1 {
                    first_name
                } else {
                    format!("{first_name} + {} more", paths.len() - 1)
                };
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ThemePalette>().set_drag_count(count);
                    ui.global::<ThemePalette>().set_is_dragging(true);
                    ui.set_drag_label(SharedString::from(label));
                    seed_drag_ghost_from_paths(&ui, &paths);
                    ui.window().request_redraw();
                }
                let effect = file_drag::start(paths);
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ThemePalette>().set_is_dragging(false);
                    ui.global::<ThemePalette>().set_drag_count(0);
                    ui.set_drag_label(SharedString::from(""));
                    clear_drag_ghost_ui(&ui);
                    ui.set_drag_target_path(SharedString::from(""));
                    ui.set_drag_over_pane(SharedString::from(""));
                    // If an external app moved the files, refresh so they disappear.
                    use windows::Win32::System::Ole::DROPEFFECT_MOVE;
                    if effect == DROPEFFECT_MOVE {
                        c.borrow_mut().invalidate_and_refresh_both_panes(&ui);
                    }
                }
            }
        });
    }

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

    // Refresh the secondary pane explicitly. Used by the post-drag-drop refresh
    // path so both panes pick up moved files without depending on which pane is
    // currently "active".
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            if !ctrl.secondary_path.is_empty() {
                let path = ctrl.secondary_path.clone();
                ctrl.app_state.invalidate_directory_path(Path::new(&path));
                ctrl.secondary_navigate(&ui, path);
            }
        }
    });

    // Debounce: keystroke fires search_requested -> 200ms timer -> search()
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
            let q = query.to_string();
            let mut ctrl = c.borrow_mut();
            if looks_like_path_query(&q) {
                let _ = ctrl.search_by_path(&ui, &q, true);
            } else {
                ctrl.search(&ui, q);
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_toggle_search_scope(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().toggle_search_scope(&ui);
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
    let c = controller.clone();
    ui.on_toggle_quick_look(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().toggle_quick_look(&ui);
        }
    });

    let weak = ui.as_weak();
    ui.on_quick_look_play(move || {
        if let Some(ui) = weak.upgrade() {
            let path = ui.get_quick_look_path().to_string();
            if !path.is_empty() {
                let _ = open::that(&path);
            }
        }
    });

    let weak = ui.as_weak();
  ui.on_quick_look_close(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_quick_look_visible(false);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tab_reordered(move |from, to| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().reorder_tab(&ui, from, to);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_rename_preset_picked(move |preset| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .apply_rename_preset_choice(&ui, preset.to_string());
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_batch_rename_preview_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            let ctrl = c.borrow();
            if let Some(PendingPrompt::BatchRename(paths)) = &ctrl.pending_prompt {
                let paths = paths.clone();
                drop(ctrl);
                c.borrow().sync_batch_rename_preview(&ui, &paths);
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tool_overlay_activate(move |id| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().tool_overlay_activate(&ui, id.to_string());
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tool_overlay_toggle(move |id| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().tool_overlay_toggle(&ui, id.to_string());
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tool_overlay_secondary(move |id| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().tool_overlay_secondary(&ui, id.to_string());
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tool_overlay_close(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_tool_overlay_visible(false);
            let _ = c;
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_tool_overlay_primary_action(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().tool_overlay_primary_action(&ui);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_shortcut_hint_changed(move |command, hint| {
        c.borrow_mut()
            .shortcut_hint_changed(command.to_string(), hint.to_string());
        let _ = weak;
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_compare_row_activate(move |path| {
        if let Some(ui) = weak.upgrade() {
            let left = c.borrow().compare_left.clone();
            let full = Path::new(&left).join(path.as_str());
            if let Some(parent) = full.parent() {
                c.borrow_mut()
                    .navigate(&ui, parent.to_string_lossy().to_string(), true);
            }
            ui.set_compare_overlay_visible(false);
        }
    });
    let weak = ui.as_weak();
    ui.on_compare_overlay_close(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_compare_overlay_visible(false);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_compare_toggle_hide_same(move || {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            ctrl.compare_hide_same = !ctrl.compare_hide_same;
            ctrl.push_compare_overlay(&ui);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_dupe_group_keep(move |id| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().dupe_group_keep(&ui, id.to_string());
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_dupe_group_delete_others(move |id| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().dupe_group_delete_others(&ui, id.to_string());
        }
    });
    let weak = ui.as_weak();
    ui.on_dupe_overlay_close(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_dupe_overlay_visible(false);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_search_chip(move |op| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().search_chip_insert(&ui, op.to_string());
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_search_save_requested(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().search_save_current(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_search_help_toggle(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_search_help_visible(!ui.get_search_help_visible());
            if ui.get_search_help_visible() {
                ui.set_search_chip_hint(ss(
                    "Tips: ext:pdf  kind:image  size:>10mb  modified:week  tag:yellow  content:TODO",
                ));
            }
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_queue_pill_clicked(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().show_operation_queue(&ui);
            ui.set_op_drawer_visible(true);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_welcome_next_step(move || {
        if let Some(ui) = weak.upgrade() {
            let step = ui.get_welcome_step();
            if step < 2 {
                ui.set_welcome_step(step + 1);
            } else {
                c.borrow_mut().settings.first_run_welcome_dismissed = true;
                let _ = write_native_json("settings.json", &c.borrow().settings);
                ui.set_welcome_visible(false);
                c.borrow_mut().navigate(&ui, "home://".to_string(), true);
            }
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_welcome_set_mode(move |mode| {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            ctrl.settings.ui_mode = mode.to_string();
            ctrl.save_settings();
            ui.set_ui_mode(ss(mode.as_str()));
            let simple = ctrl.side_items_simple();
            ui.set_side_items_simple(model_from_vec(simple));
            ui.set_welcome_step(2);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_toggle_dual_pane(move || {
        if let Some(ui) = weak.upgrade() {
            let was_dual = ui.get_dual_pane();
            ui.set_dual_pane(!was_dual);
            if !was_dual {
                let path = c.borrow().default_secondary_path();
                c.borrow_mut().secondary_navigate(&ui, path);
            } else {
                c.borrow_mut().active_pane = ActivePane::Primary;
                c.borrow().sync_active_pane(&ui);
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_clear_selection(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().clear_selection(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_marquee_select(move |x, y, w, h, commit_preview| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .marquee_select(&ui, x, y, w, h, commit_preview);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_rename_tag_prompt(move |path| {
        if let Some(ui) = weak.upgrade() {
            let p = path.to_string();
            if let Some(tag_id) = p.strip_prefix("tag:") {
                c.borrow_mut()
                    .show_rename_tag_prompt(&ui, tag_id.to_string());
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_navigate(move |path| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().secondary_navigate(&ui, path.to_string());
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_file_opened(move |index| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().secondary_file_opened(&ui, index);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    let pd = preview_debounce.clone();
    ui.on_secondary_file_selected(move |index, ctrl, shift| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .secondary_file_selected(&ui, index, ctrl, shift);
            c.borrow().sync_active_pane(&ui);
        }
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
    ui.on_secondary_file_context(move |index| {
        if let Some(ui) = weak.upgrade() {
            let should_select = {
                let ctrl = c.borrow();
                index >= 0 && !ctrl.secondary_selected_set.contains(&(index as usize))
            };
            if should_select {
                c.borrow_mut()
                    .secondary_file_selected(&ui, index, false, false);
            } else {
                let mut ctrl = c.borrow_mut();
                ctrl.active_pane = ActivePane::Secondary;
                if index >= 0 {
                    ctrl.secondary_selected_index = index;
                    if !ctrl.secondary_selected_set.contains(&(index as usize)) {
                        ctrl.secondary_selected_set.clear();
                        ctrl.secondary_selected_set.insert(index as usize);
                        ctrl.secondary_select_anchor = index;
                    }
                    if let Some(entry) = ctrl.secondary_visible_files.get(index as usize) {
                        ui.set_selected_name(ss(&entry.name));
                    }
                    ui.set_selected_index(index);
                    ctrl.update_secondary_selection_in_model(&ui, &[index as usize]);
                    ctrl.update_status(&ui);
                }
            }
            c.borrow().update_preview(&ui);
            let is_archive = c
                .borrow()
                .selected_entries()
                .iter()
                .any(|entry| is_archive_ext(entry.extension.as_deref().unwrap_or("")));
            ui.set_context_on_file(index >= 0);
            ui.set_context_is_archive(is_archive);
            ui.set_context_visible(true);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_go_up(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().secondary_go_up(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_go_back(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().secondary_go_back(&ui);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_sort_column(move |col| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().sort_secondary_column(&ui, &col);
        }
    });

    let filter_debounce = Rc::new(slint::Timer::default());
    let weak = ui.as_weak();
    let c = controller.clone();
    let fd = filter_debounce.clone();
    ui.on_filter_changed(move |text| {
        let t = text.to_string();
        let weak2 = weak.clone();
        let c2 = c.clone();
        fd.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(150),
            move || {
                if let Some(ui) = weak2.upgrade() {
                    c2.borrow_mut().set_folder_filter(&ui, t.clone());
                }
            },
        );
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_navigate_path(move |path| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().secondary_navigate(&ui, path.to_string());
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_toggle_hidden(move || {
        if let Some(ui) = weak.upgrade() {
            let new_state = !ui.get_show_hidden();
            ui.set_show_hidden(new_state);
            // Push the toggle into the controller and refresh so dotfiles
            // and .ini files appear or disappear immediately.
            {
                let mut ctrl = c.borrow_mut();
                ctrl.show_hidden = new_state;
            }
            c.borrow_mut().refresh(&ui);
        }
    });

    // HTML / Markdown preview: open the selected file in the system default
    // browser so the user can see the rendered output. The preview pane
    // itself keeps showing source code so this is the "Output" half of the
    // Code / Output toggle.
    let weak = ui.as_weak();
    let c_browser = controller.clone();
    ui.on_open_preview_in_browser(move || {
        if let Some(_ui) = weak.upgrade() {
            let ctrl = c_browser.borrow();
            if let Some(entry) = ctrl.selected_entry() {
                let _ = open::that(&entry.path);
            }
        }
    });

    // Local AI install/uninstall callbacks. The explainer dialog opens via
    // an in-UI property change; these handlers run the actual work.
    let weak = ui.as_weak();
    let c_ai = controller.clone();
    ui.on_ai_install_confirm(move || {
        if let Some(ui) = weak.upgrade() {
            let progress = c_ai.borrow().ai_progress.clone();
            local_ai::start_install(progress);
            ui.set_ai_install_state(SharedString::from("downloading"));
        }
    });
    let weak = ui.as_weak();
    let c_ai = controller.clone();
    ui.on_ai_uninstall(move || {
        if let Some(ui) = weak.upgrade() {
            let progress = c_ai.borrow().ai_progress.clone();
            local_ai::uninstall(progress);
            crate::inference::reset_inference_sessions();
            {
                let mut b = c_ai.borrow_mut();
                if let Ok(mut cap) = b.app_state.ai_capabilities.lock() {
                    *cap = None;
                }
                let caps = compute_ai_capabilities();
                b.ai = caps.clone();
                b.settings.search_semantic_mode = false;
                b.settings.clip_search_enabled = false;
                b.save_settings();
                ui.set_ai_device(ss(&caps.reason));
                ui.set_ai_gpu_status(ss(&caps.gpu_summary));
                ui.set_ai_label(ss(ai_status_label(&caps)));
            }
            ui.set_ai_install_state(SharedString::from("not_installed"));
            ui.set_semantic_search_available(false);
            ui.set_search_semantic_mode(false);
            ui.set_clip_search_enabled(false);
            ui.set_ai_download_progress(0.0);
            ui.set_ai_install_message(SharedString::from(""));
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_command(move |command| {
        if let Some(ui) = weak.upgrade() {
            let cmd = command.to_string();
            {
                let mut ctrl = c.borrow_mut();
                ctrl.recent_commands.retain(|c| c != &cmd);
                ctrl.recent_commands.push_front(cmd.clone());
                while ctrl.recent_commands.len() > 8 {
                    ctrl.recent_commands.pop_back();
                }
            }
            c.borrow_mut().command(&ui, &cmd);
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
    ui.on_custom_accent_changed(move |hex_val| {
        if let Some(ui) = weak.upgrade() {
            let mut h = hex_val.to_string();
            if !h.starts_with('#') {
                h = format!("#{h}");
            }
            if h.len() != 7 {
                return;
            }
            // Reject any character that isn't a hex digit so a typo like "#xxxxxx"
            // doesn't slip through and crash color parsing downstream.
            if !h[1..].chars().all(|c| c.is_ascii_hexdigit()) {
                return;
            }
            let mut controller = c.borrow_mut();
            controller.settings.custom_accent_hex = Some(h.clone());
            controller.settings.accent = "custom".to_string();
            apply_theme(&ui, &controller.settings);
            controller.save_settings();
            ui.set_custom_accent_hex(ss(&h));
            ui.set_active_accent(ss("custom"));
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_folder_color_changed(move |hex_val| {
        if let Some(ui) = weak.upgrade() {
            let mut h = hex_val.to_string();
            if !h.starts_with('#') {
                h = format!("#{h}");
            }
            // Only accept full 7-char hex strings so typing in the LineEdit
            // doesn't repaint with a half-finished color on every keystroke.
            if h.len() != 7 {
                return;
            }
            let mut controller = c.borrow_mut();
            controller.settings.folder_color = Some(h.clone());
            apply_theme(&ui, &controller.settings);
            controller.save_settings();
            ui.set_folder_color_hex(ss(&h));
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
    ui.on_set_ui_mode(move |mode| {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            ctrl.settings.ui_mode = mode.to_string();
            ctrl.save_settings();
            let simple = ctrl.side_items_simple();
            ui.set_side_items_simple(model_from_vec(simple));
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_ui_mode_prompt_choice(move |mode| {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            ctrl.settings.ui_mode = mode.to_string();
            ctrl.save_settings();
            let simple = ctrl.side_items_simple();
            ui.set_side_items_simple(model_from_vec(simple));
            // Sequence the first-run flow: once the user has chosen Simple or
            // Normal, immediately show the welcome dialog. If they had already
            // dismissed the welcome on a previous launch, skip it.
            if !ctrl.settings.first_run_welcome_dismissed {
                ui.set_welcome_visible(true);
            }
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
    let c = controller.clone();
    ui.on_toggle_search_semantic_mode(move || {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            if !local_ai_semantic_ready() {
                ctrl.settings.search_semantic_mode = false;
                ui.set_search_semantic_mode(false);
                ui.set_semantic_search_available(false);
                ctrl.save_settings();
                ctrl.show_toast(&ui, "Install Local AI before using semantic search.");
                return;
            }
            ctrl.settings.search_semantic_mode = !ctrl.settings.search_semantic_mode;
            ui.set_search_semantic_mode(ctrl.settings.search_semantic_mode);
            ctrl.save_settings();
            let q = ui.get_search_text().to_string();
            ctrl.search(&ui, q);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_set_clip_search_enabled(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            if enabled && !local_ai_image_search_ready() {
                ctrl.settings.clip_search_enabled = false;
                ui.set_clip_search_enabled(false);
                ctrl.save_settings();
                ctrl.show_toast(
                    &ui,
                    "Install Local AI image models before enabling image tags in search.",
                );
                return;
            }
            ctrl.settings.clip_search_enabled = enabled;
            ctrl.save_settings();
            ui.set_clip_search_enabled(enabled);
            let q = ui.get_search_text().to_string();
            ctrl.search(&ui, q);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_windows_set_default_folder_handler(move || {
        if let Some(ui) = weak.upgrade() {
            #[cfg(target_os = "windows")]
            {
                match set_as_default_file_manager() {
                    Ok(()) => c.borrow_mut().show_toast(
                        &ui,
                        "Pathfinder is set as the default folder handler for your user account.",
                    ),
                    Err(e) => c.borrow_mut().show_toast_kind(&ui, e, "error"),
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                c.borrow_mut()
                    .show_toast(&ui, "Windows integration is only available on Windows.");
            }
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_windows_restore_folder_handler(move || {
        if let Some(ui) = weak.upgrade() {
            #[cfg(target_os = "windows")]
            {
                match restore_as_default_file_manager() {
                    Ok(()) => c
                        .borrow_mut()
                        .show_toast(&ui, "Restored Explorer defaults for folder open."),
                    Err(e) => c.borrow_mut().show_toast_kind(&ui, e, "error"),
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                c.borrow_mut()
                    .show_toast(&ui, "Windows integration is only available on Windows.");
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_welcome_set_default_handler(move || {
        if let Some(ui) = weak.upgrade() {
            #[cfg(target_os = "windows")]
            {
                match set_as_default_file_manager() {
                    Ok(()) => {
                        ui.set_welcome_default_handler_set(true);
                        ui.set_welcome_default_status(ss(
                            "Done. Opening a folder shortcut now launches Pathfinder for your user account.",
                        ));
                        c.borrow_mut().show_toast(
                            &ui,
                            "Pathfinder is set as the default folder handler.",
                        );
                    }
                    Err(e) => {
                        ui.set_welcome_default_status(ss(&format!(
                            "Could not register: {e}. You can try again from Settings -> View."
                        )));
                        c.borrow_mut().show_toast_kind(&ui, e, "error");
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                ui.set_welcome_default_status(ss(
                    "Default folder handler registration is only available on Windows.",
                ));
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_welcome_open_taskbar_settings(move || {
        if let Some(_ui) = weak.upgrade() {
            #[cfg(target_os = "windows")]
            {
                let _ = open::that("ms-settings:taskbar");
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_welcome_dismiss(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_welcome_visible(false);
            let mut ctrl = c.borrow_mut();
            ctrl.settings.first_run_welcome_dismissed = true;
            ctrl.save_settings();
            ctrl.navigate(&ui, "home://".to_string(), true);
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_install_update(move || {
        if let Some(ui) = weak.upgrade() {
            let url = ui.get_update_download_url().to_string();
            if url.is_empty() {
                let _ = open::that(GITHUB_RELEASES_URL);
                return;
            }
            // Show toast on the event loop thread where Rc is accessible.
            c.borrow_mut().show_toast(&ui, "Downloading update...");
            let weak2 = weak.clone();
            std::thread::spawn(move || {
                match download_and_install_update(&url) {
                    Ok(()) => {
                        let toast = if url.to_ascii_lowercase().contains(".msi") {
                            "Windows Installer started - follow the MSI wizard, then restart Pathfinder."
                        } else {
                            "Installer launched - Pathfinder will close."
                        };
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak2.upgrade() {
                                ui.set_toast_text(ss(toast));
                                ui.set_toast_kind(ss("info"));
                            }
                            std::thread::spawn(|| {
                                std::thread::sleep(std::time::Duration::from_millis(1400));
                                let _ = slint::invoke_from_event_loop(|| {
                                    let _ = slint::quit_event_loop();
                                });
                            });
                        });
                    }
                    Err(e) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak2.upgrade() {
                                ui.set_toast_text(ss(&format!("Update failed: {e}")));
                                ui.set_toast_kind(ss("error"));
                            }
                        });
                    }
                }
            });
        }
    });

    let weak = ui.as_weak();
    ui.on_minimize(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    // Storage view callbacks. Rescan kicks off a fresh scan in the background;
    // open-path jumps from a storage row into the regular file pane; toggle
    // flips between the bucket-grid view and the flat ranked list.
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_rescan(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().rescan_storage(&ui);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_open_path(move |path| {
        if let Some(ui) = weak.upgrade() {
            let p = path.to_string();
            let target = if Path::new(&p).is_dir() {
                p
            } else {
                Path::new(&p)
                    .parent()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or(p)
            };
            c.borrow_mut().navigate(&ui, target, true);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_toggle_show_all(move || {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            ctrl.storage_show_all_state = !ctrl.storage_show_all_state;
            ui.set_storage_show_all(ctrl.storage_show_all_state);
            if !ctrl.storage_show_all_state {
                ctrl.clear_storage_bucket_filter(&ui);
            } else {
                ctrl.storage_selected_bucket.clear();
                ui.set_storage_selected_bucket(ss(""));
                ui.set_storage_selected_bucket_name(ss(""));
                if let Some(result) = ctrl.storage_result_clone() {
                    ctrl.push_storage_top_items(&ui, &result);
                }
            }
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_select_bucket(move |bucket| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .select_storage_bucket(&ui, bucket.to_string());
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_clear_bucket(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().clear_storage_bucket_filter(&ui);
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_switch_root(move |new_root| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .switch_storage_root(&ui, new_root.to_string());
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_drill_search_changed(move |q| {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            ctrl.storage_drill_search = q.to_string();
            if let Some(result) = ctrl.storage_result_clone() {
                ctrl.push_storage_top_items(&ui, &result);
            }
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_set_sort_mode(move |mode| {
        if let Some(ui) = weak.upgrade() {
            let mut ctrl = c.borrow_mut();
            ctrl.storage_sort_mode = mode.to_string();
            ui.set_storage_sort_mode(ss(&mode));
            if let Some(result) = ctrl.storage_result_clone() {
                ctrl.push_storage_top_items(&ui, &result);
            }
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_tip_action(move |action, path| {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut()
                .prepare_storage_tip_action(&ui, action.to_string(), path.to_string());
        }
    });
    // Storage entry context helpers: reveal in Explorer and Open With.
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_entry_more(move |path| {
        if let Some(ui) = weak.upgrade() {
            let p = path.to_string();
            if let Err(e) = reveal_in_folder(p.clone()) {
                c.borrow_mut().show_toast_kind(&ui, e, "error");
            }
        }
    });
    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_storage_entry_open_with(move |path| {
        if let Some(ui) = weak.upgrade() {
            let p = path.to_string();
            if let Err(e) = open_with_dialog(&p, Some(&ui)) {
                c.borrow_mut().show_toast_kind(&ui, e, "error");
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_maximize(move || {
        if let Some(ui) = weak.upgrade() {
            let window = ui.window();
            window.set_maximized(!window.is_maximized());
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_close(move || {
        // Snapshot geometry before tearing down.
        if let Some(ui) = weak.upgrade() {
            use i_slint_backend_winit::WinitWindowAccessor;
            let mut info: Option<(bool, u32, u32, i32, i32)> = None;
            ui.window().with_winit_window(|window| {
                let is_max = window.is_maximized();
                let size = window.inner_size();
                let pos = window.outer_position().unwrap_or_default();
                let scale = window.scale_factor();
                let lw = (size.width as f64 / scale).round() as u32;
                let lh = (size.height as f64 / scale).round() as u32;
                let lx = (pos.x as f64 / scale).round() as i32;
                let ly = (pos.y as f64 / scale).round() as i32;
                info = Some((is_max, lw, lh, lx, ly));
            });
            if let Some((is_max, lw, lh, lx, ly)) = info {
                let mut ctrl = c.borrow_mut();
                ctrl.settings.window_maximized = is_max;
                if !is_max {
                    ctrl.settings.window_w = lw;
                    ctrl.settings.window_h = lh;
                    ctrl.settings.window_x = lx;
                    ctrl.settings.window_y = ly;
                }
                ctrl.save_settings();
            }
        }
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
        let op_queue_for_progress = controller.borrow().app_state.operation_queue.clone();
        let ai_progress_for_ui = controller.borrow().ai_progress.clone();
        let dir_ready = controller.borrow().directory_ready.clone();
        let pending_dir = controller.borrow().pending_directory_result.clone();
        let search_ready = controller.borrow().search_ready.clone();
        let pending_search = controller.borrow().pending_search_result.clone();
        let preview_ready = controller.borrow().preview_ready.clone();
        let pending_preview = controller.borrow().pending_preview_result.clone();
        let timer = slint::Timer::default();
        let prev_ai_install = Rc::new(Cell::new(local_ai::InstallState::NotInstalled));
        let prev_ai_cell = prev_ai_install.clone();
        let idle_poll_skips = Rc::new(Cell::new(0u8));
        let idle_skips_cell = idle_poll_skips.clone();
        // 200ms tick when work is pending; skips every other tick when idle.
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                if let Some(ui) = weak.upgrade() {
                    if let Ok(mut ctrl) = c.try_borrow_mut() {
                        ctrl.apply_deferred_startup_if_ready(&ui);
                        ctrl.pump_visible_enrichment(&ui);
                    }
                }

                let thumb_fired = ready_flag.swap(false, Ordering::AcqRel);
                let git_fired = git_ready.swap(false, Ordering::AcqRel);
                let op_fired = op_ready.swap(false, Ordering::AcqRel);
                let dir_pending = dir_ready.load(Ordering::Acquire);
                let search_pending = search_ready.load(Ordering::Acquire);
                let preview_pending = preview_ready.load(Ordering::Acquire);
                let storage_pending = {
                    if let Ok(ctrl) = c.try_borrow() {
                        ctrl.storage_scan_ready.load(Ordering::Acquire)
                            || ctrl.storage_scan_active
                    } else {
                        false
                    }
                };
                let work_pending = thumb_fired
                    || git_fired
                    || op_fired
                    || dir_pending
                    || search_pending
                    || preview_pending
                    || storage_pending;
                if !work_pending {
                    let skips = idle_skips_cell.get();
                    idle_skips_cell.set(skips.wrapping_add(1));
                    if skips.is_multiple_of(2) {
                        return;
                    }
                } else {
                    idle_skips_cell.set(0);
                }

                // Storage scan completion: drain the pending result, cache it,
                // and push to the UI if the storage view is currently active.
                if let Some(ui) = weak.upgrade() {
                    let needs_storage_poll = {
                        let ctrl = c.borrow();
                        ctrl.storage_scan_active
                            || ui.get_is_storage_view()
                            || ctrl.storage_scan_ready.load(Ordering::Acquire)
                    };
                    if needs_storage_poll {
                        c.borrow_mut().poll_storage_scan(&ui);
                    }
                    if let Ok(ctrl) = c.try_borrow() {
                        ctrl.sync_queue_busy(&ui);
                    }
                }

                // Push live progress for any currently running archive op into
                // the op drawer. Cheap: at most one entry will be "running" at
                // a time during a manual compress / extract action.
                if let Some(ui) = weak.upgrade() {
                    if ui.get_op_drawer_visible() {
                        if let Ok(queue) = op_queue_for_progress.lock() {
                            if let Some(running) = queue.iter().rev().find(|it| {
                                (it.kind == "archive" || it.kind == "extract")
                                    && it.status == "running"
                            }) {
                                let frac = if running.bytes_total > 0 {
                                    (running.bytes_done as f64 / running.bytes_total as f64)
                                        .clamp(0.0, 1.0) as f32
                                } else {
                                    -1.0
                                };
                                ui.set_op_drawer_progress(frac);
                            }
                        }
                    }

                    // Mirror local AI installer state into the Slint properties
                    // only while Settings is open or a download is active.
                    let ai_state = ai_progress_for_ui
                        .state
                        .lock()
                        .map(|s| *s)
                        .unwrap_or(local_ai::InstallState::NotInstalled);
                    if ui.get_settings_visible()
                        || ai_state == local_ai::InstallState::Downloading
                    {
                        ui.set_ai_install_state(SharedString::from(ai_state.as_slint_str()));
                        let downloaded = ai_progress_for_ui
                            .bytes_downloaded
                            .load(std::sync::atomic::Ordering::Acquire);
                        let total = ai_progress_for_ui
                            .bytes_total
                            .load(std::sync::atomic::Ordering::Acquire);
                        let frac = if total > 0 {
                            (downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32
                        } else {
                            0.0
                        };
                        ui.set_ai_download_progress(frac);
                        if let Ok(msg) = ai_progress_for_ui.message.lock() {
                            ui.set_ai_install_message(SharedString::from(msg.as_str()));
                        }
                    }
                    let prev_st = prev_ai_cell.get();
                    if ai_state == local_ai::InstallState::Installed
                        && prev_st != local_ai::InstallState::Installed
                    {
                        {
                            if let Ok(ctrl) = c.try_borrow_mut() {
                                if let Ok(mut cap) = ctrl.app_state.ai_capabilities.lock() {
                                    *cap = None;
                                }
                            }
                        }
                        crate::inference::reset_inference_sessions();
                        let caps = compute_ai_capabilities();
                        if let Ok(mut ctrl) = c.try_borrow_mut() {
                            ctrl.ai = caps.clone();
                            ui.set_ai_device(ss(&caps.reason));
                            ui.set_ai_gpu_status(ss(&caps.gpu_summary));
                            ui.set_ai_label(ss(ai_status_label(&caps)));
                            invalidate_local_ai_ready_cache();
                            ui.set_semantic_search_available(local_ai_semantic_ready_cached());
                        }
                    }
                    prev_ai_cell.set(ai_state);
                }
                let dir_fired = dir_ready.swap(false, Ordering::AcqRel);
                let search_fired = search_ready.swap(false, Ordering::AcqRel);
                let preview_fired = preview_ready.swap(false, Ordering::AcqRel);
                if preview_fired {
                    if let Some(ui) = weak.upgrade() {
                        let result = pending_preview.lock().ok().and_then(|mut l| l.take());
                        if let Some(result) = result {
                            // Only apply if this is still the current selection
                            // and the preview pane is open.
                            let current = c
                                .try_borrow()
                                .map(|ctrl| {
                                    ctrl.preview_generation.load(Ordering::SeqCst)
                                        == result.generation
                                })
                                .unwrap_or(false);
                            if current && ui.get_preview_visible() {
                                ui.set_preview_body(ss(result.body));
                                ui.set_preview_meta(ss(result.meta));
                                ui.set_preview_rendered(ss(result.rendered));
                                if let Some(rgba) = result.pdf_image {
                                    ui.set_preview_pdf_page(rgba_to_slint_image(&rgba));
                                    ui.set_preview_has_pdf_page(true);
                                }
                            }
                        }
                    }
                }
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
                    if dir_fired {
                        let result = pending_dir.lock().ok().and_then(|mut lock| lock.take());
                        if let Some(result) = result {
                            if let Ok(mut ctrl) = c.try_borrow_mut() {
                                if ctrl.nav_generation.load(Ordering::SeqCst) != result.generation {
                                    return;
                                }
                                if !same_path_string(&ctrl.current_path, &result.path) {
                                    return;
                                }
                                if let Some(err) = result.error {
                                    ui.set_empty_state(ss(format!(
                                        "Cannot open \"{}\"",
                                        result.path
                                    )));
                                    ctrl.show_toast_kind(&ui, err, "error");
                                    return;
                                }
                                if ctrl.files.is_empty() {
                                    let page = DirectoryPage {
                                        entries: result.entries,
                                        partial: result.partial,
                                        skipped_entries: result.skipped_entries,
                                    };
                                    ctrl.apply_directory_listing(
                                        &ui,
                                        result.path,
                                        page,
                                        false,
                                        true,
                                    );
                                } else {
                                    ctrl.files = result.entries;
                                    ctrl.apply_filter();
                                    ctrl.update_file_models(&ui);
                                    ui.set_empty_state(ss(format!(
                                        "Showing the first {} items while the full folder loads.",
                                        ctrl.files.len()
                                    )));
                                }
                            }
                        }
                    }
                    if search_fired {
                        let result = pending_search.lock().ok().and_then(|mut lock| lock.take());
                        if let Some(result) = result {
                            if let Ok(mut ctrl) = c.try_borrow_mut() {
                                if same_path_string(&ctrl.search_root(), &result.path)
                                    && ctrl.search_query == result.query
                                {
                                    let count = result.entries.len();
                                    ctrl.visible_files = result.entries;
                                    ctrl.apply_sort();
                                    ctrl.update_models(&ui);
                                    ctrl.update_status(&ui);
                                    if count == 0 && !result.partial {
                                        ui.set_empty_state(ss(
                                            "No matches. Try ext:pdf, kind:image, size:>10mb, or modified:week",
                                        ));
                                    } else {
                                        ui.set_empty_state(ss(""));
                                    }
                                    ui.set_status_right(ss(format!(
                                        "{} | {}{} ({} {})",
                                        result.path,
                                        result.source,
                                        if result.partial { " — still searching" } else { "" },
                                        count,
                                        if count == 1 { "match" } else { "matches" }
                                    )));
                                    if !result.partial {
                                        ctrl.show_toast_kind(
                                            &ui,
                                            format!(
                                                "{} match{} from {}",
                                                count,
                                                if count == 1 { "" } else { "es" },
                                                result.source
                                            ),
                                            "info",
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if op_fired {
                        let result = pending_op.lock().ok().and_then(|mut lock| lock.take());
                        ui.set_op_drawer_visible(false);
                        if let Some(result) = result {
                            if let Ok(mut ctrl) = c.try_borrow_mut() {
                                if !result.invalidate_dirs.is_empty() {
                                    ctrl.invalidate_paste_directories(&result.invalidate_dirs);
                                }
                                if result.clear_clipboard {
                                    ctrl.clipboard = None;
                                    #[cfg(target_os = "windows")]
                                    let _ = file_drag::clear_shell_files_clipboard();
                                    ctrl.refresh_clipboard_visuals(&ui);
                                }
                                if result.refresh_both_panes {
                                    ctrl.invalidate_and_refresh_both_panes(&ui);
                                } else if result.refresh {
                                    ctrl.refresh(&ui);
                                }
                                if let Some(path) = result.secondary_refresh_path {
                                    ctrl.secondary_navigate(&ui, path);
                                }
                                ctrl.show_toast_kind(&ui, result.message, &result.kind);
                            }
                        }
                    }
                    if thumb_fired || git_fired {
                        if let Ok(mut ctrl) = c.try_borrow_mut() {
                            ctrl.update_file_models(&ui);
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
                            .map(|t| {
                                t.elapsed()
                                    >= NativeController::toast_display_duration(
                                        &ctrl.toast_current_kind,
                                        &ctrl.toast_current_message,
                                    )
                            })
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
    ui.on_toast_copy(move || {
        if let Some(ui) = weak.upgrade() {
            let text = ui.get_toast_text().to_string();
            if text.is_empty() {
                return;
            }
            let mut ctrl = c.borrow_mut();
            match copy_text_to_clipboard(&text) {
                Ok(()) => ctrl.show_toast_kind(&ui, "Message copied to clipboard", "success"),
                Err(e) => ctrl.show_toast_kind(&ui, e, "error"),
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_toast_dismiss(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().dismiss_toast(&ui);
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
    ui.on_prompt_cancel(move || {
        if let Some(ui) = weak.upgrade() {
            c.borrow_mut().cancel_prompt(&ui);
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
    let c = controller.clone();
    ui.on_filter_commands(move |query| {
        if let Some(ui) = weak.upgrade() {
            let recent: Vec<String> = c.borrow().recent_commands.iter().cloned().collect();
            ui.set_command_items(command_items_filtered_with_recent(&query, &recent));
            let mut ctrl = c.borrow_mut();
            if !ctrl.settings.palette_tip_shown {
                ctrl.settings.palette_tip_shown = true;
                ctrl.save_settings();
                ctrl.show_toast(
                    &ui,
                    "Tip: type to filter commands. Try “duplicates”, “rules”, or “home”.",
                );
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_command_run_first(move |query| {
        if let Some(ui) = weak.upgrade() {
            use slint::Model;
            let recent: Vec<String> = c.borrow().recent_commands.iter().cloned().collect();
            let items = command_items_filtered_with_recent(&query, &recent);
            if let Some(item) = items.row_data(0) {
                ui.set_command_visible(false);
                let cmd = item.command.to_string();
                {
                    let mut ctrl = c.borrow_mut();
                    ctrl.recent_commands.retain(|c| c != &cmd);
                    ctrl.recent_commands.push_front(cmd.clone());
                    while ctrl.recent_commands.len() > 8 {
                        ctrl.recent_commands.pop_back();
                    }
                }
                c.borrow_mut().command(&ui, cmd.as_str());
            }
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
                    .map(|e| (e.path.clone(), e.name.clone(), e.kind == FileKind::Directory))
            };
            if let Some((old_path, old_name, is_dir)) = result {
                c.borrow_mut()
                    .commit_rename(&ui, &old_path, &old_name, &new_name, is_dir);
            }
        }
    });

    let weak = ui.as_weak();
    let c = controller.clone();
    ui.on_secondary_rename_file(move |index, new_name| {
        if let Some(ui) = weak.upgrade() {
            let result = {
                let ctrl = c.borrow();
                ctrl.secondary_visible_files
                    .get(index as usize)
                    .map(|e| (e.path.clone(), e.name.clone(), e.kind == FileKind::Directory))
            };
            if let Some((old_path, old_name, is_dir)) = result {
                c.borrow_mut()
                    .commit_rename(&ui, &old_path, &old_name, &new_name, is_dir);
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
    ui.on_ce_font_preset(move |slot, preset| {
        if let Some(ui) = weak.upgrade() {
            let p = preset.to_string();
            if slot.as_str() == "ui" {
                let id = normalize_ui_font_preset(&p);
                ui.set_ce_ui_font(ss(&id));
                ui.set_ce_preview_ui_font(ss(bundled_ui_family_from_preset(id.as_str())));
            } else {
                let id = normalize_mono_font_preset(&p);
                ui.set_ce_mono_font(ss(&id));
                ui.set_ce_preview_mono_font(ss(bundled_mono_family_from_preset(id.as_str())));
            }
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
    ui.on_ce_gradient_changed(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            ui.set_ce_gradient_background(enabled);
            if !enabled {
                ui.set_ce_gradient_accent(false);
            }
            let def = editor_def_from_ui(&ui);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_gradient_accent_changed(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            ui.set_ce_gradient_accent(enabled);
            let def = editor_def_from_ui(&ui);
            apply_custom_theme_to_ui(&ui, &def);
        }
    });

    let weak = ui.as_weak();
    ui.on_ce_apply_preview(move || {
        if let Some(ui) = weak.upgrade() {
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
            normalize_theme_font_presets(&mut def);
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
                sync_editor_state(&ui, &def);
                apply_custom_theme_to_ui(&ui, &def);
                apply_window_finish(&ui, &def.finish);
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

fn configure_native_window(ui: &MainWindow, settings: &NativeSettings) {
    use i_slint_backend_winit::WinitWindowAccessor;
    use i_slint_backend_winit::winit::dpi::{LogicalPosition, LogicalSize};

    ui.window().with_winit_window(|window| {
        window.set_resizable(true);
        window.set_min_inner_size(Some(LogicalSize::new(900.0, 600.0)));
        window.set_max_inner_size::<LogicalSize<f64>>(None);

        if settings.window_maximized {
            window.set_maximized(true);
        } else if settings.window_w > 0 {
            let _ = window.request_inner_size(LogicalSize::new(
                settings.window_w as f64,
                settings.window_h as f64,
            ));
            if settings.window_x != i32::MIN {
                // Clamp y so the title bar is never hidden behind the screen top edge.
                let safe_y = (settings.window_y as f64).max(30.0);
                let safe_x = (settings.window_x as f64).max(0.0);
                window.set_outer_position(LogicalPosition::new(safe_x, safe_y));
            }
        } else {
            // First launch: 80% of the monitor's logical area, centered with top margin.
            if let Some(monitor) = window.current_monitor() {
                let scale = monitor.scale_factor();
                let phys = monitor.size();
                let log_w = phys.width as f64 / scale;
                let log_h = phys.height as f64 / scale;
                let target_w = (log_w * 0.80).clamp(900.0, 1600.0);
                // Reserve ~10% of height for taskbar; clamp to safe range.
                let target_h = ((log_h - 80.0) * 0.82).clamp(600.0, 1000.0);
                let _ = window.request_inner_size(LogicalSize::new(target_w, target_h));
                // Center horizontally; push down 40px from top so title bar is always visible.
                let cx = ((log_w - target_w) / 2.0).max(0.0);
                let cy = 40.0_f64.max((log_h - target_h) / 2.0 - 40.0);
                window.set_outer_position(LogicalPosition::new(cx, cy));
            }
        }
    });
}

#[cfg(target_os = "windows")]
fn apply_mica(ui: &MainWindow) {
    use i_slint_backend_winit::WinitWindowAccessor;
    use i_slint_backend_winit::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};

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

// ============================================================================
// Windows Integration Commands (COM Interop, VSS, UAC, Taskbar Pinning)
// ============================================================================

#[cfg(target_os = "windows")]
#[tauri::command]
fn get_context_menu_actions(
    path: String,
) -> Result<Vec<windows_integration::ContextMenuAction>, String> {
    windows_integration::get_context_menu_actions(&path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn get_context_menu_actions(_path: String) -> Result<Vec<serde_json::Value>, String> {
    Err("Context menu actions are Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn invoke_context_menu_action(path: String, action_id: u32) -> Result<(), String> {
    windows_integration::invoke_context_menu_action(&path, action_id)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn invoke_context_menu_action(_path: String, _action_id: u32) -> Result<(), String> {
    Err("Context menu actions are Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn get_previous_versions(
    path: String,
) -> Result<Vec<windows_integration::PreviousVersion>, String> {
    windows_integration::get_previous_versions(&path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn get_previous_versions(_path: String) -> Result<Vec<serde_json::Value>, String> {
    Err("Previous versions (VSS) are Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn restore_from_previous_version(path: String, version_id: String) -> Result<(), String> {
    windows_integration::restore_from_previous_version(&path, &version_id)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn restore_from_previous_version(_path: String, _version_id: String) -> Result<(), String> {
    Err("Previous versions (VSS) are Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn is_process_elevated() -> bool {
    windows_integration::is_process_elevated()
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn is_process_elevated() -> bool {
    false
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn retry_as_administrator(
    operation: String,
    path: String,
) -> Result<windows_integration::AdminRetryResult, String> {
    windows_integration::retry_as_administrator(&operation, &path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn retry_as_administrator(_operation: String, _path: String) -> Result<serde_json::Value, String> {
    Err("Administrator retry is Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn take_ownership(path: String) -> Result<windows_integration::AdminRetryResult, String> {
    windows_integration::take_ownership(&path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn take_ownership(_path: String) -> Result<serde_json::Value, String> {
    Err("Take ownership is Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn create_shortcut(
    target_path: String,
    shortcut_path: String,
    args: Option<String>,
    working_dir: Option<String>,
) -> Result<(), String> {
    windows_integration::create_shortcut(
        &target_path,
        &shortcut_path,
        args.as_deref(),
        working_dir.as_deref(),
    )
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn create_shortcut(
    _target_path: String,
    _shortcut_path: String,
    _args: Option<String>,
    _working_dir: Option<String>,
) -> Result<(), String> {
    Err("Shortcut creation is Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn pin_to_taskbar(path: String) -> Result<windows_integration::PinningResult, String> {
    windows_integration::pin_to_taskbar(&path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn pin_to_taskbar(_path: String) -> Result<serde_json::Value, String> {
    Err("Taskbar pinning is Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn pin_to_start_menu(path: String) -> Result<windows_integration::PinningResult, String> {
    windows_integration::pin_to_start_menu(&path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn pin_to_start_menu(_path: String) -> Result<serde_json::Value, String> {
    Err("Start menu pinning is Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn unpin_from_taskbar(path: String) -> Result<windows_integration::PinningResult, String> {
    windows_integration::unpin_from_taskbar(&path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn unpin_from_taskbar(_path: String) -> Result<serde_json::Value, String> {
    Err("Taskbar unpinning is Windows-only".to_string())
}

#[cfg(target_os = "windows")]
#[tauri::command]
fn unpin_from_start_menu(path: String) -> Result<windows_integration::PinningResult, String> {
    windows_integration::unpin_from_start_menu(&path)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn unpin_from_start_menu(_path: String) -> Result<serde_json::Value, String> {
    Err("Start menu unpinning is Windows-only".to_string())
}

// -- Mouse back/forward button navigation -------------------------------------

#[cfg(target_os = "windows")]
static MOUSE_NAV_UI: std::sync::OnceLock<slint::Weak<MainWindow>> = std::sync::OnceLock::new();

#[cfg(target_os = "windows")]
static MOUSE_NAV_ORIG_PROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

// Right edge of the tab strip in Slint logical px (matches main.slint titlebar).
// Updated whenever tabs change so WM_NCHITTEST can avoid HTCAPTION over tabs/+.
#[cfg(target_os = "windows")]
static TITLEBAR_TABS_RIGHT_LOGICAL: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(46f32.to_bits());

#[cfg(target_os = "windows")]
fn sync_titlebar_hit_regions(tabs: &[TabItem]) {
    use std::sync::atomic::Ordering;
    const TABROW_X: f32 = 46.0;
    const TAB_SPACING: f32 = 2.0;
    const TAB_MAX: f32 = 240.0;
    let mut right = TABROW_X;
    for tab in tabs {
        // Conservative upper bound so WM_NCHITTEST never returns HTCAPTION over a tab.
        let _ = tab;
        right += TAB_MAX + TAB_SPACING;
    }
    TITLEBAR_TABS_RIGHT_LOGICAL.store(right.to_bits(), Ordering::Release);
}

/// Map mouse back / forward to the same Slint callbacks as the toolbar.
/// Registered on the winit event path so navigation works even when a custom
/// `WNDPROC` subclass is not first in the chain (winit already handles
/// `WM_XBUTTONDOWN` in the client area and emits `MouseInput`).
fn register_winit_mouse_side_button_navigation(ui: &MainWindow) {
    use i_slint_backend_winit::EventResult;
    use i_slint_backend_winit::WinitWindowAccessor;
    use i_slint_backend_winit::winit::event::{ElementState, MouseButton, WindowEvent};

    let weak = ui.as_weak();
    ui.window().on_winit_window_event(move |_win, event| {
        let WindowEvent::MouseInput { state, button, .. } = event else {
            return EventResult::Propagate;
        };
        if *state != ElementState::Pressed {
            return EventResult::Propagate;
        }
        let go_back = matches!(button, MouseButton::Back);
        let go_forward = matches!(button, MouseButton::Forward);
        if !go_back && !go_forward {
            return EventResult::Propagate;
        }
        let w = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() {
                if go_back {
                    ui.invoke_go_back();
                } else {
                    ui.invoke_go_forward();
                }
            }
        });
        EventResult::Propagate
    });
}

#[cfg(target_os = "windows")]
fn mouse_nav_dispatch_side_buttons(wparam: windows::Win32::Foundation::WPARAM) {
    use windows::Win32::UI::WindowsAndMessaging::XBUTTON1;
    let button = ((wparam.0 as u32) >> 16) as u16;
    if let Some(weak) = MOUSE_NAV_UI.get() {
        let w = weak.clone();
        let is_back = button == XBUTTON1;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = w.upgrade() {
                if is_back {
                    ui.invoke_go_back();
                } else {
                    ui.invoke_go_forward();
                }
            }
        });
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn mouse_nav_wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, GetWindowRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT,
        HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, WM_NCHITTEST, WM_NCXBUTTONDOWN,
    };

    // DWM iconic-thumbnail path used to live here. Windows 11 doesn't reliably
    // honor DWMWA_FORCE_ICONIC_REPRESENTATION for non-tabbed apps and our
    // DwmSetIconicThumbnail seed call kept returning E_INVALIDARG (0x80070057),
    // so DWM fell back to a static app-icon thumbnail with no corner icon
    // overlay. Removing the WM_DWMSENDICONICTHUMBNAIL handler (plus the
    // attribute setup in enable_taskbar_iconic_thumbnail below) lets DWM
    // generate its standard live-preview thumbnail of the actual window.

    if msg == WM_NCHITTEST {
        let x = ((lparam.0 as u32 & 0xffff) as i16) as i32;
        let y = (((lparam.0 as u32 >> 16) & 0xffff) as i16) as i32;
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            let local_x = x - rect.left;
            let local_y = y - rect.top;
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            let border = 8;

            let left = local_x >= 0 && local_x < border;
            let right = local_x >= width - border && local_x < width;
            let top = local_y >= 0 && local_y < border;
            let bottom = local_y >= height - border && local_y < height;

            let hit = match (left, right, top, bottom) {
                (true, _, true, _) => Some(HTTOPLEFT),
                (_, true, true, _) => Some(HTTOPRIGHT),
                (true, _, _, true) => Some(HTBOTTOMLEFT),
                (_, true, _, true) => Some(HTBOTTOMRIGHT),
                (true, _, _, _) => Some(HTLEFT),
                (_, true, _, _) => Some(HTRIGHT),
                (_, _, true, _) => Some(HTTOP),
                (_, _, _, true) => Some(HTBOTTOM),
                _ => None,
            };
            if let Some(hit) = hit {
                return windows::Win32::Foundation::LRESULT(hit as isize);
            }

            // Native caption drag only in the empty titlebar strip (logical coords
            // must match main.slint). HTCAPTION here steals clicks from Slint tabs.
            let scale = MOUSE_NAV_UI
                .get()
                .and_then(|weak| weak.upgrade())
                .map(|ui| ui.window().scale_factor())
                .filter(|s| *s > 0.0)
                .unwrap_or(1.0);
            let lx = local_x as f32 / scale;
            let ly = local_y as f32 / scale;
            let width_logical = width as f32 / scale;

            const TITLE_H: f32 = 36.0;
            const WIN_BTNS_W: f32 = 184.0;
            const PLUS_X: f32 = 10.0;
            const PLUS_Y: f32 = 4.0;
            const PLUS_W: f32 = 28.0;
            const PLUS_H: f32 = 28.0;
            const TAB_Y: f32 = 6.0;
            const TAB_H: f32 = 30.0;

            let in_titlebar = (0.0..TITLE_H).contains(&ly);
            let in_window_buttons = lx >= width_logical - WIN_BTNS_W;
            let in_plus =
                (PLUS_X..PLUS_X + PLUS_W).contains(&lx) && (PLUS_Y..PLUS_Y + PLUS_H).contains(&ly);
            let tabs_right = f32::from_bits(TITLEBAR_TABS_RIGHT_LOGICAL.load(Ordering::Acquire));
            let in_tabs = (46.0..tabs_right).contains(&lx) && (TAB_Y..TAB_Y + TAB_H).contains(&ly);
            let in_drag_strip = in_titlebar
                && !in_window_buttons
                && !in_plus
                && !in_tabs
                && lx < width_logical - WIN_BTNS_W;
            if in_drag_strip {
                return windows::Win32::Foundation::LRESULT(HTCAPTION as isize);
            }
        }
    }

    // Non-client hits (e.g. caption) use WM_NCXBUTTON*; winit does not emit MouseInput for these.
    if msg == WM_NCXBUTTONDOWN {
        mouse_nav_dispatch_side_buttons(wparam);
    }

    // Many mice send browser back/forward as WM_APPCOMMAND instead of XBUTTON messages.
    const WM_APPCOMMAND: u32 = 0x0319;
    const FAPPCOMMAND_MASK: u16 = 0xF000;
    const APPCOMMAND_BROWSER_BACKWARD: u16 = 1;
    const APPCOMMAND_BROWSER_FORWARD: u16 = 2;
    if msg == WM_APPCOMMAND {
        let cmd = (((lparam.0 >> 16) & 0xFFFF) as u16) & !FAPPCOMMAND_MASK;
        let navigate = match cmd {
            APPCOMMAND_BROWSER_BACKWARD => Some(true),
            APPCOMMAND_BROWSER_FORWARD => Some(false),
            _ => None,
        };
        if let Some(is_back) = navigate {
            if let Some(weak) = MOUSE_NAV_UI.get() {
                let w = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        if is_back {
                            ui.invoke_go_back();
                        } else {
                            ui.invoke_go_forward();
                        }
                    }
                });
            }
            return windows::Win32::Foundation::LRESULT(1);
        }
    }

    let orig = MOUSE_NAV_ORIG_PROC.load(Ordering::Acquire);
    if orig != 0 {
        type RawProc = unsafe extern "system" fn(
            windows::Win32::Foundation::HWND,
            u32,
            windows::Win32::Foundation::WPARAM,
            windows::Win32::Foundation::LPARAM,
        ) -> windows::Win32::Foundation::LRESULT;
        // SAFETY: orig is the original WNDPROC stored by SetWindowLongPtrW.
        let orig_fn: RawProc = unsafe { std::mem::transmute(orig as usize) };
        unsafe { CallWindowProcW(Some(orig_fn), hwnd, msg, wparam, lparam) }
    } else {
        windows::Win32::Foundation::LRESULT(0)
    }
}

/// Explicitly clear the iconic-thumbnail attributes so DWM returns to the
/// standard live-preview taskbar thumbnail. Earlier versions tried to provide
/// a custom maze-logo bitmap via DwmSetIconicThumbnail, but the seed call
/// returned E_INVALIDARG on Windows 11 and DWM ended up showing a static app
/// icon with no corner overlay. With these flags cleared, DWM uses its own
/// composition cache to produce a real thumbnail of the window contents.
#[cfg(target_os = "windows")]
unsafe fn enable_taskbar_iconic_thumbnail(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
    use windows::core::BOOL;
    const DWMWA_FORCE_ICONIC_REPRESENTATION: i32 = 7;
    const DWMWA_HAS_ICONIC_BITMAP: i32 = 10;
    let disable: BOOL = BOOL(0);
    unsafe {
        let r1 = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_HAS_ICONIC_BITMAP),
            &disable as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );
        let r2 = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_FORCE_ICONIC_REPRESENTATION),
            &disable as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );
        eprintln!(
            "[taskbar] cleared iconic attributes HAS={:?} FORCE={:?} (live preview enabled)",
            r1, r2
        );
    }
}

#[cfg(target_os = "windows")]
fn startup_warning_toast(weak_ui: slint::Weak<MainWindow>, message: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak_ui.upgrade() {
            ui.set_toast_text(ss(&message));
            ui.set_toast_kind(ss("warning"));
        }
    });
}

fn install_mouse_nav(ui: &MainWindow) {
    // Defer the WindowProc subclass via a Slint Timer for the same reason the
    // IDropTarget registration is deferred: calling `with_winit_window` synchronously
    // right after `ui.show()` returns silently because winit hasn't fully exposed
    // the HWND through Slint's accessor yet. The timer fires once after the event
    // loop has pumped a tick, at which point the HWND is real and SetWindowLongPtrW
    // actually replaces Slint's default proc with ours.
    use i_slint_backend_winit::WinitWindowAccessor;
    use i_slint_backend_winit::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GWLP_WNDPROC, SetWindowLongPtrW};

    let _ = MOUSE_NAV_UI.set(ui.as_weak());
    let weak = ui.as_weak();
    let timer = Box::new(slint::Timer::default());
    timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(150),
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let hwnd_opt = ui
                .window()
                .with_winit_window(|window| {
                    let handle = window.window_handle().ok()?;
                    let RawWindowHandle::Win32(h) = handle.as_raw() else {
                        return None;
                    };
                    Some(HWND(h.hwnd.get() as *mut core::ffi::c_void))
                })
                .flatten()
                .or_else(find_pathfinder_hwnd);
            let Some(hwnd) = hwnd_opt else {
                startup_warning_toast(
                    weak.clone(),
                    "Mouse back/forward buttons are unavailable (could not attach to the window)."
                        .to_string(),
                );
                return;
            };
            unsafe {
                let orig =
                    SetWindowLongPtrW(hwnd, GWLP_WNDPROC, mouse_nav_wnd_proc as *const () as isize);
                MOUSE_NAV_ORIG_PROC.store(orig, Ordering::Release);
                eprintln!(
                    "[mouse_nav] subclassed HWND {:?}, orig proc {:#x}",
                    hwnd.0, orig
                );
                enable_taskbar_iconic_thumbnail(hwnd);
            }
        },
    );
    Box::leak(timer);
}

#[cfg(not(target_os = "windows"))]
fn install_mouse_nav(_ui: &MainWindow) {}

// Determine which folder a drop should land in, based on the screen-space
// drop point. In dual-pane mode the cursor side picks the pane (primary if
// the drop is left of the splitter, otherwise secondary). In single-pane
// mode it always returns the active directory.
//
// `screen_x`/`screen_y` are raw screen pixels from IDropTarget::Drop.
// We convert to client pixels via ScreenToClient, then to Slint logical
// pixels by dividing by the window's scale factor.
#[cfg(target_os = "windows")]
fn drag_cursor_logical_from_screen(ui: &MainWindow) -> Option<(f32, f32)> {
    use i_slint_backend_winit::WinitWindowAccessor;
    use i_slint_backend_winit::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let hwnd = ui
        .window()
        .with_winit_window(|window| {
            let handle = window.window_handle().ok()?;
            let RawWindowHandle::Win32(h) = handle.as_raw() else {
                return None;
            };
            Some(HWND(h.hwnd.get() as *mut core::ffi::c_void))
        })
        .flatten()
        .or_else(find_pathfinder_hwnd)?;
    unsafe {
        let _ = ScreenToClient(hwnd, &mut pt);
    }
    let scale = ui.window().scale_factor().max(0.1);
    Some(((pt.x as f32) / scale, (pt.y as f32) / scale))
}

#[cfg(target_os = "windows")]
fn drag_cursor_logical_from_screen_pt(
    ui: &MainWindow,
    hwnd: windows::Win32::Foundation::HWND,
    screen_x: i32,
    screen_y: i32,
) -> (f32, f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ScreenToClient;

    let mut p = POINT {
        x: screen_x,
        y: screen_y,
    };
    unsafe {
        let _ = ScreenToClient(hwnd, &mut p);
    }
    let scale = ui.window().scale_factor().max(0.1);
    ((p.x as f32) / scale, (p.y as f32) / scale)
}

#[cfg(target_os = "windows")]
fn seed_drag_ghost_from_paths(ui: &MainWindow, paths: &[String]) {
    if let Some(path) = paths.first() {
        let p = Path::new(path);
        ui.set_drag_ghost_is_dir(p.is_dir());
        ui.set_drag_ghost_ext(SharedString::from(
            p.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string(),
        ));
    }
    if let Some((x, y)) = drag_cursor_logical_from_screen(ui) {
        ui.set_drag_cursor_x(x.max(0.0));
        ui.set_drag_cursor_y(y.max(0.0));
    }
    ui.window().request_redraw();
}

#[cfg(target_os = "windows")]
fn clear_drag_ghost_ui(ui: &MainWindow) {
    ui.set_drag_ghost_ext(SharedString::from(""));
    ui.set_drag_ghost_is_dir(false);
    ui.window().request_redraw();
}

#[cfg(not(target_os = "windows"))]
fn seed_drag_ghost_from_paths(_ui: &MainWindow, _paths: &[String]) {}

#[cfg(not(target_os = "windows"))]
fn clear_drag_ghost_ui(_ui: &MainWindow) {}

#[cfg(not(target_os = "windows"))]
fn drag_cursor_logical_from_screen_pt(_ui: &MainWindow, _hwnd: (), _x: i32, _y: i32) -> (f32, f32) {
    (0.0, 0.0)
}

/// Toolbar address-bar X offset (relative to toolbar left edge) in simple mode.
/// Mirrors `toolbar.simple_addr_x` in main.slint.
#[cfg(target_os = "windows")]
fn toolbar_simple_addr_x(toolbar_width: f32) -> f32 {
    const SIMPLE_BTN_W: f32 = 88.0;
    const SIMPLE_ROW_GAP: f32 = 4.0;
    const SIMPLE_ROW1_SLOTS: f32 = 4.0;
    let simple_row1_budget = (toolbar_width * 0.42).clamp(240.0, 380.0);
    let simple_row1_slot_w = SIMPLE_BTN_W.min(
        ((simple_row1_budget - 10.0 - (SIMPLE_ROW1_SLOTS - 1.0) * SIMPLE_ROW_GAP) / SIMPLE_ROW1_SLOTS)
            .max(54.0),
    );
    let simple_row1_step = simple_row1_slot_w + SIMPLE_ROW_GAP;
    10.0 + SIMPLE_ROW1_SLOTS * simple_row1_step + 8.0
}

#[cfg(target_os = "windows")]
struct PaneGridGeom {
    cols: usize,
    grid_cell_w: f32,
    grid_item_h: f32,
    grid_gap: f32,
}

#[cfg(target_os = "windows")]
fn pane_grid_geometry(pane_w: f32, view_mode: &str, grid_w: f32, grid_h: f32) -> PaneGridGeom {
    const PAD: f32 = 16.0;
    const SCROLLBAR: f32 = 14.0;
    let file_area_w = (pane_w - PAD * 2.0 - SCROLLBAR).max(1.0);
    let compact = view_mode == "compact";
    let cell_w_target = if compact { 200.0 } else { grid_w.max(96.0) };
    let cols = (file_area_w / cell_w_target).floor().max(1.0) as usize;
    let grid_cell_w = file_area_w / cols as f32;
    let grid_item_h = match view_mode {
        "gallery" => 154.0_f32,
        "compact" => 32.0_f32,
        _ => grid_h.max(96.0),
    };
    let grid_gap = if compact { 2.0 } else { 8.0 };
    PaneGridGeom {
        cols,
        grid_cell_w,
        grid_item_h,
        grid_gap,
    }
}

#[cfg(target_os = "windows")]
fn pane_scroll_y(ui: &MainWindow, is_secondary: bool, view_mode: &str) -> f32 {
    if is_secondary {
        ui.get_secondary_list_scroll_y()
    } else if view_mode == "list" {
        ui.get_primary_list_scroll_y()
    } else {
        ui.get_primary_grid_scroll_y()
    }
}

#[cfg(target_os = "windows")]
fn hit_test_grid_folder_drop(
    files: &[FileEntry],
    lx: f32,
    ly: f32,
    list_top: f32,
    scroll_y: f32,
    left: f32,
    geom: &PaneGridGeom,
) -> Option<String> {
    let rel_x = lx - left;
    let rel_y = ly - list_top + scroll_y;
    if rel_y < 0.0 {
        return None;
    }
    let row_stride = geom.grid_item_h + geom.grid_gap;
    let row = (rel_y / row_stride).floor() as isize;
    if row < 0 {
        return None;
    }
    let col = ((rel_x - geom.grid_gap / 2.0) / geom.grid_cell_w).floor() as isize;
    if col < 0 || col as usize >= geom.cols {
        return None;
    }
    let cell_x1 = col as f32 * geom.grid_cell_w + geom.grid_gap / 2.0;
    let cell_y1 = row as f32 * row_stride;
    let cell_x2 = cell_x1 + (geom.grid_cell_w - geom.grid_gap);
    let cell_y2 = cell_y1 + geom.grid_item_h;
    if rel_x < cell_x1 || rel_x >= cell_x2 || rel_y < cell_y1 || rel_y >= cell_y2 {
        return None;
    }
    let idx = row as usize * geom.cols + col as usize;
    let entry = files.get(idx)?;
    if entry.kind != FileKind::Directory || entry.path.is_empty() {
        return None;
    }
    Some(entry.path.clone())
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn hit_test_list_folder_drop(
    ui: &MainWindow,
    files: &[FileEntry],
    pane_left: f32,
    pane_w: f32,
    lx: f32,
    ly: f32,
    list_top: f32,
    scroll_y: f32,
    sort_by: &str,
) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    const PAD: f32 = 16.0;
    const SCROLLBAR: f32 = 14.0;
    let view = ui.get_view_mode();
    let view_str = view.as_str();
    let left = pane_left + PAD;
    let right = pane_left + pane_w - PAD - SCROLLBAR;
    if lx < left || lx > right || ly < list_top {
        return None;
    }
    match view_str {
        "list" => hit_test_list_rows(files, left, right, lx, ly, list_top, scroll_y, sort_by),
        "compact" | "grid" | "gallery" => {
            let metrics = ui.global::<AppMetrics>();
            let geom = pane_grid_geometry(
                pane_w,
                view_str,
                metrics.get_grid_w(),
                metrics.get_grid_h(),
            );
            hit_test_grid_folder_drop(files, lx, ly, list_top, scroll_y, left, &geom)
        }
        _ => None,
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn rects_intersect(
    ax1: f32,
    ay1: f32,
    ax2: f32,
    ay2: f32,
    bx1: f32,
    by1: f32,
    bx2: f32,
    by2: f32,
) -> bool {
    ax1 < bx2 && ax2 > bx1 && ay1 < by2 && ay2 > by1
}

#[allow(clippy::too_many_arguments)]
fn marquee_selection_list(
    files: &[FileEntry],
    sort_by: &str,
    mx1: f32,
    my1: f32,
    mx2: f32,
    my2: f32,
    list_top: f32,
    scroll_y: f32,
    row_h: f32,
) -> std::collections::HashSet<usize> {
    const GROUP_HEADER_H: f32 = 26.0;
    let with_groups = sort_by == "modified";
    let mut result = std::collections::HashSet::new();
    let mut cursor = 0.0_f32;
    let mut last_group = "";
    for (i, entry) in files.iter().enumerate() {
        let group = if with_groups {
            date_group_label(entry.modified)
        } else {
            ""
        };
        let header_h = if with_groups && group != last_group {
            GROUP_HEADER_H
        } else {
            0.0
        };
        let row_pane_y1 = list_top + cursor + header_h - scroll_y;
        let row_pane_y2 = row_pane_y1 + row_h;
        if row_pane_y1 > my2 {
            break;
        }
        if rects_intersect(mx1, my1, mx2, my2, 0.0, row_pane_y1, f32::MAX, row_pane_y2) {
            result.insert(i);
        }
        cursor += header_h + row_h;
        if with_groups {
            last_group = group;
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn marquee_selection_grid(
    count: usize,
    cols: usize,
    mx1: f32,
    my1: f32,
    mx2: f32,
    my2: f32,
    pad: f32,
    scroll_y: f32,
    grid_cell_w: f32,
    grid_item_h: f32,
    grid_gap: f32,
) -> std::collections::HashSet<usize> {
    if cols == 0 || count == 0 {
        return std::collections::HashSet::new();
    }
    let row_stride = grid_item_h + grid_gap;
    let cx1 = mx1 - pad;
    let cy1 = my1 - pad + scroll_y;
    let cx2 = mx2 - pad;
    let cy2 = my2 - pad + scroll_y;
    let cell_w = grid_cell_w - grid_gap;

    let min_row = (cy1 / row_stride).floor().max(0.0) as usize;
    let max_row = ((cy2 / row_stride).ceil() as usize).min(count.div_ceil(cols));
    let min_col = ((cx1 - grid_gap / 2.0) / grid_cell_w).floor().max(0.0) as usize;
    let max_col =
        (((cx2 - grid_gap / 2.0) / grid_cell_w).ceil() as usize).min(cols.saturating_sub(1));

    let mut result = std::collections::HashSet::new();
    for row in min_row..=max_row {
        for col in min_col..=max_col {
            let i = row * cols + col;
            if i >= count {
                continue;
            }
            let cell_x1 = col as f32 * grid_cell_w + grid_gap / 2.0;
            let cell_y1 = row as f32 * row_stride;
            let cell_x2 = cell_x1 + cell_w;
            let cell_y2 = cell_y1 + grid_item_h;
            if rects_intersect(cx1, cy1, cx2, cy2, cell_x1, cell_y1, cell_x2, cell_y2) {
                result.insert(i);
            }
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn hit_test_list_rows(
    files: &[FileEntry],
    _left: f32,
    _right: f32,
    _lx: f32,
    ly: f32,
    list_top: f32,
    scroll_y: f32,
    sort_by: &str,
) -> Option<String> {
    const ROW_H: f32 = 38.0;
    const GROUP_HEADER_H: f32 = 26.0;
    let with_groups = sort_by == "modified";
    let target_y = ly - list_top + scroll_y;
    let mut cursor = 0.0_f32;
    let mut last_group: &str = "";
    for entry in files {
        let group = if with_groups {
            date_group_label(entry.modified)
        } else {
            ""
        };
        let header_h = if with_groups && group != last_group {
            GROUP_HEADER_H
        } else {
            0.0
        };
        let row_total = header_h + ROW_H;
        let row_start = cursor + header_h;
        let row_end = cursor + row_total;
        if target_y >= row_start && target_y < row_end {
            if entry.kind != FileKind::Directory || entry.path.is_empty() {
                return None;
            }
            return Some(entry.path.clone());
        }
        cursor = row_end;
        if with_groups {
            last_group = group;
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn preview_pane_width(ui: &MainWindow) -> f32 {
    if ui.get_preview_visible() {
        ui.get_preview_w_user().clamp(220.0, 640.0)
    } else {
        0.0
    }
}

#[cfg(target_os = "windows")]
fn pick_drop_destination(
    ui: &MainWindow,
    ctrl: &NativeController,
    hwnd: windows::Win32::Foundation::HWND,
    screen_x: i32,
    screen_y: i32,
) -> String {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ScreenToClient;

    let mut p = POINT {
        x: screen_x,
        y: screen_y,
    };
    unsafe {
        let _ = ScreenToClient(hwnd, &mut p);
    }
    let scale = ui.window().scale_factor().max(0.1);
    let lx = (p.x as f32) / scale;
    let ly = (p.y as f32) / scale;

    let sidebar_w = ui.get_sidebar_w();
    let primary_w = ui.get_primary_pane_w();
    let splitter_w = ui.get_pane_splitter_w();
    let title_h: f32 = 36.0;
    // Simple mode uses a two-row toolbar that's 88 px tall. Normal mode is 44.
    // Using the wrong value here mis-routes every drop near the top of the file
    // list by the difference, so keep this in sync with `toolbar_h` in main.slint.
    let is_simple = ui.get_ui_mode().as_str() == "simple";
    let toolbar_h: f32 = if is_simple { 88.0 } else { 44.0 };
    let content_top = title_h + toolbar_h;
    let main_left = sidebar_w;
    let window_logical_w = (ui.window().size().width as f32) / scale;
    let file_area_right = window_logical_w - preview_pane_width(ui);

    // Drops on the preview pane (right strip) land in the active pane's folder,
    // matching Explorer where the details/preview strip is not a subfolder target.
    if ui.get_preview_visible() && lx >= file_area_right && ly >= content_top {
        let dest = if ctrl.active_pane == ActivePane::Secondary && !ctrl.secondary_path.is_empty()
        {
            ctrl.secondary_path.clone()
        } else {
            ctrl.current_path.clone()
        };
        if is_virtual_nav_path(&dest) {
            file_drag::log("pick_drop_destination: preview pane over virtual path");
            return String::new();
        }
        file_drag::log(&format!(
            "pick_drop_destination: PREVIEW STRIP -> '{}'",
            dest
        ));
        return dest;
    }

    // 0a. Tab strip drops route into that tab's folder. Tabs live in the title
    //     bar at y in [6, 36], starting at x = 10. Each TabButton is a min of
    //     132 px and a max of 240 px wide based on its title; we approximate
    //     using char-count since the actual text-rendered width isn't reachable
    //     from outside slint. Close enough to be useful; users won't notice a
    //     few pixels of slop on a 200 px tab.
    if ly >= 6.0 && ly <= title_h {
        let tabs: Vec<TabItem> = {
            use slint::Model;
            ui.get_tabs().iter().collect()
        };
        let mut x_cursor = 10.0_f32;
        const TAB_SPACING: f32 = 2.0;
        for tab in &tabs {
            let title = tab.title.to_string();
            let approx_w = ((title.chars().count() as f32) * 7.0 + 50.0).clamp(132.0, 240.0);
            let tab_left = x_cursor;
            let tab_right = x_cursor + approx_w;
            if lx >= tab_left && lx <= tab_right {
                let path = tab.path.to_string();
                if !path.is_empty() && std::path::Path::new(&path).is_dir() {
                    file_drag::log(&format!(
                        "pick_drop_destination: TAB '{}' -> '{}'",
                        title, path
                    ));
                    return path;
                }
            }
            x_cursor = tab_right + TAB_SPACING;
        }
    }

    // 0b. Breadcrumb drops route into that ancestor folder. The address bar
    //     sits in the toolbar at y in [title_h + 6, title_h + 37] (normal) or
    //     y in [title_h + 8, title_h + 38] (simple), starting at x = sidebar_w
    //     + 167 (normal) or x = sidebar_w + 390 (simple). Each crumb chip is
    //     roughly char-count * 7 + 18 px wide; we walk them in order summing
    //     widths to find the cursor's chip.
    let addr_y_top = if is_simple {
        title_h + 8.0
    } else {
        title_h + 6.0
    };
    let addr_y_bot = if is_simple {
        title_h + 38.0
    } else {
        title_h + 37.0
    };
    let addr_x_start = if is_simple {
        let toolbar_w = (ui.window().size().width as f32 / scale - sidebar_w).max(1.0);
        sidebar_w + toolbar_simple_addr_x(toolbar_w)
    } else {
        sidebar_w + 167.0
    };
    if ly >= addr_y_top && ly <= addr_y_bot && lx >= addr_x_start {
        let crumbs: Vec<ChoiceItem> = {
            use slint::Model;
            ui.get_breadcrumbs().iter().collect()
        };
        // Crumbs are inset 6 px inside the address bar Rectangle. They're
        // ChoiceItems (id = path, label = display name) built by build_breadcrumbs.
        let mut x_cursor = addr_x_start + 6.0;
        const CRUMB_GAP: f32 = 0.0;
        for crumb in &crumbs {
            let label = crumb.label.to_string();
            let chip_w = ((label.chars().count() as f32) * 7.0 + 24.0).max(18.0);
            let crumb_left = x_cursor;
            let crumb_right = x_cursor + chip_w;
            if lx >= crumb_left && lx <= crumb_right {
                let path = crumb.id.to_string();
                if !path.is_empty() && std::path::Path::new(&path).is_dir() {
                    file_drag::log(&format!(
                        "pick_drop_destination: BREADCRUMB '{}' -> '{}'",
                        label, path
                    ));
                    return path;
                }
            }
            x_cursor = crumb_right + CRUMB_GAP;
        }
    }

    // 1. Sidebar drops route by hit-testing rows. Each SideRow is 32 px tall,
    //    starting 14 px below the title bar (matches main.slint sidebar layout).
    if lx >= 0.0 && lx < sidebar_w && ly >= title_h {
        let list_top = title_h + 14.0;
        if ly >= list_top {
            let row = ((ly - list_top) / 32.0).floor() as isize;
            if row >= 0 {
                let items: Vec<SideItem> = {
                    use slint::Model;
                    let model: ModelRc<SideItem> = if ui.get_ui_mode().as_str() == "simple" {
                        ui.get_side_items_simple()
                    } else {
                        ui.get_side_items()
                    };
                    model.iter().collect()
                };
                if let Some(item) = items.get(row as usize) {
                    let path = item.path.to_string();
                    let is_header = item.is_header;
                    if !is_header && !path.is_empty() && std::path::Path::new(&path).is_dir() {
                        file_drag::log(&format!(
                            "pick_drop_destination: SIDEBAR row {} -> '{}'",
                            row, path
                        ));
                        return path;
                    }
                }
            }
        }
        // Sidebar hit but not on a usable item - fall through to pane logic below
        // so the user still gets a sensible destination instead of an empty path.
    }

    // 2. File-pane drops. Determine which pane the cursor is over (primary or
    //    secondary in dual mode) and hit-test against folder rows there.
    let pad = ui.global::<AppMetrics>().get_pad();
    let list_header_h = if ui.get_view_mode().as_str() == "list" {
        32.0
    } else {
        pad
    };
    let primary_list_top = content_top + pad + list_header_h;
    let secondary_list_top = content_top + 38.0 + list_header_h;

    let (base_dir, pane_left, pane_w, files, list_top, scroll_y, sort_by, pane_label): (
        String,
        f32,
        f32,
        &[FileEntry],
        f32,
        f32,
        String,
        &'static str,
    ) = if ui.get_dual_pane() {
        let split_x = sidebar_w + primary_w + splitter_w;
        if lx < split_x {
            (
                ctrl.current_path.clone(),
                main_left,
                primary_w,
                &ctrl.visible_files[..],
                primary_list_top,
                pane_scroll_y(ui, false, ui.get_view_mode().as_str()),
                ctrl.sort_by.clone(),
                "primary",
            )
        } else if ctrl.secondary_path.is_empty() {
            file_drag::log("pick_drop_destination: secondary pane has no folder open");
            return String::new();
        } else {
            let sec_left = sidebar_w + primary_w + splitter_w;
            let sec_w = ui.get_secondary_pane_w();
            (
                ctrl.secondary_path.clone(),
                sec_left,
                sec_w,
                &ctrl.secondary_visible_files[..],
                secondary_list_top,
                pane_scroll_y(ui, true, ui.get_view_mode().as_str()),
                ctrl.secondary_sort_by.clone(),
                "secondary",
            )
        }
    } else {
        (
            ctrl.current_path.clone(),
            main_left,
            primary_w,
            &ctrl.visible_files[..],
            primary_list_top,
            pane_scroll_y(ui, false, ui.get_view_mode().as_str()),
            ctrl.sort_by.clone(),
            "primary",
        )
    };

    if let Some(sub) = hit_test_list_folder_drop(
        ui,
        files,
        pane_left,
        pane_w,
        lx,
        ly,
        list_top,
        scroll_y,
        &sort_by,
    ) {
        file_drag::log(&format!(
            "pick_drop_destination: pane={} ROW -> '{}'",
            pane_label, sub
        ));
        return sub;
    }
    file_drag::log(&format!(
        "pick_drop_destination: pane={} BACKGROUND -> '{}'",
        pane_label, base_dir
    ));
    base_dir
}

// Fallback HWND lookup: enumerate all top-level windows owned by the current
// thread and return the first visible one. Used when `with_winit_window`
// returns silently (e.g. timing race during deferred IDropTarget registration).
#[cfg(target_os = "windows")]
fn find_pathfinder_hwnd() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, IsWindowVisible};
    use windows::core::BOOL;

    struct Found(Option<HWND>);
    let mut found = Found(None);

    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            if IsWindowVisible(hwnd).as_bool() {
                let found = &mut *(lparam.0 as *mut Found);
                if found.0.is_none() {
                    found.0 = Some(hwnd);
                    return BOOL(0); // stop enumeration
                }
            }
            BOOL(1)
        }
    }

    unsafe {
        let tid = GetCurrentThreadId();
        let _ = EnumThreadWindows(tid, Some(cb), LPARAM(&mut found as *mut _ as isize));
    }
    found.0
}

#[cfg(target_os = "windows")]
fn set_as_default_file_manager() -> Result<(), String> {
    folder_shell_registry::set_pathfinder_as_default_folder_handler()
}

#[cfg(target_os = "windows")]
fn is_default_file_manager_registered() -> bool {
    folder_shell_registry::pathfinder_is_default_folder_handler()
}

#[cfg(target_os = "windows")]
fn restore_as_default_file_manager() -> Result<(), String> {
    folder_shell_registry::restore_windows_default_folder_handler()
}

#[cfg(target_os = "windows")]
fn generate_registry_file() -> Result<String, String> {
    folder_shell_registry::generate_registry_file_content()
}

#[cfg(target_os = "windows")]
fn get_handler_registration_status() -> Result<(usize, usize), String> {
    folder_shell_registry::verify_shell_handler_entries()
}

#[cfg(not(target_os = "windows"))]
fn set_as_default_file_manager() -> Result<(), String> {
    Err("Windows only".into())
}

#[cfg(not(target_os = "windows"))]
fn restore_as_default_file_manager() -> Result<(), String> {
    Err("Windows only".into())
}

#[cfg(not(target_os = "windows"))]
fn generate_registry_file() -> Result<String, String> {
    Err("Windows only".into())
}

#[cfg(not(target_os = "windows"))]
fn get_handler_registration_status() -> Result<(usize, usize), String> {
    Err("Windows only".into())
}

#[tauri::command]
fn set_default_file_manager() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        set_as_default_file_manager()?;
        Ok("Pathfinder is now set as the default file manager.".to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("This feature is only available on Windows.".to_string())
    }
}

#[tauri::command]
fn export_registry_file() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let content = generate_registry_file()?;
        // Prefer Downloads, fall back to the desktop, then home. CWD for an
        // installed app is typically C:\Windows\System32 which would silently
        // bury the file somewhere the user can't find it.
        let out_dir = dirs::download_dir()
            .or_else(dirs::desktop_dir)
            .or_else(dirs::home_dir)
            .ok_or_else(|| "Could not locate a writable user folder for the export.".to_string())?;
        let file_path = out_dir.join("pathfinder-folder-handler.reg");
        std::fs::write(&file_path, &content)
            .map_err(|e| format!("Failed to write registry file: {e}"))?;
        Ok(format!(
            "Registry file exported to: {}",
            file_path.display()
        ))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("This feature is only available on Windows.".to_string())
    }
}

#[tauri::command]
fn check_handler_registration() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let (valid, total) = get_handler_registration_status()?;
        let status = if valid == total {
            format!(
                "[ok] Pathfinder is properly registered as the default handler ({}/{})",
                valid, total
            )
        } else if valid > 0 {
            format!(
                "[!] Partial registration: {}/{} registry entries configured. \
                 Click 'Set as default' to complete the registration.",
                valid, total
            )
        } else {
            "[x] Not registered. Click 'Set as default' to configure Pathfinder \
             as your default file manager."
                .to_string()
        };
        Ok(status)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("This feature is only available on Windows.".to_string())
    }
}

/// Installer / uninstaller hooks call these flags so registry logic stays in one place.
fn handle_shell_handler_cli_flags() -> bool {
    // Single pass over argv. Any other CLI parsing already happens later in
    // run(); these flags short-circuit before COM init for headless use.
    let mut install = false;
    let mut uninstall = false;
    for a in std::env::args().skip(1) {
        if a == "--install-shell-handler" {
            install = true;
        } else if a == "--uninstall-shell-handler" {
            uninstall = true;
        }
    }
    #[cfg(target_os = "windows")]
    {
        if install {
            match folder_shell_registry::set_pathfinder_as_default_folder_handler() {
                Ok(()) => eprintln!("[pathfinder] Shell handler registered (HKCU)."),
                Err(e) => {
                    eprintln!("[pathfinder] Shell handler registration failed: {e}");
                    std::process::exit(1);
                }
            }
            return true;
        }
        if uninstall {
            match folder_shell_registry::restore_windows_default_folder_handler() {
                Ok(()) => eprintln!("[pathfinder] Shell handler removed (HKCU)."),
                Err(e) => {
                    eprintln!("[pathfinder] Shell handler removal failed: {e}");
                    std::process::exit(1);
                }
            }
            return true;
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if install || uninstall {
            eprintln!("[pathfinder] Shell handler flags are Windows-only.");
            std::process::exit(1);
        }
    }
    false
}

pub fn run() {
    if handle_shell_handler_cli_flags() {
        return;
    }

    // COM must be initialised on the main thread before any shell APIs are used.
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let _ = slint::platform::set_platform(Box::new(
        i_slint_backend_winit::Backend::new().expect("failed to create Slint winit backend"),
    ));

    let initial_settings: NativeSettings =
        read_native_json("settings.json", NativeSettings::default());
    let ui = MainWindow::new().expect("failed to create Pathfinder window");
    configure_native_window(&ui, &initial_settings);
    let cli_folder = parse_cli_startup_folder();
    // Pass the already-loaded settings into the controller so it doesn't pay
    // a second disk read for the same file. The other state files are loaded
    // in parallel inside NativeController::new via rayon::join.
    let controller = Rc::new(RefCell::new(NativeController::new(
        cli_folder,
        initial_settings,
    )));
    controller.borrow_mut().initialize_ui(&ui);
    wire_native_callbacks(&ui, controller.clone());

    // First-run welcome: shown until the user clicks "Got it". Save flag in
    // settings so we never repeat. We pre-populate the dialog state but
    // intentionally suppress display while the Simple/Normal UI mode prompt is
    // open, so the two never overlap. If the UI mode prompt is up, the welcome
    // is opened from on_ui_mode_prompt_choice once the user picks a mode.
    if !controller.borrow().settings.first_run_welcome_dismissed {
        #[cfg(target_os = "windows")]
        {
            if is_default_file_manager_registered() {
                ui.set_welcome_default_handler_set(true);
                ui.set_welcome_default_status(ss(
                    "Already registered. Pathfinder is your default folder handler.",
                ));
            }
        }
        if !ui.get_ui_mode_prompt_visible() {
            ui.set_welcome_visible(true);
        }
    }

    // Detect NPU/AI capabilities in background; update UI labels once done
    let weak_ui = ui.as_weak();
    std::thread::spawn(move || {
        let caps = compute_ai_capabilities();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak_ui.upgrade() {
                ui.set_ai_device(SharedString::from(&caps.reason));
                ui.set_ai_gpu_status(SharedString::from(&caps.gpu_summary));
                ui.set_ai_label(SharedString::from(ai_status_label(&caps)));
            }
        });
    });

    ui.show().expect("failed to show Pathfinder window");
    controller.borrow_mut().finish_startup(&ui);
    apply_mica(&ui);
    register_winit_mouse_side_button_navigation(&ui);
    install_mouse_nav(&ui);

    // Register IDropTarget so files dropped from Explorer land in the current folder.
    //
    // We DEFER this via a one-shot Slint Timer because calling `with_winit_window`
    // synchronously right after `ui.show()` returns silently - the winit Window
    // object isn't fully reachable through Slint's accessor until the event loop
    // has pumped at least one tick. The Timer callback runs on the UI thread
    // (no Send bound), so we can pass our Rc<RefCell<NativeController>> in.
    // Reduced from 150ms to 50ms for faster startup on modern systems.
    #[cfg(target_os = "windows")]
    {
        let weak_drop = ui.as_weak();
        let c_drop = controller.clone();
        let drop_register_timer = Box::new(slint::Timer::default());
        drop_register_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(50),
            move || {
                use i_slint_backend_winit::WinitWindowAccessor;
                use i_slint_backend_winit::winit::raw_window_handle::{
                    HasWindowHandle, RawWindowHandle,
                };
                use windows::Win32::Foundation::HWND;

                eprintln!("[file_drag] deferred registration starting");
                let Some(ui) = weak_drop.upgrade() else {
                    eprintln!("[file_drag] UI gone before registration");
                    return;
                };
                let weak_inner = ui.as_weak();
                let c_inner = c_drop.clone();
                let result = ui.window().with_winit_window(|window| {
                    let handle = match window.window_handle() {
                        Ok(h) => h,
                        Err(e) => {
                            eprintln!("[file_drag] window_handle err: {:?}", e);
                            return None;
                        }
                    };
                    let h = match handle.as_raw() {
                        RawWindowHandle::Win32(h) => h,
                        other => {
                            eprintln!("[file_drag] unexpected handle: {:?}", other);
                            return None;
                        }
                    };
                    let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
                    eprintln!("[file_drag] got HWND from winit: {:?}", hwnd.0);
                    Some(hwnd)
                });
                let hwnd_opt = result.flatten().or_else(|| {
                    eprintln!("[file_drag] with_winit_window failed; using fallback");
                    find_pathfinder_hwnd()
                });
                let Some(hwnd) = hwnd_opt else {
                    startup_warning_toast(
                        weak_drop.clone(),
                        "Drag-and-drop from Explorer is unavailable (could not attach to the window)."
                            .to_string(),
                    );
                    return;
                };
                // Seed the ghost-overlay label whenever a drag enters the window
                // from an external source (Explorer, another app). Internal drags
                // already set the label in on_start_file_drag because we know the
                // file names at drag-start without waiting for DragEnter.
                let weak_label = ui.as_weak();
                file_drag::register_drag_paths_handler(move |paths| {
                    let Some(ui) = weak_label.upgrade() else {
                        return;
                    };
                    if paths.is_empty() {
                        ui.set_drag_label(SharedString::from(""));
                        ui.global::<ThemePalette>().set_is_dragging(false);
                        ui.global::<ThemePalette>().set_drag_count(0);
                        clear_drag_ghost_ui(&ui);
                        return;
                    }
                    let first_name = std::path::Path::new(&paths[0])
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| paths[0].clone());
                    let label = if paths.len() == 1 {
                        first_name
                    } else {
                        format!("{first_name} + {} more", paths.len() - 1)
                    };
                    ui.set_drag_label(SharedString::from(label));
                    ui.global::<ThemePalette>()
                        .set_drag_count(paths.len() as i32);
                    ui.global::<ThemePalette>().set_is_dragging(true);
                    seed_drag_ghost_from_paths(&ui, &paths);
                });

                // Highlight the destination pane AND the exact target folder
                // while dragging - fires every DragOver tick on the UI thread.
                // pick_drop_destination resolves the cursor position to the
                // folder path that a drop would land in (sidebar entry, subfolder
                // row, or pane background) and we surface that as drag_target_path
                // so the slint side can put a glow on the row or sidebar item.
                let weak_hover = ui.as_weak();
                let c_hover = controller.clone();
                file_drag::register_drag_over_handler(move |is_active, screen_x, screen_y| {
                    let Some(ui) = weak_hover.upgrade() else {
                        return;
                    };
                    if !is_active {
                        ui.set_drag_over_pane(SharedString::from(""));
                        ui.set_drag_target_path(SharedString::from(""));
                        return;
                    }
                    let (logical_x, logical_y) =
                        drag_cursor_logical_from_screen_pt(&ui, hwnd, screen_x, screen_y);
                    ui.set_drag_cursor_x(logical_x.max(0.0));
                    ui.set_drag_cursor_y(logical_y.max(0.0));
                    let pane_label = if ui.get_dual_pane() {
                        let split_x = ui.get_sidebar_w()
                            + ui.get_primary_pane_w()
                            + ui.get_pane_splitter_w();
                        if logical_x < split_x {
                            "primary"
                        } else {
                            "secondary"
                        }
                    } else {
                        let sw = ui.get_sidebar_w();
                        if logical_x >= sw { "primary" } else { "" }
                    };
                    ui.set_drag_over_pane(SharedString::from(pane_label));
                    // Resolve the precise drop destination for the highlight glow.
                    // borrow() here can fail if a previous mutation is mid-flight;
                    // skip the row/sidebar glow update in that case rather than
                    // panic.
                    if let Ok(ctrl) = c_hover.try_borrow() {
                        let target = pick_drop_destination(&ui, &ctrl, hwnd, screen_x, screen_y);
                        ui.set_drag_target_path(SharedString::from(target));
                    }
                    // DoDragDrop blocks the Slint event loop; nudge a redraw so
                    // the ghost overlay and drop highlights track the cursor.
                    ui.window().request_redraw();
                });

                if let Some(dt) = file_drag::register_drop_target(
                    hwnd,
                    move |paths, is_move, screen_x, screen_y| {
                        if let Some(ui) = weak_inner.upgrade() {
                            ui.set_drag_over_pane(SharedString::from(""));
                            // Determine destination pane from drop coordinates.
                            // ScreenToClient gives raw client pixels; Slint uses
                            // logical (DPI-scaled) units for its pane widths.
                            let dest_dir = pick_drop_destination(
                                &ui,
                                &c_inner.borrow(),
                                hwnd,
                                screen_x,
                                screen_y,
                            );
                            eprintln!(
                                "[file_drag] dropping {} item(s) into '{}' (move={})",
                                paths.len(),
                                dest_dir,
                                is_move
                            );
                            c_inner
                                .borrow_mut()
                                .drop_files_from_drag(&ui, paths, is_move, dest_dir);
                        }
                    },
                ) {
                    std::mem::forget(dt);
                    eprintln!("[file_drag] drop target installed");
                } else {
                    eprintln!("[file_drag] register_drop_target returned None");
                }
            },
        );
        // Leak so the Timer stays alive long enough to fire.
        Box::leak(drop_register_timer);
    }

    // Auto-update: HTTPS via `ureq` (no PowerShell). Thread starts here so we
    // sit right before `run_event_loop`; we wait until the loop is accepting
    // `invoke_from_event_loop` before the first GitHub check (fixes "never on launch").
    let weak_ui_upd = ui.as_weak();
    std::thread::spawn(move || {
        updater_log(&format!(
            "background updater thread started; current={} api={}",
            env!("CARGO_PKG_VERSION"),
            GITHUB_LATEST_RELEASE_API
        ));
        if !wait_until_slint_event_loop_ready(Duration::from_secs(45)) {
            updater_log(
                "updater: Slint event loop did not become ready in 45s; aborting updater thread",
            );
            return;
        }
        updater_log("updater: event loop ready, first GitHub check");
        let mut consecutive_failures: u32 = 0;
        loop {
            updater_log("checking github for new release");
            match check_github_release_now() {
                Ok(result) => {
                    updater_log(&format!(
                        "result: latest={} current={} available={} download_url={}",
                        result.latest_version,
                        result.current_version,
                        result.available,
                        if result.download_url.is_empty() {
                            "<none>"
                        } else {
                            result.download_url.as_str()
                        }
                    ));
                    if result.available {
                        let ver = SharedString::from(result.latest_version.clone());
                        let dl = SharedString::from(result.download_url.clone());
                        let weak_pill = weak_ui_upd.clone();
                        let r = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak_pill.upgrade() {
                                ui.set_update_available(true);
                                ui.set_update_version(ver);
                                ui.set_update_download_url(dl);
                            }
                        });
                        if r.is_err() {
                            updater_log("invoke_from_event_loop failed - will retry next cycle");
                        } else {
                            updater_log("pill set on UI thread");
                        }
                    }
                    consecutive_failures = 0;
                    std::thread::sleep(std::time::Duration::from_secs(60 * 60));
                }
                Err(e) => {
                    updater_log(&format!("check failed: {e}"));
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let wait_secs = match consecutive_failures {
                        1 => 15,
                        2 => 60,
                        3 => 300,
                        _ => 60 * 60,
                    };
                    updater_log(&format!(
                        "retrying in {wait_secs}s (failure {consecutive_failures})"
                    ));
                    std::thread::sleep(std::time::Duration::from_secs(wait_secs));
                }
            }
        }
    });

    slint::run_event_loop().expect("error while running Slint event loop");
}
