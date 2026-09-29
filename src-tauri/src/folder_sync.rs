//! Reviewed, one-way folder synchronization. Planning never writes to either tree.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const MAX_ENTRIES: usize = 200_000;
const COPY_BUFFER: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SyncAction {
    CreateDir,
    Copy,
    Update,
    DeleteFile,
    DeleteDir,
    Conflict,
    Same,
    KeepExtra,
}

impl SyncAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::CreateDir => "create folder",
            Self::Copy => "copy",
            Self::Update => "update",
            Self::DeleteFile | Self::DeleteDir => "delete (back up)",
            Self::Conflict => "conflict — skipped",
            Self::Same => "same",
            Self::KeepExtra => "extra — kept",
        }
    }

    fn will_write(self) -> bool {
        matches!(
            self,
            Self::CreateDir | Self::Copy | Self::Update | Self::DeleteFile | Self::DeleteDir
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    kind: EntryKind,
    len: u64,
    modified_ns: u128,
    created_ns: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum EntryKind {
    File,
    Directory,
    Link,
}

#[derive(Clone, Debug)]
pub struct SyncItem {
    pub relative: PathBuf,
    pub action: SyncAction,
    pub conflict_reason: Option<String>,
    pub source_size: u64,
    pub destination_size: u64,
    source: Option<Fingerprint>,
    destination: Option<Fingerprint>,
    source_hash: Option<String>,
    destination_hash: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SyncPlan {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub exclusions: String,
    pub delete_extras: bool,
    pub items: Vec<SyncItem>,
    pub excluded: usize,
    pub bytes_to_copy: u64,
    cache: HashMap<String, CacheRow>,
}

impl SyncPlan {
    pub fn count(&self, action: SyncAction) -> usize {
        self.items
            .iter()
            .filter(|item| item.action == action)
            .count()
    }

    pub fn pending_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.action.will_write())
            .count()
    }

    pub fn summary(&self) -> String {
        format!(
            "{} copies, {} updates, {} new folders, {} deletions, {} extras kept, {} conflicts skipped, {} excluded · {} to copy",
            self.count(SyncAction::Copy),
            self.count(SyncAction::Update),
            self.count(SyncAction::CreateDir),
            self.count(SyncAction::DeleteFile) + self.count(SyncAction::DeleteDir),
            self.count(SyncAction::KeepExtra),
            self.count(SyncAction::Conflict),
            self.excluded,
            short_size(self.bytes_to_copy),
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheRow {
    source: Fingerprint,
    destination: Fingerprint,
    hash: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FingerprintCache {
    rows: HashMap<String, CacheRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub relative: String,
    pub action: String,
    pub status: String,
    pub detail: String,
    pub previous_version: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncReport {
    pub run_id: String,
    pub source: String,
    pub destination: String,
    pub started_at: u64,
    pub finished_at: u64,
    pub completed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub versions_dir: String,
    pub outcomes: Vec<SyncOutcome>,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        format!(
            "Sync finished: {} completed, {} skipped, {} failed{}; previous versions: {}",
            self.completed,
            self.skipped,
            self.failed,
            if self.cancelled { ", cancelled" } else { "" },
            self.versions_dir
        )
    }
}

fn short_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn unix_ns(value: std::io::Result<SystemTime>) -> u128 {
    value
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn fingerprint(path: &Path) -> Result<Option<Fingerprint>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    let kind = if metadata.file_type().is_symlink() || reparse {
        EntryKind::Link
    } else if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Link
    };
    Ok(Some(Fingerprint {
        kind,
        len: if kind == EntryKind::File {
            metadata.len()
        } else {
            0
        },
        modified_ns: unix_ns(metadata.modified()),
        created_ns: unix_ns(metadata.created()),
    }))
}

fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("Folder sync cancelled.".to_string())
    } else {
        Ok(())
    }
}

fn root_contains(root: &Path, other: &Path) -> bool {
    #[cfg(windows)]
    {
        let root = root.to_string_lossy().replace('/', "\\").to_lowercase();
        let other = other.to_string_lossy().replace('/', "\\").to_lowercase();
        other == root || other.starts_with(&(root.trim_end_matches('\\').to_string() + "\\"))
    }
    #[cfg(not(windows))]
    {
        other.starts_with(root)
    }
}

fn validate_roots(source: &Path, destination: &Path) -> Result<(PathBuf, PathBuf), String> {
    let source = fs::canonicalize(source).map_err(|e| format!("Source folder: {e}"))?;
    let destination =
        fs::canonicalize(destination).map_err(|e| format!("Destination folder: {e}"))?;
    if !source.is_dir() || !destination.is_dir() {
        return Err("Source and destination must both be existing folders.".to_string());
    }
    if root_contains(&source, &destination) || root_contains(&destination, &source) {
        return Err("Sync folders must be separate; neither may contain the other.".to_string());
    }
    let history = history_dir(&destination)?;
    if root_contains(&source, &history) || root_contains(&destination, &history) {
        return Err("Sync history would be inside a synced folder.".to_string());
    }
    Ok((source, destination))
}

pub fn history_dir(destination: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(destination).map_err(|e| e.to_string())?;
    let parent = canonical.parent().ok_or("Destination has no parent")?;
    let identity = canonical.to_string_lossy().into_owned();
    #[cfg(windows)]
    let identity = identity.to_lowercase();
    let digest = Sha256::digest(identity.as_bytes());
    Ok(parent
        .join(".pathfinder-sync-history")
        .join(hex::encode(&digest[..8])))
}

fn relative_key(relative: &Path) -> String {
    relative.to_string_lossy().replace('\\', "/")
}

fn comparison_key(relative: &Path) -> String {
    let key = relative_key(relative);
    #[cfg(windows)]
    {
        key.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        key
    }
}

fn simple_glob(pattern: &str, value: &str) -> bool {
    // A bounded component matcher: '*' and '?' never cross a path separator.
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star, mut retry) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(position) = star {
            retry += 1;
            v = retry;
            p = position + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn is_excluded(relative: &Path, patterns: &[String]) -> bool {
    let key = relative_key(relative);
    #[cfg(windows)]
    let key = key.to_lowercase();
    let parts: Vec<&str> = key.split('/').collect();
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().replace('\\', "/");
        #[cfg(windows)]
        let pattern = pattern.to_lowercase();
        if pattern.is_empty() {
            return false;
        }
        if let Some(prefix) = pattern.strip_suffix("/**") {
            return key == prefix || key.starts_with(&(prefix.to_string() + "/"));
        }
        if pattern.contains('/') {
            let rule_parts: Vec<&str> = pattern.split('/').collect();
            rule_parts.len() <= parts.len()
                && rule_parts
                    .iter()
                    .zip(&parts)
                    .all(|(rule, part)| simple_glob(rule, part))
                && (rule_parts.len() == parts.len() || !pattern.contains('*'))
        } else {
            parts.iter().any(|part| simple_glob(&pattern, part))
        }
    })
}

fn scan_tree(
    root: &Path,
    patterns: &[String],
    cancel: &AtomicBool,
) -> Result<(BTreeMap<String, (PathBuf, Fingerprint)>, usize), String> {
    let mut entries = BTreeMap::new();
    let mut excluded = 0;
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let omit = is_excluded(
                entry.path().strip_prefix(root).unwrap_or(entry.path()),
                patterns,
            );
            if omit {
                excluded += 1;
            }
            !omit
        });
    for entry in walker {
        cancelled(cancel)?;
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry.path().strip_prefix(root).map_err(|e| e.to_string())?;
        if entries.len() >= MAX_ENTRIES {
            return Err(format!(
                "Sync preview exceeds {MAX_ENTRIES} entries; narrow the folders or add exclusions."
            ));
        }
        if let Some(metadata) = fingerprint(entry.path())? {
            let key = comparison_key(relative);
            if entries
                .insert(key, (relative.to_path_buf(), metadata))
                .is_some()
            {
                return Err(format!(
                    "Folder contains names that collide under Windows path comparison: {}",
                    relative.display()
                ));
            }
        }
    }
    Ok((entries, excluded))
}

fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let mut input = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        cancelled(cancel)?;
        let size = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn cache_file(destination: &Path) -> Result<PathBuf, String> {
    Ok(history_dir(destination)?.join("fingerprints.json"))
}

fn read_cache(destination: &Path) -> FingerprintCache {
    cache_file(destination)
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path.parent().ok_or("Output path has no parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = File::create(&temp).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        use windows::core::PCWSTR;
        let source: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let dest: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(dest.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    fs::rename(&temp, path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn plan(
    source: &Path,
    destination: &Path,
    exclusions: &str,
    delete_extras: bool,
    cancel: &AtomicBool,
) -> Result<SyncPlan, String> {
    let (source, destination) = validate_roots(source, destination)?;
    let patterns: Vec<String> = exclusions
        .split([',', ';', '\n'])
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(str::to_string)
        .collect();
    let (source_entries, source_excluded) = scan_tree(&source, &patterns, cancel)?;
    let (destination_entries, destination_excluded) = scan_tree(&destination, &patterns, cancel)?;
    let previous = read_cache(&destination);
    let mut cache = HashMap::new();
    let mut items = Vec::new();
    let mut blocked_paths = HashSet::new();
    let mut bytes_to_copy = 0u64;
    let all_paths: BTreeSet<String> = source_entries
        .keys()
        .chain(destination_entries.keys())
        .cloned()
        .collect();
    for key in &all_paths {
        cancelled(cancel)?;
        let blocked_by_parent = key
            .match_indices('/')
            .any(|(index, _)| blocked_paths.contains(&key[..index]));
        let left_entry = source_entries.get(key);
        let right_entry = destination_entries.get(key);
        let relative = left_entry
            .or(right_entry)
            .map(|(path, _)| path)
            .ok_or("Missing planned path")?;
        let left = left_entry.map(|(_, fingerprint)| fingerprint);
        let right = right_entry.map(|(_, fingerprint)| fingerprint);
        let mut source_hash = None;
        let mut destination_hash = None;
        let action = if blocked_by_parent
            || (left_entry.is_some()
                && right_entry.is_some()
                && left_entry.map(|entry| &entry.0) != right_entry.map(|entry| &entry.0))
        {
            SyncAction::Conflict
        } else {
            match (left, right) {
                (Some(left), Some(right))
                    if left.kind == EntryKind::Directory && right.kind == EntryKind::Directory =>
                {
                    SyncAction::Same
                }
                (Some(left), Some(right))
                    if left.kind == EntryKind::File && right.kind == EntryKind::File =>
                {
                    if let Some(row) = previous
                        .rows
                        .get(key)
                        .filter(|row| &row.source == left && &row.destination == right)
                    {
                        cache.insert(key.clone(), row.clone());
                        SyncAction::Same
                    } else {
                        let left_hash = hash_file(&source.join(relative), cancel)?;
                        let right_hash = hash_file(&destination.join(relative), cancel)?;
                        source_hash = Some(left_hash.clone());
                        destination_hash = Some(right_hash.clone());
                        if left_hash == right_hash {
                            cache.insert(
                                key.clone(),
                                CacheRow {
                                    source: left.clone(),
                                    destination: right.clone(),
                                    hash: left_hash,
                                },
                            );
                            SyncAction::Same
                        } else if right.modified_ns > left.modified_ns {
                            SyncAction::Conflict
                        } else {
                            bytes_to_copy = bytes_to_copy.saturating_add(left.len);
                            SyncAction::Update
                        }
                    }
                }
                (Some(left), None) if left.kind == EntryKind::Directory => SyncAction::CreateDir,
                (Some(left), None) if left.kind == EntryKind::File => {
                    source_hash = Some(hash_file(&source.join(relative), cancel)?);
                    bytes_to_copy = bytes_to_copy.saturating_add(left.len);
                    SyncAction::Copy
                }
                (None, Some(right)) if delete_extras && right.kind == EntryKind::File => {
                    destination_hash = Some(hash_file(&destination.join(relative), cancel)?);
                    SyncAction::DeleteFile
                }
                (None, Some(right)) if delete_extras && right.kind == EntryKind::Directory => {
                    SyncAction::DeleteDir
                }
                (None, Some(_)) => SyncAction::KeepExtra,
                _ => SyncAction::Conflict,
            }
        };
        if action == SyncAction::Conflict {
            blocked_paths.insert(key.clone());
        }
        let conflict_reason = if action != SyncAction::Conflict {
            None
        } else if blocked_by_parent {
            Some("A parent path conflicts; its children are blocked.".to_string())
        } else if left_entry.is_some()
            && right_entry.is_some()
            && left_entry.map(|entry| &entry.0) != right_entry.map(|entry| &entry.0)
        {
            Some("Names differ only by case on this filesystem.".to_string())
        } else if left.is_some_and(|entry| entry.kind == EntryKind::Link)
            || right.is_some_and(|entry| entry.kind == EntryKind::Link)
        {
            Some("A link or reparse point needs manual review.".to_string())
        } else if left
            .zip(right)
            .is_some_and(|(source, destination)| source.kind != destination.kind)
        {
            Some("Source and destination have different item types.".to_string())
        } else if left
            .zip(right)
            .is_some_and(|(source, destination)| destination.modified_ns > source.modified_ns)
        {
            Some("Destination has newer, different content.".to_string())
        } else {
            Some("This item cannot be synchronized safely.".to_string())
        };
        items.push(SyncItem {
            relative: relative.clone(),
            action,
            conflict_reason,
            source_size: left.map_or(0, |entry| entry.len),
            destination_size: right.map_or(0, |entry| entry.len),
            source: left.cloned(),
            destination: right.cloned(),
            source_hash,
            destination_hash,
        });
    }
    items.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(SyncPlan {
        source,
        destination,
        exclusions: exclusions.to_string(),
        delete_extras,
        items,
        excluded: source_excluded + destination_excluded,
        bytes_to_copy,
        cache,
    })
}

fn validate_relative(relative: &Path) -> Result<(), String> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("Unsafe sync path: {}", relative.display()));
    }
    Ok(())
}

#[cfg(windows)]
struct SyncLock(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for SyncLock {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::ReleaseMutex;
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
struct SyncLock(PathBuf);

#[cfg(not(windows))]
impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn acquire_sync_lock(destination: &Path) -> Result<SyncLock, String> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        use windows::core::PCWSTR;
        let digest = Sha256::digest(destination.to_string_lossy().to_lowercase().as_bytes());
        let name = format!("Local\\PathfinderSync-{}", hex::encode(&digest[..12]));
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }
            .map_err(|e| e.to_string())?;
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            Ok(SyncLock(handle))
        } else {
            unsafe {
                let _ = CloseHandle(handle);
            }
            Err("Another sync is already writing to this destination.".into())
        }
    }
    #[cfg(not(windows))]
    {
        let path = history_dir(destination)?.join("sync.lock");
        fs::create_dir_all(path.parent().ok_or("Lock has no parent")?)
            .map_err(|e| e.to_string())?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| "Another sync is already writing to this destination.".to_string())?;
        Ok(SyncLock(path))
    }
}

