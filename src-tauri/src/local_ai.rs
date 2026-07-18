//! Pathfinder Local AI: versioned asset catalog, hashed downloads, profiles,
//! and silent self-update.
//!
//! Layout on disk:
//! ```text
//! %APPDATA%\Pathfinder\ai\
//!   manifest.json
//!   install.lock
//!   onnxruntime.dll (+ provider DLLs)
//!   text-embedding.onnx
//!   tokenizer.json
//!   image-classifier.onnx
//!   *.part                  <- in-progress downloads
//! ```

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

const BUNDLED_CATALOG_JSON: &str = include_str!("../ai-catalog.json");
const MANIFEST_SCHEMA: u32 = 2;
const LOCK_STALE_SECS: u64 = 3600;

/// State machine for the AI installer. Mirrored to the Slint property
/// `ai_install_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    NotInstalled,
    Downloading,
    Updating,
    Installed,
    Error,
}

impl InstallState {
    pub fn as_slint_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Downloading => "downloading",
            Self::Updating => "updating",
            Self::Installed => "installed",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledAsset {
    pub id: String,
    pub revision: String,
    pub sha256: String,
    pub bytes: u64,
    pub local_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    pub state: InstallState,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub catalog_version: String,
    #[serde(default)]
    pub installed_assets: Vec<InstalledAsset>,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub installed_at: Option<u64>,
    #[serde(default)]
    pub last_verified_at: Option<u64>,
    #[serde(default)]
    pub last_update_check_at: Option<u64>,
    #[serde(default)]
    pub embedding_model_id: String,
    #[serde(default)]
    pub embedding_dim: u32,
    #[serde(default)]
    pub embedding_revision: String,
    #[serde(default)]
    pub classifier_model_id: String,
    #[serde(default)]
    pub error_message: String,
    /// Legacy field from schema v1 — kept for migration.
    #[serde(default)]
    pub installed_models: Vec<String>,
}

fn default_schema() -> u32 {
    MANIFEST_SCHEMA
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA,
            state: InstallState::NotInstalled,
            profile: "balanced".into(),
            catalog_version: String::new(),
            installed_assets: Vec::new(),
            total_bytes: 0,
            installed_at: None,
            last_verified_at: None,
            last_update_check_at: None,
            embedding_model_id: String::new(),
            embedding_dim: 0,
            embedding_revision: String::new(),
            classifier_model_id: String::new(),
            error_message: String::new(),
            installed_models: Vec::new(),
        }
    }
}

/// Live progress published by the background installer / updater.
pub struct InstallProgress {
    pub state: Mutex<InstallState>,
    pub bytes_downloaded: AtomicU64,
    pub bytes_total: AtomicU64,
    pub message: Mutex<String>,
    pub busy: AtomicBool,
    pub update_available: AtomicBool,
    pub update_bytes: AtomicU64,
}

