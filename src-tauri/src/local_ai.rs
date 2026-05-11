//! Pathfinder Local AI: install state tracking, model downloads, and
//! background progress reporting. Inference itself ships in a follow-up
//! release that adds the `ort` crate with the DirectML execution provider
//! for NPU acceleration; this module handles everything around the model
//! files so the install flow is real and visible from the Settings tab.
//!
//! Layout on disk:
//!
//! ```text
//! %APPDATA%\Pathfinder\ai\
//!   manifest.json            <- install state, model list, sizes
//!   text-embedding.onnx      <- all-MiniLM-L6-v2, ~25 MB
//!   text-embedding.tokens    <- tokenizer vocab
//!   image-classifier.onnx    <- MobileNetV3-small, ~10 MB
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// State machine for the AI installer. Mirrored to the Slint property
/// `ai_install_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    NotInstalled,
    Downloading,
    Installed,
    Error,
}

impl InstallState {
    pub fn as_slint_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Downloading => "downloading",
            Self::Installed => "installed",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub state: InstallState,
    pub installed_models: Vec<String>,
    pub total_bytes: u64,
    pub installed_at: Option<u64>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            state: InstallState::NotInstalled,
            installed_models: Vec::new(),
            total_bytes: 0,
            installed_at: None,
        }
    }
}

/// Live progress published by the background installer. Polled by the UI
/// timer that drives the rest of the queue progress reporting.
pub struct InstallProgress {
    pub state: std::sync::Mutex<InstallState>,
    pub bytes_downloaded: AtomicU64,
    pub bytes_total: AtomicU64,
    pub message: std::sync::Mutex<String>,
    pub busy: AtomicBool,
}

impl InstallProgress {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(InstallState::NotInstalled),
            bytes_downloaded: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            message: std::sync::Mutex::new(String::new()),
            busy: AtomicBool::new(false),
        }
    }
}

/// One model file we know how to fetch. URL points at a CDN-hosted ONNX
/// blob. SHA stays empty for the initial release; once we have stable
/// hosted mirrors with known digests, fill it in and verify at download time.
struct ModelSource {
    pub local_name: &'static str,
    pub display_name: &'static str,
    pub url: &'static str,
    pub approx_bytes: u64,
}

/// The set of model files that make up a complete Local AI install.
/// Picked for size and Snapdragon X / Intel Core Ultra / AMD XDNA NPU support
/// via DirectML when `ort` lands in the follow-up release.
const MODELS: &[ModelSource] = &[
    ModelSource {
        local_name: "text-embedding.onnx",
        display_name: "Text embedding (all-MiniLM-L6-v2)",
        url: "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx",
        approx_bytes: 90_000_000,
    },
    ModelSource {
        local_name: "tokenizer.json",
        display_name: "Tokenizer vocab",
        url: "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json",
        approx_bytes: 720_000,
    },
    ModelSource {
        local_name: "image-classifier.onnx",
        display_name: "Image classifier (MobileNetV3-small)",
        url: "https://github.com/onnx/models/raw/main/Computer_Vision/mobilenetv3_Opset18_timm/mobilenetv3_Opset18.onnx",
        approx_bytes: 10_000_000,
    },
];

/// Total approximate install size used for the explainer dialog.
pub fn approx_total_mb() -> u32 {
    let total: u64 = MODELS.iter().map(|m| m.approx_bytes).sum();
    (total / 1_000_000) as u32
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

pub fn read_manifest() -> Manifest {
    let p = manifest_path();
    if !p.exists() {
        return Manifest::default();
    }
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Manifest>(&s).ok())
        .unwrap_or_default()
}

pub fn write_manifest(m: &Manifest) {
    let dir = ai_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(s) = serde_json::to_string_pretty(m) {
        let _ = std::fs::write(manifest_path(), s);
    }
}

