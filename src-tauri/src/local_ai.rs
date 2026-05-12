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
//!   onnxruntime.dll          <- ONNX Runtime (Windows DirectML build), user download
//!   onnxruntime_providers_*.dll
//!   text-embedding.onnx        <- all-MiniLM-L6-v2
//!   tokenizer.json
//!   image-classifier.onnx    <- MobileNetV3-small
//! ```

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use zip::ZipArchive;

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
/// ONNX Runtime + DirectML native libraries (same NuGet Microsoft ships).
/// Version must satisfy `ort` 2.0-rc API (ONNX Runtime 1.24.x).
#[cfg(windows)]
const ORT_DIRECTML_NUPKG_VER: &str = "1.24.0";
#[cfg(windows)]
const ORT_NUPKG_URL: &str =
    "https://www.nuget.org/api/v2/package/Microsoft.ML.OnnxRuntime.DirectML/1.24.0";
#[cfg(windows)]
const ORT_NUPKG_APPROX_BYTES: u64 = 72_000_000;

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
pub fn approx_total_install_mb() -> u32 {
    let mut total: u64 = MODELS.iter().map(|m| m.approx_bytes).sum();
    #[cfg(windows)]
    {
        total = total.saturating_add(ORT_NUPKG_APPROX_BYTES);
    }
    ((total.saturating_add(500_000)) / 1_000_000) as u32
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

/// DirectML ONNX Runtime DLL next to models (Windows Local AI install).
#[cfg(windows)]
pub fn onnx_runtime_installed() -> bool {
    ai_dir().join("onnxruntime.dll").is_file()
}

#[cfg(not(windows))]
pub fn onnx_runtime_installed() -> bool {
    false
}

/// Core embedding model files present (semantic search / embeddings).
pub fn core_models_installed() -> bool {
    let d = ai_dir();
    d.join("text-embedding.onnx").is_file() && d.join("tokenizer.json").is_file()
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
    let mut bytes_total: u64 = MODELS.iter().map(|m| m.approx_bytes).sum();
    #[cfg(windows)]
    {
        bytes_total = bytes_total.saturating_add(ORT_NUPKG_APPROX_BYTES);
    }
    progress.bytes_total.store(bytes_total, Ordering::Release);
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

        #[cfg(windows)]
        {
            if let Ok(mut m) = progress.message.lock() {
                *m = "Downloading ONNX Runtime (DirectML) ...".to_string();
            }
            let ort_zip = dir.join("_Microsoft.ML.OnnxRuntime.DirectML.zip");
            let _ = std::fs::remove_file(&ort_zip);
            match download_file_with_progress(
                ORT_NUPKG_URL,
                &ort_zip,
                ORT_NUPKG_APPROX_BYTES,
                &progress,
                accumulated,
            ) {
                Ok(written) => {
                    accumulated = accumulated.saturating_add(written);
                    progress.bytes_downloaded.store(accumulated, Ordering::Release);
                    if let Err(e) = extract_win64_native_dlls_from_ort_package(&ort_zip, &dir) {
                        let _ = std::fs::remove_file(&ort_zip);
                        if let Ok(mut s) = progress.state.lock() {
                            *s = InstallState::Error;
                        }
                        if let Ok(mut m) = progress.message.lock() {
                            *m = format!("Extract ONNX Runtime failed: {e}");
                        }
                        progress.busy.store(false, Ordering::Release);
                        return;
                    }
                    let _ = std::fs::remove_file(&ort_zip);
                    installed.push("onnxruntime.dll (DirectML bundle)".to_string());
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&ort_zip);
                    if let Ok(mut s) = progress.state.lock() {
                        *s = InstallState::Error;
                    }
                    if let Ok(mut m) = progress.message.lock() {
                        *m = format!("ONNX Runtime download failed: {e}");
                    }
                    progress.busy.store(false, Ordering::Release);
                    return;
                }
            }
        }

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

/// Read PowerShell stderr in the background so a verbose error stream cannot
/// fill the pipe buffer and block the child (classic `stderr` + `wait` deadlock).
fn drain_stderr_capped(mut stderr: std::process::ChildStderr, out: Arc<Mutex<String>>, cap: usize) {
    use std::io::Read;
    let mut kept = Vec::with_capacity(cap.min(4096));
    let mut chunk = [0u8; 8192];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                if room > 0 {
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
    if let Ok(mut g) = out.lock() {
        *g = String::from_utf8_lossy(&kept).into_owned();
    }
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

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to pipe stderr".to_string())?;
    const STDERR_CAP: usize = 64 * 1024;
    let err_shared = Arc::new(Mutex::new(String::new()));
    let err_for_thread = Arc::clone(&err_shared);
    let drain = std::thread::spawn(move || {
        drain_stderr_capped(stderr, err_for_thread, STDERR_CAP);
    });

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

    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = drain.join();
    let err_buf = err_shared.lock().map(|g| g.clone()).unwrap_or_default();

    if !status.success() {
        let err_msg = if err_buf.is_empty() {
            "Download failed".to_string()
        } else {
            err_buf.trim().to_string()
        };
        // PowerShell may have created a truncated file; do not leave a corrupt artifact.
        let _ = std::fs::remove_file(dest);
        return Err(err_msg);
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

/// Join `dest` with a single zip entry file name. Rejects absolute paths,
/// `..`, `.`, and multi-segment names (zip-slip) so we only write under `dest`.
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

/// Pull `runtimes/win-x64/native/*.dll` from the official DirectML NuGet
/// (.nupkg is a zip) into `dest` so `onnxruntime.dll` sits next to the models.
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
        return Err(format!(
            "onnxruntime.dll not found under {prefix} in Microsoft.ML.OnnxRuntime.DirectML {}",
            ORT_DIRECTML_NUPKG_VER
        ));
    }
    Ok(())
}