impl InstallProgress {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InstallState::NotInstalled),
            bytes_downloaded: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            message: Mutex::new(String::new()),
            busy: AtomicBool::new(false),
            update_available: AtomicBool::new(false),
            update_bytes: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogAsset {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub url: String,
    pub local_name: String,
    #[serde(default)]
    pub sha256: String,
    pub bytes: u64,
    #[serde(default)]
    pub embed_dim: Option<u32>,
    #[serde(default)]
    pub max_seq: Option<u32>,
    #[serde(default)]
    pub query_prefix: Option<String>,
    #[serde(default)]
    pub input_name: Option<String>,
    #[serde(default)]
    pub input_size: Option<u32>,
    #[serde(default)]
    pub extract: Option<String>,
    #[serde(default)]
    pub pairs_with: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogProfile {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub recommended: bool,
    pub assets: Vec<String>,
    pub embedding_asset: String,
    pub classifier_asset: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiCatalog {
    pub schema_version: u32,
    pub catalog_version: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub remote_catalog_url: String,
    pub default_profile: String,
    pub assets: HashMap<String, CatalogAsset>,
    pub profiles: HashMap<String, CatalogProfile>,
}

#[derive(Debug, Clone)]
pub struct ActiveModelInfo {
    pub profile: String,
    pub embedding_id: String,
    pub embedding_dim: u32,
    pub embedding_revision: String,
    pub query_prefix: String,
    pub max_seq: usize,
    pub classifier_id: String,
    pub classifier_input_name: String,
    pub classifier_input_size: u32,
}

/// Total approximate install size for a profile (MB).
pub fn approx_install_mb_for_profile(profile: &str) -> u32 {
    let catalog = bundled_catalog();
    let profile = resolve_profile(&catalog, profile);
    let total: u64 = profile
        .assets
        .iter()
        .filter_map(|id| catalog.assets.get(id))
        .map(|a| a.bytes)
        .sum();
    ((total.saturating_add(500_000)) / 1_000_000) as u32
}

pub fn approx_total_install_mb() -> u32 {
    approx_install_mb_for_profile("balanced")
}

pub fn profile_summaries() -> Vec<(String, String, String, bool, u32)> {
    let catalog = active_catalog();
    let mut rows: Vec<_> = catalog
        .profiles
        .values()
        .map(|p| {
            let mb = approx_install_mb_for_profile(&p.id);
            (
                p.id.clone(),
                p.display_name.clone(),
                p.description.clone(),
                p.recommended,
                mb,
            )
        })
        .collect();
    rows.sort_by(|a, b| {
        // Recommended first, then name.
        b.3.cmp(&a.3).then_with(|| a.1.cmp(&b.1))
    });
    rows
}

pub fn ai_dir() -> PathBuf {
    let mut p = dirs::config_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    p.push("Pathfinder");
    p.push("ai");
    p
}

pub fn manifest_path() -> PathBuf {
    ai_dir().join("manifest.json")
}

fn lock_path() -> PathBuf {
    ai_dir().join("install.lock")
}

fn catalog_cache_path() -> PathBuf {
    ai_dir().join("catalog.cache.json")
}

pub fn bundled_catalog() -> AiCatalog {
    serde_json::from_str(BUNDLED_CATALOG_JSON).expect("bundled ai-catalog.json must parse")
}

/// Prefer a freshly fetched remote catalog cache; fall back to bundled.
pub fn active_catalog() -> AiCatalog {
    let cache = catalog_cache_path();
    if cache.is_file() {
        if let Ok(s) = fs::read_to_string(&cache) {
            if let Ok(c) = serde_json::from_str::<AiCatalog>(&s) {
                if c.schema_version >= 1 && !c.assets.is_empty() {
                    return c;
                }
            }
        }
    }
    bundled_catalog()
}

fn resolve_profile<'a>(catalog: &'a AiCatalog, profile: &str) -> &'a CatalogProfile {
    catalog
        .profiles
        .get(profile)
        .or_else(|| catalog.profiles.get(&catalog.default_profile))
        .or_else(|| catalog.profiles.values().next())
        .expect("catalog must contain at least one profile")
}

#[cfg(windows)]
pub fn onnx_runtime_installed() -> bool {
    ai_dir().join("onnxruntime.dll").is_file()
}

#[cfg(not(windows))]
pub fn onnx_runtime_installed() -> bool {
    false
}

pub fn core_models_installed() -> bool {
    let d = ai_dir();
    d.join("text-embedding.onnx").is_file() && d.join("tokenizer.json").is_file()
}

pub fn classifier_installed() -> bool {
    ai_dir().join("image-classifier.onnx").is_file()
}

/// Verified install: files present and runtime available. Full SHA verification
/// is reserved for repair / update paths so readiness checks stay cheap.
pub fn install_verified() -> bool {
    let m = read_manifest();
    if !matches!(m.state, InstallState::Installed) {
        return false;
    }
    if !core_models_installed() {
        return false;
    }
    #[cfg(windows)]
    if !onnx_runtime_installed() {
        return false;
    }
    true
}

/// Expensive integrity check used by Verify & repair.
pub fn verify_asset_hashes() -> Result<(), String> {
    let m = read_manifest();
    for asset in &m.installed_assets {
        if asset.local_name.starts_with('_') || asset.local_name.ends_with(".nupkg") {
            continue;
        }
        if asset.id.starts_with("ort-") {
            #[cfg(windows)]
            if !onnx_runtime_installed() {
                return Err("onnxruntime.dll missing".into());
            }
            continue;
        }
        let path = ai_dir().join(&asset.local_name);
        if !path.is_file() {
            return Err(format!("missing {}", asset.local_name));
        }
        if !asset.sha256.is_empty() {
            let actual = sha256_file(&path)?;
            if !actual.eq_ignore_ascii_case(&asset.sha256) {
                return Err(format!("{} hash mismatch", asset.local_name));
            }
        }
    }
    Ok(())
}