/// Kick off an install. Returns immediately; progress is published via
/// the shared `InstallProgress` and the UI polls it every 100 ms.
pub fn start_install(progress: Arc<InstallProgress>) {
    if progress.busy.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Ok(mut s) = progress.state.lock() { *s = InstallState::Downloading; }
    progress.bytes_downloaded.store(0, Ordering::Release);
    progress
        .bytes_total
        .store(MODELS.iter().map(|m| m.approx_bytes).sum(), Ordering::Release);
    if let Ok(mut m) = progress.message.lock() { *m = "Preparing download...".to_string(); }

    std::thread::spawn(move || {
        let dir = ai_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            if let Ok(mut s) = progress.state.lock() { *s = InstallState::Error; }
            if let Ok(mut m) = progress.message.lock() { *m = format!("Cannot create AI folder: {}", e); }
            progress.busy.store(false, Ordering::Release);
            return;
        }
        let mut accumulated: u64 = 0;
        let mut installed: Vec<String> = Vec::new();
        for model in MODELS {
            if let Ok(mut m) = progress.message.lock() { *m = format!("Downloading {} ...", model.display_name); }
            let dest = dir.join(model.local_name);
            match download_file_with_progress(model.url, &dest, model.approx_bytes, &progress, accumulated) {
                Ok(written) => {
                    accumulated = accumulated.saturating_add(written);
                    progress.bytes_downloaded.store(accumulated, Ordering::Release);
                    installed.push(model.local_name.to_string());
                }
                Err(e) => {
                    if let Ok(mut s) = progress.state.lock() { *s = InstallState::Error; }
                    if let Ok(mut m) = progress.message.lock() { *m = format!("{} failed: {}", model.display_name, e); }
                    progress.busy.store(false, Ordering::Release);
                    return;
                }
            }
        }
        let manifest = Manifest {
            state: InstallState::Installed,
            installed_models: installed,
            total_bytes: accumulated,
            installed_at: Some(now_unix_secs()),
        };
        write_manifest(&manifest);
        if let Ok(mut s) = progress.state.lock() { *s = InstallState::Installed; }
        if let Ok(mut m) = progress.message.lock() { *m = "Install complete.".to_string(); }
        progress.busy.store(false, Ordering::Release);
    });
}

/// Delete every file under `%APPDATA%\Pathfinder\ai`. Sets state back to
/// NotInstalled and clears the on-disk manifest.
pub fn uninstall(progress: Arc<InstallProgress>) {
    if progress.busy.load(Ordering::Acquire) {
        return;
    }
    let dir = ai_dir();
    let _ = std::fs::remove_dir_all(&dir);
    if let Ok(mut s) = progress.state.lock() { *s = InstallState::NotInstalled; }
    if let Ok(mut m) = progress.message.lock() { *m = String::new(); }
    progress.bytes_downloaded.store(0, Ordering::Release);
    write_manifest(&Manifest::default());
}

/// Download a single file with periodic byte-count updates to the shared
/// progress struct. Uses a streaming PowerShell Invoke-WebRequest because
/// it works without bundling a TLS stack and respects the user's network
/// configuration (proxies, certificates, etc).
fn download_file_with_progress(
    url: &str,
    dest: &std::path::Path,
    expected_bytes: u64,
    progress: &Arc<InstallProgress>,
    accumulated_before: u64,
) -> Result<u64, String> {
    use std::io::Read;

    // Use the `ureq` crate would be cleaner, but to avoid adding a TLS
    // dep we shell out to PowerShell which Windows already has. Stream
    // the response to disk in 64 KB chunks and report progress.
    let script = format!(
        "$ProgressPreference='SilentlyContinue'; \
         $resp = [System.Net.HttpWebRequest]::Create('{}'); \
         $resp.UserAgent = 'Pathfinder'; \
         $r = $resp.GetResponse(); \
         $s = $r.GetResponseStream(); \
         $fs = [System.IO.File]::Create('{}'); \
         $buf = New-Object byte[] 65536; \
         while (($n = $s.Read($buf, 0, $buf.Length)) -gt 0) {{ \
             $fs.Write($buf, 0, $n); \
         }} \
         $fs.Close(); $s.Close(); $r.Close();",
        url.replace('\'', "''"),
        dest.to_string_lossy().replace('\'', "''")
    );

    let mut child = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    // Poll progress by reading the file size while PowerShell streams.
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if let Ok(meta) = std::fs::metadata(dest) {
            progress.bytes_downloaded.store(
                accumulated_before.saturating_add(meta.len()),
                Ordering::Release,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    let mut err_buf = String::new();
    if let Some(stderr) = child.stderr.as_mut() {
        let _ = stderr.read_to_string(&mut err_buf);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(if err_buf.is_empty() {
            "Download failed".to_string()
        } else {
            err_buf.trim().to_string()
        });
    }
    let written = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(expected_bytes);
    Ok(written)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