fn ensure_safe_parent(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    validate_relative(relative)?;
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for part in parent.components() {
            let Component::Normal(name) = part else {
                return Err("Unsafe path component.".into());
            };
            current.push(name);
            match fingerprint(&current)? {
                Some(info) if info.kind == EntryKind::Directory => {}
                Some(_) => {
                    return Err(format!(
                        "Folder changed or links outside sync root: {}",
                        current.display()
                    ));
                }
                None => {
                    fs::create_dir(&current).map_err(|e| format!("{}: {e}", current.display()))?
                }
            }
        }
    }
    Ok(root.join(relative))
}

fn verify_item(plan: &SyncPlan, item: &SyncItem, cancel: &AtomicBool) -> Result<(), String> {
    let source = plan.source.join(&item.relative);
    let destination = plan.destination.join(&item.relative);
    let matches =
        |expected: &Option<Fingerprint>, actual: &Option<Fingerprint>| match (expected, actual) {
            (Some(left), Some(right))
                if left.kind == EntryKind::Directory && right.kind == EntryKind::Directory =>
            {
                true
            }
            _ => expected == actual,
        };
    if !matches(&item.source, &fingerprint(&source)?)
        || !matches(&item.destination, &fingerprint(&destination)?)
    {
        return Err("Source or destination changed after preview; preview again.".to_string());
    }
    if let Some(expected) = &item.source_hash {
        if hash_file(&source, cancel)? != *expected {
            return Err("Source content changed after preview; preview again.".to_string());
        }
    }
    if let Some(expected) = &item.destination_hash {
        if hash_file(&destination, cancel)? != *expected {
            return Err("Destination content changed after preview; preview again.".to_string());
        }
    }
    Ok(())
}