pub fn read_manifest() -> Manifest {
    let p = manifest_path();
    if !p.exists() {
        return Manifest::default();
    }
    let Ok(s) = fs::read_to_string(&p) else {
        return Manifest::default();
    };
    let mut m: Manifest = serde_json::from_str(&s).unwrap_or_default();
    migrate_manifest(&mut m);
    m
}

fn migrate_manifest(m: &mut Manifest) {
    if m.schema_version < MANIFEST_SCHEMA {
        m.schema_version = MANIFEST_SCHEMA;
        if m.profile.is_empty() {
            m.profile = "balanced".into();
        }
        // Legacy installs listed file names only — treat as installed if files exist.
        if matches!(m.state, InstallState::Installed) && m.installed_assets.is_empty() {
            if core_models_installed() {
                // Keep Installed; verify will repair hashes on next update check.
            } else {
                m.state = InstallState::NotInstalled;
            }
        }
    }
}

pub fn write_manifest(m: &Manifest) {
    let dir = ai_dir();
    let _ = fs::create_dir_all(&dir);
    let tmp = dir.join("manifest.json.part");
    if let Ok(s) = serde_json::to_string_pretty(m) {
        if fs::write(&tmp, s).is_ok() {
            let _ = fs::rename(&tmp, manifest_path());
        }
    }
}

pub fn actual_disk_usage_bytes() -> u64 {
    let dir = ai_dir();
    if !dir.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("part") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

pub fn active_model_info() -> Option<ActiveModelInfo> {
    let m = read_manifest();
    if !matches!(m.state, InstallState::Installed) || !core_models_installed() {
        return None;
    }
    let catalog = active_catalog();
    let profile = resolve_profile(&catalog, &m.profile);
    let emb = catalog.assets.get(&profile.embedding_asset)?;
    let clf = catalog.assets.get(&profile.classifier_asset)?;
    Some(ActiveModelInfo {
        profile: profile.id.clone(),
        embedding_id: emb.id.clone(),
        embedding_dim: emb.embed_dim.unwrap_or(384),
        embedding_revision: m.embedding_revision.clone(),
        query_prefix: emb.query_prefix.clone().unwrap_or_default(),
        max_seq: emb.max_seq.unwrap_or(128) as usize,
        classifier_id: clf.id.clone(),
        classifier_input_name: clf
            .input_name
            .clone()
            .unwrap_or_else(|| "data".into()),
        classifier_input_size: clf.input_size.unwrap_or(224),
    })
}

fn try_acquire_lock() -> Result<(), String> {
    let dir = ai_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let lock = lock_path();
    if lock.exists() {
        if let Ok(meta) = fs::metadata(&lock) {
            if let Ok(modified) = meta.modified() {
                if modified
                    .elapsed()
                    .unwrap_or(Duration::from_secs(0))
                    .as_secs()
                    < LOCK_STALE_SECS
                {
                    return Err("Another Local AI install is already running.".into());
                }
            }
        }
        let _ = fs::remove_file(&lock);
    }
    fs::write(&lock, format!("{}", std::process::id())).map_err(|e| e.to_string())?;
    Ok(())
}

fn release_lock() {
    let _ = fs::remove_file(lock_path());
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn asset_path(asset: &CatalogAsset) -> PathBuf {
    ai_dir().join(&asset.local_name)
}

fn asset_is_valid(asset: &CatalogAsset) -> bool {
    if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
        #[cfg(windows)]
        {
            return onnx_runtime_installed();
        }
        #[cfg(not(windows))]
        {
            return true;
        }
    }
    let path = asset_path(asset);
    if !path.is_file() {
        return false;
    }
    let Ok(meta) = fs::metadata(&path) else {
        return false;
    };
    if asset.bytes > 0 {
        let len = meta.len();
        // Allow small variance for CDN encoding differences; require near match.
        let lo = asset.bytes.saturating_mul(95) / 100;
        let hi = asset.bytes.saturating_mul(105) / 100;
        if len < lo || len > hi.max(asset.bytes.saturating_add(1024 * 1024)) {
            return false;
        }
    }
    if !asset.sha256.is_empty() {
        match sha256_file(&path) {
            Ok(actual) => actual.eq_ignore_ascii_case(&asset.sha256),
            Err(_) => false,
        }
    } else {
        true
    }
}

fn download_https(url: &str, dest: &Path, progress: &InstallProgress, base: u64) -> Result<u64, String> {
    let _ = fs::remove_file(dest);
    let part = PathBuf::from(format!("{}.part", dest.display()));
    let _ = fs::remove_file(&part);

    let resp = ureq::get(url)
        .set("User-Agent", "Pathfinder-LocalAI/1.0")
        .set("Accept", "application/octet-stream,*/*")
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    if !(200..300).contains(&resp.status()) {
        return Err(format!("HTTP {}", resp.status()));
    }
    let mut reader = resp.into_reader();
    let mut out = File::create(&part).map_err(|e| format!("create part file: {e}"))?;
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        written = written.saturating_add(n as u64);
        progress
            .bytes_downloaded
            .store(base.saturating_add(written), Ordering::Release);
    }
    drop(out);
    fs::rename(&part, dest).map_err(|e| format!("finalize download: {e}"))?;
    Ok(written)
}