fn copy_staged(
    source: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<(PathBuf, String), String> {
    let parent = destination.parent().ok_or("Destination has no parent")?;
    let name = destination
        .file_name()
        .ok_or("Destination has no name")?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let temp = parent.join(format!(
        ".{name}.pathfinder-sync-{}-{nonce}.part",
        std::process::id()
    ));
    let result = (|| {
        let mut input = File::open(source).map_err(|e| e.to_string())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; COPY_BUFFER];
        loop {
            cancelled(cancel)?;
            let amount = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if amount == 0 {
                break;
            }
            output
                .write_all(&buffer[..amount])
                .map_err(|e| e.to_string())?;
            hasher.update(&buffer[..amount]);
        }
        let metadata = input.metadata().map_err(|e| e.to_string())?;
        let modified = metadata.modified().map_err(|e| e.to_string())?;
        output
            .set_times(FileTimes::new().set_modified(modified))
            .map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
        drop(output);
        fs::set_permissions(&temp, metadata.permissions()).map_err(|e| e.to_string())?;
        Ok((temp.clone(), format!("{:x}", hasher.finalize())))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn version_path(versions: &Path, relative: &Path) -> Result<PathBuf, String> {
    ensure_safe_parent(versions, relative)
}

fn apply_item(
    plan: &SyncPlan,
    item: &SyncItem,
    versions: &Path,
    cancel: &AtomicBool,
) -> Result<Option<PathBuf>, String> {
    cancelled(cancel)?;
    verify_item(plan, item, cancel)?;
    let source = plan.source.join(&item.relative);
    let destination = ensure_safe_parent(&plan.destination, &item.relative)?;
    match item.action {
        SyncAction::CreateDir => {
            if destination.exists() {
                return Err("Destination appeared after preview.".into());
            }
            fs::create_dir(&destination).map_err(|e| e.to_string())?;
            Ok(None)
        }
        SyncAction::Copy | SyncAction::Update => {
            let (temp, copied_hash) = copy_staged(&source, &destination, cancel)?;
            if item
                .source_hash
                .as_ref()
                .is_some_and(|hash| hash != &copied_hash)
            {
                let _ = fs::remove_file(&temp);
                return Err("Source changed while copying; staged copy discarded.".into());
            }
            if item.action == SyncAction::Copy {
                if destination.exists() {
                    let _ = fs::remove_file(&temp);
                    return Err("Destination appeared after preview.".into());
                }
                if let Err(error) = fs::rename(&temp, &destination) {
                    let _ = fs::remove_file(&temp);
                    return Err(error.to_string());
                }
                Ok(None)
            } else {
                // Recheck just before the destructive step; keep the old version on the same volume.
                if let Err(error) = verify_item(plan, item, cancel) {
                    let _ = fs::remove_file(&temp);
                    return Err(error);
                }
                let previous = version_path(versions, &item.relative)?;
                fs::rename(&destination, &previous).map_err(|e| {
                    let _ = fs::remove_file(&temp);
                    e.to_string()
                })?;
                if let Err(error) = fs::rename(&temp, &destination) {
                    let restore = fs::rename(&previous, &destination);
                    let _ = fs::remove_file(&temp);
                    return Err(match restore {
                        Ok(()) => {
                            format!("Could not install new file; old version restored: {error}")
                        }
                        Err(restore_error) => format!(
                            "Could not install new file: {error}; old version remains at {} ({restore_error})",
                            previous.display()
                        ),
                    });
                }
                Ok(Some(previous))
            }
        }
        SyncAction::DeleteFile => {
            let previous = version_path(versions, &item.relative)?;
            fs::rename(&destination, &previous).map_err(|e| e.to_string())?;
            Ok(Some(previous))
        }
        SyncAction::DeleteDir => {
            let previous = version_path(versions, &item.relative)?;
            let created = match fingerprint(&previous)? {
                Some(info) if info.kind == EntryKind::Directory => false,
                Some(_) => return Err("Version path changed unexpectedly.".into()),
                None => {
                    fs::create_dir(&previous).map_err(|e| e.to_string())?;
                    true
                }
            };
            if let Err(error) = fs::remove_dir(&destination) {
                if created {
                    let _ = fs::remove_dir(&previous);
                }
                return Err(format!("Folder was not empty or changed: {error}"));
            }
            Ok(Some(previous))
        }
        SyncAction::Conflict | SyncAction::Same | SyncAction::KeepExtra => Ok(None),
    }
}

fn append_event(file: &mut File, outcome: &SyncOutcome, count: &mut usize) -> Result<(), String> {
    let serialized = serde_json::to_vec(outcome).map_err(|e| e.to_string())?;
    file.write_all(&serialized)
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|e| e.to_string())?;
    *count += 1;
    if (*count).is_multiple_of(16) {
        file.sync_data().map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn execute(plan: SyncPlan, cancel: &AtomicBool) -> Result<SyncReport, String> {
    execute_with_progress(plan, cancel, |_, _| {})
}

pub fn execute_with_progress(
    plan: SyncPlan,
    cancel: &AtomicBool,
    progress: impl Fn(usize, usize),
) -> Result<SyncReport, String> {
    let (source, destination) = validate_roots(&plan.source, &plan.destination)?;
    if source != plan.source || destination != plan.destination {
        return Err("Sync folder identity changed after preview; preview again.".into());
    }
    let _lock = acquire_sync_lock(&destination)?;
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?;
    let run_id = format!("{}-{}", start.as_secs(), start.subsec_nanos());
    let history = history_dir(&destination)?;
    let parent = destination.parent().ok_or("Destination has no parent")?;
    let relative_run = history
        .strip_prefix(parent)
        .map_err(|e| e.to_string())?
        .join(&run_id);
    let root = ensure_safe_parent(parent, &relative_run)?;
    fs::create_dir(&root).map_err(|e| e.to_string())?;
    let versions = root.join("versions");
    fs::create_dir(&versions).map_err(|e| e.to_string())?;
    let mut report = SyncReport {
        run_id,
        source: source.to_string_lossy().into_owned(),
        destination: destination.to_string_lossy().into_owned(),
        started_at: start.as_secs(),
        finished_at: 0,
        completed: 0,
        skipped: 0,
        failed: 0,
        cancelled: false,
        versions_dir: versions.to_string_lossy().into_owned(),
        outcomes: Vec::new(),
    };
    // A durable report exists before any destination change. It identifies the
    // version folder even if the process stops before the final report is saved.
    write_json_new(&root.join("report.json"), &report)?;
    let mut items: Vec<&SyncItem> = plan.items.iter().collect();
    items.sort_by(|left, right| {
        let priority = |action| match action {
            SyncAction::CreateDir => 0,
            SyncAction::Copy | SyncAction::Update => 1,
            SyncAction::DeleteFile => 2,
            SyncAction::DeleteDir => 3,
            SyncAction::Conflict | SyncAction::Same | SyncAction::KeepExtra => 4,
        };
        priority(left.action)
            .cmp(&priority(right.action))
            .then_with(|| {
                if left.action == SyncAction::DeleteDir {
                    right
                        .relative
                        .components()
                        .count()
                        .cmp(&left.relative.components().count())
                } else {
                    left.relative.cmp(&right.relative)
                }
            })
    });
    let journal = root.join("events.jsonl");
    let mut journal_file = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&journal)
        .map_err(|e| e.to_string())?;
    let mut event_count = 0usize;
    let total = items.len();
    for (index, item) in items.into_iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            report.cancelled = true;
            break;
        }
        if !item.action.will_write() {
            if matches!(item.action, SyncAction::Conflict | SyncAction::KeepExtra) {
                report.skipped += 1;
                let outcome = SyncOutcome {
                    relative: relative_key(&item.relative),
                    action: item.action.label().into(),
                    status: "skipped".into(),
                    detail: if item.action == SyncAction::Conflict {
                        item.conflict_reason
                            .clone()
                            .unwrap_or_else(|| "Resolve this conflict manually.".into())
                    } else {
                        "Destination-only item retained because deletion is off.".into()
                    },
                    previous_version: None,
                };
                append_event(&mut journal_file, &outcome, &mut event_count)?;
                report.outcomes.push(outcome);
            }
            progress(index + 1, total);
            continue;
        }
        let result = apply_item(&plan, item, &versions, cancel);
        let outcome = match result {
            Ok(previous) => {
                report.completed += 1;
                SyncOutcome {
                    relative: relative_key(&item.relative),
                    action: item.action.label().into(),
                    status: "done".into(),
                    detail: String::new(),
                    previous_version: previous.map(|path| path.to_string_lossy().into_owned()),
                }
            }
            Err(error) if cancel.load(Ordering::Acquire) => {
                report.cancelled = true;
                report.skipped += 1;
                SyncOutcome {
                    relative: relative_key(&item.relative),
                    action: item.action.label().into(),
                    status: "cancelled".into(),
                    detail: error,
                    previous_version: None,
                }
            }
            Err(error) => {
                report.failed += 1;
                SyncOutcome {
                    relative: relative_key(&item.relative),
                    action: item.action.label().into(),
                    status: "failed".into(),
                    detail: error,
                    previous_version: None,
                }
            }
        };
        append_event(&mut journal_file, &outcome, &mut event_count)?;
        report.outcomes.push(outcome);
        progress(index + 1, total);
        if report.cancelled {
            break;
        }
    }
    journal_file.sync_all().map_err(|e| e.to_string())?;
    report.finished_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0);
    write_json_new(&root.join("report.json"), &report)?;
    if !report.cancelled && report.failed == 0 {
        let mut cache = FingerprintCache { rows: plan.cache };
        let completed: HashSet<&str> = report
            .outcomes
            .iter()
            .filter(|outcome| outcome.status == "done")
            .map(|outcome| outcome.relative.as_str())
            .collect();
        for item in &plan.items {
            if matches!(item.action, SyncAction::Copy | SyncAction::Update)
                && completed.contains(relative_key(&item.relative).as_str())
            {
                if let (Ok(Some(source)), Ok(Some(destination)), Some(hash)) = (
                    fingerprint(&plan.source.join(&item.relative)),
                    fingerprint(&plan.destination.join(&item.relative)),
                    item.source_hash.clone(),
                ) {
                    cache.rows.insert(
                        comparison_key(&item.relative),
                        CacheRow {
                            source,
                            destination,
                            hash,
                        },
                    );
                }
            }
        }
        write_json_new(&cache_file(&plan.destination)?, &cache)?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("pathfinder-sync-test-{nanos}"));
            fs::create_dir_all(root.join("source")).unwrap();
            fs::create_dir_all(root.join("destination")).unwrap();
            Self(root)
        }
        fn source(&self) -> PathBuf {
            self.0.join("source")
        }
        fn destination(&self) -> PathBuf {
            self.0.join("destination")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preview_is_read_only_and_deletion_is_opt_in() {
        let fixture = Fixture::new();
        fs::write(fixture.source().join("new.txt"), b"new").unwrap();
        fs::write(fixture.destination().join("extra.txt"), b"old").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "",
            false,
            &cancel,
        )
        .unwrap();
        assert_eq!(preview.count(SyncAction::Copy), 1);
        assert_eq!(preview.count(SyncAction::DeleteFile), 0);
        assert!(fixture.destination().join("extra.txt").exists());
        assert!(!history_dir(&fixture.destination()).unwrap().exists());
        let preview = plan(&fixture.source(), &fixture.destination(), "", true, &cancel).unwrap();
        assert_eq!(preview.count(SyncAction::DeleteFile), 1);
    }

    #[test]
    fn updates_and_deletions_keep_previous_versions() {
        let fixture = Fixture::new();
        fs::write(fixture.destination().join("changed.txt"), b"old").unwrap();
        fs::write(fixture.source().join("changed.txt"), b"new content").unwrap();
        fs::write(fixture.destination().join("extra.txt"), b"keep me").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(&fixture.source(), &fixture.destination(), "", true, &cancel).unwrap();
        let report = execute(preview, &cancel).unwrap();
        assert_eq!(report.failed, 0);
        assert_eq!(
            fs::read(fixture.destination().join("changed.txt")).unwrap(),
            b"new content"
        );
        assert!(!fixture.destination().join("extra.txt").exists());
        assert_eq!(
            fs::read(Path::new(&report.versions_dir).join("changed.txt")).unwrap(),
            b"old"
        );
        assert_eq!(
            fs::read(Path::new(&report.versions_dir).join("extra.txt")).unwrap(),
            b"keep me"
        );
        let report_path = Path::new(&report.versions_dir)
            .parent()
            .unwrap()
            .join("report.json");
        let saved: SyncReport = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert_eq!(saved.completed, report.completed);
        let next = plan(&fixture.source(), &fixture.destination(), "", true, &cancel).unwrap();
        assert_eq!(next.pending_count(), 0);
    }

    #[test]
    fn changed_destination_after_preview_is_preserved() {
        let fixture = Fixture::new();
        fs::write(fixture.destination().join("same.txt"), b"old").unwrap();
        fs::write(fixture.source().join("same.txt"), b"new content").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "",
            false,
            &cancel,
        )
        .unwrap();
        fs::write(fixture.destination().join("same.txt"), b"external change").unwrap();
        let report = execute(preview, &cancel).unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(
            fs::read(fixture.destination().join("same.txt")).unwrap(),
            b"external change"
        );
    }

    #[test]
    fn changed_source_after_preview_is_not_copied() {
        let fixture = Fixture::new();
        fs::write(fixture.source().join("new.txt"), b"reviewed bytes").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "",
            false,
            &cancel,
        )
        .unwrap();
        fs::write(fixture.source().join("new.txt"), b"different bytes").unwrap();
        let report = execute(preview, &cancel).unwrap();
        assert_eq!(report.failed, 1);
        assert!(!fixture.destination().join("new.txt").exists());
    }

    #[test]
    fn stopping_after_one_item_leaves_later_items_untouched() {
        let fixture = Fixture::new();
        fs::write(fixture.source().join("a.txt"), b"first").unwrap();
        fs::write(fixture.source().join("b.txt"), b"second").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "",
            false,
            &cancel,
        )
        .unwrap();
        let report = execute_with_progress(preview, &cancel, |done, _| {
            if done == 1 {
                cancel.store(true, Ordering::Release);
            }
        })
        .unwrap();
        assert!(report.cancelled);
        assert_eq!(report.completed, 1);
        assert_eq!(
            fs::read(fixture.destination().join("a.txt")).unwrap(),
            b"first"
        );
        assert!(!fixture.destination().join("b.txt").exists());
        let run_dir = Path::new(&report.versions_dir).parent().unwrap();
        let saved: SyncReport =
            serde_json::from_slice(&fs::read(run_dir.join("report.json")).unwrap()).unwrap();
        assert!(saved.cancelled);
        assert!(run_dir.join("events.jsonl").exists());
    }

    #[test]
    fn stopping_during_skipped_items_finishes_promptly() {
        let fixture = Fixture::new();
        fs::write(fixture.destination().join("a.txt"), b"retained").unwrap();
        fs::write(fixture.destination().join("b.txt"), b"retained").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "",
            false,
            &cancel,
        )
        .unwrap();
        let report = execute_with_progress(preview, &cancel, |done, _| {
            if done == 1 {
                cancel.store(true, Ordering::Release);
            }
        })
        .unwrap();
        assert!(report.cancelled);
        assert_eq!(report.skipped, 1);
        assert!(fixture.destination().join("a.txt").exists());
        assert!(fixture.destination().join("b.txt").exists());
    }

    #[test]
    fn excludes_and_overlapping_roots_are_checked() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.source().join("cache")).unwrap();
        fs::write(fixture.source().join("cache").join("skip.bin"), b"skip").unwrap();
        fs::write(fixture.source().join("keep.txt"), b"keep").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "cache/**, *.bin",
            false,
            &cancel,
        )
        .unwrap();
        assert_eq!(preview.count(SyncAction::Copy), 1);
        assert!(plan(&fixture.0, &fixture.destination(), "", false, &cancel).is_err());
    }

    #[test]
    fn deleting_nested_extra_folder_preserves_its_file() {
        let fixture = Fixture::new();
        let extra = fixture.destination().join("old").join("nested");
        fs::create_dir_all(&extra).unwrap();
        fs::create_dir(fixture.destination().join("empty")).unwrap();
        fs::write(extra.join("note.txt"), b"recoverable").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(&fixture.source(), &fixture.destination(), "", true, &cancel).unwrap();
        assert_eq!(preview.count(SyncAction::DeleteDir), 3);
        let report = execute(preview, &cancel).unwrap();
        assert_eq!(report.failed, 0);
        assert!(!fixture.destination().join("old").exists());
        assert!(!fixture.destination().join("empty").exists());
        assert!(Path::new(&report.versions_dir).join("empty").is_dir());
        assert_eq!(
            fs::read(Path::new(&report.versions_dir).join("old/nested/note.txt")).unwrap(),
            b"recoverable"
        );
    }

    #[test]
    fn a_newer_destination_is_a_conflict_and_is_not_overwritten() {
        let fixture = Fixture::new();
        let source = fixture.source().join("report.txt");
        let destination = fixture.destination().join("report.txt");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"destination").unwrap();
        let old = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let new = old + std::time::Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old))
            .unwrap();
        File::options()
            .write(true)
            .open(&destination)
            .unwrap()
            .set_times(FileTimes::new().set_modified(new))
            .unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "",
            false,
            &cancel,
        )
        .unwrap();
        assert_eq!(preview.count(SyncAction::Conflict), 1);
        assert_eq!(preview.pending_count(), 0);
        assert!(
            preview.items[0]
                .conflict_reason
                .as_deref()
                .unwrap()
                .contains("newer")
        );
        let report = execute(preview, &cancel).unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(fs::read(destination).unwrap(), b"destination");
    }

    #[test]
    fn a_conflicting_folder_blocks_deletion_of_its_children() {
        let fixture = Fixture::new();
        fs::write(fixture.source().join("shared"), b"source file").unwrap();
        let destination_folder = fixture.destination().join("shared");
        fs::create_dir(&destination_folder).unwrap();
        fs::write(destination_folder.join("keep.txt"), b"preserve me").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(&fixture.source(), &fixture.destination(), "", true, &cancel).unwrap();
        assert_eq!(preview.count(SyncAction::Conflict), 2);
        assert_eq!(preview.pending_count(), 0);
        assert!(
            preview.items.iter().any(|item| item
                .conflict_reason
                .as_deref()
                .unwrap()
                .contains("parent"))
        );
        let report = execute(preview, &cancel).unwrap();
        assert_eq!(report.skipped, 2);
        assert_eq!(
            fs::read(destination_folder.join("keep.txt")).unwrap(),
            b"preserve me"
        );
    }

    #[cfg(windows)]
    #[test]
    fn case_only_name_difference_is_a_conflict() {
        let fixture = Fixture::new();
        fs::write(fixture.source().join("Report.txt"), b"source").unwrap();
        fs::write(fixture.destination().join("report.txt"), b"destination").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(&fixture.source(), &fixture.destination(), "", true, &cancel).unwrap();
        assert_eq!(preview.count(SyncAction::Conflict), 1);
        assert_eq!(preview.pending_count(), 0);
        assert_eq!(
            fs::read(fixture.destination().join("report.txt")).unwrap(),
            b"destination"
        );
    }

    #[test]
    fn cancellation_before_first_item_preserves_destination() {
        let fixture = Fixture::new();
        fs::write(fixture.source().join("new.txt"), b"new").unwrap();
        let cancel = AtomicBool::new(false);
        let preview = plan(
            &fixture.source(),
            &fixture.destination(),
            "",
            false,
            &cancel,
        )
        .unwrap();
        cancel.store(true, Ordering::Release);
        let report = execute(preview, &cancel).unwrap();
        assert!(report.cancelled);
        assert!(!fixture.destination().join("new.txt").exists());
    }
}