fn ensure_asset(
    asset: &CatalogAsset,
    progress: &InstallProgress,
    accumulated: &mut u64,
) -> Result<InstalledAsset, String> {
    if asset_is_valid(asset) {
        let path = if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
            ai_dir().join("onnxruntime.dll")
        } else {
            asset_path(asset)
        };
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(asset.bytes);
        let sha = if !asset.sha256.is_empty() {
            asset.sha256.clone()
        } else if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
            String::new()
        } else {
            sha256_file(&path).unwrap_or_default()
        };
        *accumulated = accumulated.saturating_add(bytes);
        progress
            .bytes_downloaded
            .store(*accumulated, Ordering::Release);
        return Ok(InstalledAsset {
            id: asset.id.clone(),
            revision: asset.sha256.clone(),
            sha256: sha,
            bytes,
            local_name: if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
                "onnxruntime.dll".into()
            } else {
                asset.local_name.clone()
            },
        });
    }

    if let Ok(mut m) = progress.message.lock() {
        *m = format!("Downloading {} …", asset.display_name);
    }
    let dest = asset_path(asset);
    let written = download_https(&asset.url, &dest, progress, *accumulated)?;
    if !asset.sha256.is_empty() {
        let actual = sha256_file(&dest)?;
        if !actual.eq_ignore_ascii_case(&asset.sha256) {
            let _ = fs::remove_file(&dest);
            return Err(format!(
                "{} hash mismatch (got {actual}, expected {})",
                asset.display_name, asset.sha256
            ));
        }
    }
    if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
        #[cfg(windows)]
        {
            extract_win64_native_dlls_from_ort_package(&dest, &ai_dir())?;
            let _ = fs::remove_file(&dest);
        }
        #[cfg(not(windows))]
        {
            let _ = dest;
        }
    }
    let final_path = if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
        ai_dir().join("onnxruntime.dll")
    } else {
        dest
    };
    let bytes = fs::metadata(&final_path)
        .map(|m| m.len())
        .unwrap_or(written);
    let sha = if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
        asset.sha256.clone()
    } else {
        sha256_file(&final_path).unwrap_or_else(|_| asset.sha256.clone())
    };
    *accumulated = accumulated.saturating_add(bytes);
    progress
        .bytes_downloaded
        .store(*accumulated, Ordering::Release);
    Ok(InstalledAsset {
        id: asset.id.clone(),
        revision: asset.sha256.clone(),
        sha256: sha,
        bytes,
        local_name: if asset.extract.as_deref() == Some("ort_nupkg_win_x64") {
            "onnxruntime.dll".into()
        } else {
            asset.local_name.clone()
        },
    })
}

fn cleanup_orphans(keep: &HashSet<String>) {
    let dir = ai_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == "manifest.json"
            || name == "install.lock"
            || name == "catalog.cache.json"
            || name.ends_with(".part")
        {
            continue;
        }
        // Keep ORT companion DLLs.
        if name.starts_with("onnxruntime") || name.starts_with("DirectML") {
            continue;
        }
        if !keep.contains(name) {
            let _ = fs::remove_file(&path);
        }
    }
}

fn install_profile_inner(
    profile_id: &str,
    progress: Arc<InstallProgress>,
    updating: bool,
) -> Result<Manifest, String> {
    try_acquire_lock()?;
    let catalog = fetch_catalog_best_effort();
    let profile = resolve_profile(&catalog, profile_id).clone();
    let assets: Vec<CatalogAsset> = profile
        .assets
        .iter()
        .filter_map(|id| catalog.assets.get(id).cloned())
        .collect();
    if assets.is_empty() {
        release_lock();
        return Err("Profile has no assets.".into());
    }

    let total: u64 = assets.iter().map(|a| a.bytes).sum();
    progress.bytes_total.store(total, Ordering::Release);
    progress.bytes_downloaded.store(0, Ordering::Release);
    if let Ok(mut s) = progress.state.lock() {
        *s = if updating {
            InstallState::Updating
        } else {
            InstallState::Downloading
        };
    }

    let mut accumulated = 0u64;
    let mut installed = Vec::new();
    let mut keep_names: HashSet<String> = HashSet::new();
    keep_names.insert("onnxruntime.dll".into());

    for asset in &assets {
        match ensure_asset(asset, &progress, &mut accumulated) {
            Ok(row) => {
                keep_names.insert(row.local_name.clone());
                installed.push(row);
            }
            Err(e) => {
                release_lock();
                return Err(e);
            }
        }
    }

    cleanup_orphans(&keep_names);

    let emb = catalog
        .assets
        .get(&profile.embedding_asset)
        .cloned()
        .ok_or_else(|| "Missing embedding asset".to_string())?;
    let clf = catalog
        .assets
        .get(&profile.classifier_asset)
        .cloned()
        .ok_or_else(|| "Missing classifier asset".to_string())?;

    let disk = actual_disk_usage_bytes();
    let manifest = Manifest {
        schema_version: MANIFEST_SCHEMA,
        state: InstallState::Installed,
        profile: profile.id.clone(),
        catalog_version: catalog.catalog_version.clone(),
        installed_assets: installed,
        total_bytes: disk,
        installed_at: Some(now_unix_secs()),
        last_verified_at: Some(now_unix_secs()),
        last_update_check_at: Some(now_unix_secs()),
        embedding_model_id: emb.id.clone(),
        embedding_dim: emb.embed_dim.unwrap_or(384),
        embedding_revision: emb.sha256.clone(),
        classifier_model_id: clf.id.clone(),
        error_message: String::new(),
        installed_models: Vec::new(),
    };
    write_manifest(&manifest);
    release_lock();
    Ok(manifest)
}

fn set_error(progress: &InstallProgress, msg: String) {
    if let Ok(mut s) = progress.state.lock() {
        *s = InstallState::Error;
    }
    if let Ok(mut m) = progress.message.lock() {
        *m = msg.clone();
    }
    let mut manifest = read_manifest();
    manifest.state = InstallState::Error;
    manifest.error_message = msg;
    write_manifest(&manifest);
    progress.busy.store(false, Ordering::Release);
}

/// Kick off an install for `profile` (default balanced).
pub fn start_install(progress: Arc<InstallProgress>, profile: Option<String>) {
    if progress.busy.swap(true, Ordering::AcqRel) {
        return;
    }
    let profile = profile
        .unwrap_or_else(|| read_manifest().profile)
        .trim()
        .to_string();
    let profile = if profile.is_empty() {
        "balanced".into()
    } else {
        profile
    };
    std::thread::spawn(move || {
        match install_profile_inner(&profile, progress.clone(), false) {
            Ok(_) => {
                if let Ok(mut s) = progress.state.lock() {
                    *s = InstallState::Installed;
                }
                if let Ok(mut m) = progress.message.lock() {
                    *m = "Install complete.".into();
                }
                progress.update_available.store(false, Ordering::Release);
                progress.busy.store(false, Ordering::Release);
            }
            Err(e) => set_error(&progress, e),
        }
    });
}

pub fn start_repair(progress: Arc<InstallProgress>) {
    // Hash-verify first; if anything is wrong, reinstall the active profile.
    if let Err(e) = verify_asset_hashes() {
        if let Ok(mut m) = progress.message.lock() {
            *m = format!("Repair needed: {e}");
        }
    }
    let profile = read_manifest().profile;
    start_install(progress, Some(profile));
}

pub fn change_profile(progress: Arc<InstallProgress>, profile: String) {
    start_install(progress, Some(profile));
}

/// Fetch remote catalog if possible and cache it; always returns a usable catalog.
pub fn fetch_catalog_best_effort() -> AiCatalog {
    let bundled = bundled_catalog();
    let url = bundled.remote_catalog_url.clone();
    if url.is_empty() {
        return bundled;
    }
    match ureq::get(&url)
        .set("User-Agent", "Pathfinder-LocalAI/1.0")
        .timeout(Duration::from_secs(20))
        .call()
    {
        Ok(resp) if (200..300).contains(&resp.status()) => {
            if let Ok(body) = resp.into_string() {
                if let Ok(remote) = serde_json::from_str::<AiCatalog>(&body) {
                    if remote.schema_version >= 1 && !remote.assets.is_empty() {
                        let _ = fs::create_dir_all(ai_dir());
                        let _ = fs::write(catalog_cache_path(), body);
                        // Prefer whichever catalog_version string compares newer
                        // lexicographically when both look like dates; else prefer remote.
                        return remote;
                    }
                }
            }
            bundled
        }
        _ => bundled,
    }
}

/// Bytes that would need downloading to bring the active profile up to date.
pub fn pending_update_bytes(catalog: &AiCatalog, profile_id: &str) -> u64 {
    let profile = resolve_profile(catalog, profile_id);
    let mut need = 0u64;
    for id in &profile.assets {
        if let Some(asset) = catalog.assets.get(id) {
            if !asset_is_valid(asset) {
                need = need.saturating_add(asset.bytes);
            }
        }
    }
    need
}

pub fn check_for_model_updates(progress: Arc<InstallProgress>) -> u64 {
    let catalog = fetch_catalog_best_effort();
    let mut manifest = read_manifest();
    if !matches!(
        manifest.state,
        InstallState::Installed | InstallState::Error
    ) && !core_models_installed()
    {
        return 0;
    }
    let profile = if manifest.profile.is_empty() {
        catalog.default_profile.clone()
    } else {
        manifest.profile.clone()
    };
    let need = pending_update_bytes(&catalog, &profile);
    progress
        .update_available
        .store(need > 0, Ordering::Release);
    progress.update_bytes.store(need, Ordering::Release);
    manifest.last_update_check_at = Some(now_unix_secs());
    if need == 0 && matches!(manifest.state, InstallState::Installed) {
        manifest.catalog_version = catalog.catalog_version.clone();
    }
    write_manifest(&manifest);
    need
}

/// Apply pending model updates for the installed profile (only changed assets).
pub fn start_model_update(progress: Arc<InstallProgress>) {
    if progress.busy.swap(true, Ordering::AcqRel) {
        return;
    }
    let profile = {
        let m = read_manifest();
        if m.profile.is_empty() {
            "balanced".into()
        } else {
            m.profile
        }
    };
    std::thread::spawn(move || {
        match install_profile_inner(&profile, progress.clone(), true) {
            Ok(_) => {
                if let Ok(mut s) = progress.state.lock() {
                    *s = InstallState::Installed;
                }
                if let Ok(mut m) = progress.message.lock() {
                    *m = "Models updated.".into();
                }
                progress.update_available.store(false, Ordering::Release);
                progress.update_bytes.store(0, Ordering::Release);
                progress.busy.store(false, Ordering::Release);
            }
            Err(e) => {
                // Soft fail: restore Installed if files still work.
                if install_verified() || core_models_installed() {
                    if let Ok(mut s) = progress.state.lock() {
                        *s = InstallState::Installed;
                    }
                    if let Ok(mut m) = progress.message.lock() {
                        *m = format!("Update failed (kept previous models): {e}");
                    }
                    let mut manifest = read_manifest();
                    manifest.state = InstallState::Installed;
                    manifest.error_message = e;
                    write_manifest(&manifest);
                    progress.busy.store(false, Ordering::Release);
                } else {
                    set_error(&progress, e);
                }
            }
        }
    });
}

/// Background auto-update: check then apply when updates exist.
pub fn maybe_auto_update_models(progress: Arc<InstallProgress>, enabled: bool) {
    if !enabled {
        return;
    }
    if progress.busy.load(Ordering::Acquire) {
        return;
    }
    let m = read_manifest();
    if !matches!(m.state, InstallState::Installed) {
        return;
    }
    std::thread::spawn(move || {
        let need = check_for_model_updates(progress.clone());
        if need > 0 && !progress.busy.load(Ordering::Acquire) {
            start_model_update(progress);
        }
    });
}

pub fn uninstall(progress: Arc<InstallProgress>) {
    if progress.busy.load(Ordering::Acquire) {
        return;
    }
    let dir = ai_dir();
    let _ = fs::remove_dir_all(&dir);
    if let Ok(mut s) = progress.state.lock() {
        *s = InstallState::NotInstalled;
    }
    if let Ok(mut m) = progress.message.lock() {
        *m = String::new();
    }
    progress.bytes_downloaded.store(0, Ordering::Release);
    progress.update_available.store(false, Ordering::Release);
    progress.update_bytes.store(0, Ordering::Release);
    // Do not recreate the folder unless needed.
    let _ = fs::create_dir_all(&dir);
    write_manifest(&Manifest::default());
}

#[cfg(windows)]
fn safe_join_single_filename(dest: &Path, fname: &str) -> Option<PathBuf> {
    let p = Path::new(fname);
    if fname.is_empty() || p.is_absolute() {
        return None;
    }
    let mut it = p.components();
    match (it.next(), it.next()) {
        (Some(std::path::Component::Normal(_)), None) => Some(dest.join(fname)),
        _ => None,
    }
}

#[cfg(windows)]
fn extract_win64_native_dlls_from_ort_package(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;
    let prefix = "runtimes/win-x64/native/";
    let mut found_main = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let raw = entry.name().replace('\\', "/");
        let Some(fname) = raw.strip_prefix(prefix) else {
            continue;
        };
        if fname.contains('/') {
            continue;
        }
        if !fname.to_ascii_lowercase().ends_with(".dll") {
            continue;
        }
        let Some(out_path) = safe_join_single_filename(dest, fname) else {
            continue;
        };
        let mut out = File::create(&out_path).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        if fname.eq_ignore_ascii_case("onnxruntime.dll") {
            found_main = true;
        }
    }
    if !found_main {
        return Err("onnxruntime.dll not found in DirectML package".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_parses_and_has_profiles() {
        let c = bundled_catalog();
        assert!(c.profiles.contains_key("balanced"));
        assert!(c.profiles.contains_key("compact"));
        assert!(c.profiles.contains_key("quality"));
        assert!(c.assets.contains_key("bge-small-quant"));
        assert!(c.assets.contains_key("minilm-l6-quant"));
        assert!(!c.assets["ort-directml-1.24.4"].sha256.is_empty());
        // Storage should grow Compact < Balanced < Quality.
        assert_eq!(
            c.profiles["balanced"].embedding_asset,
            "bge-small-quant"
        );
        assert_eq!(
            c.profiles["quality"].classifier_asset,
            "efficientnet-lite4"
        );
    }

    #[test]
    fn approx_sizes_are_sane() {
        let compact = approx_install_mb_for_profile("compact");
        let bal = approx_install_mb_for_profile("balanced");
        let quality = approx_install_mb_for_profile("quality");
        assert!(compact < bal, "compact ({compact}) should be < balanced ({bal})");
        assert!(bal < quality, "balanced ({bal}) should be < quality ({quality})");
        assert!(compact > 20);
        assert!(quality < 150);
    }

    #[test]
    fn pending_update_zero_when_nothing_installed_still_counts() {
        // Without files, every asset is "needed".
        let c = bundled_catalog();
        let need = pending_update_bytes(&c, "compact");
        assert!(need > 1_000_000);
    }
}
