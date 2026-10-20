//! Windows ML / DirectML execution-provider helpers.
//!
//! On Windows 11 24H2+, certified vendor EPs (OpenVINO / QNN / VitisAI / …)
//! can be discovered from installed packages. When unavailable we fall back
//! to DirectML NPU → GPU → CPU with a real smoke-test per candidate.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceleratorKind {
    Npu,
    Gpu,
    Cpu,
}

impl AcceleratorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npu => "NPU",
            Self::Gpu => "GPU",
            Self::Cpu => "CPU",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCandidate {
    pub label: String,
    pub kind: AcceleratorKind,
    /// Opaque tag used when building ORT session options.
    pub tag: String,
}

/// How the caller intends to use the accelerator.
///
/// Sustained workloads (embeddings / background ranking) prefer NPU for power
/// efficiency. Interactive bursts (image classify on selection) prefer a discrete
/// GPU for lowest latency, then NPU, then iGPU, then CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiWorkload {
    Sustained,
    Interactive,
}

/// Returns true when the OS build is Windows 11 24H2+ (build >= 26100).
#[cfg(windows)]
pub fn windows_ml_ep_supported() -> bool {
    build_number().map(|b| b >= 26100).unwrap_or(false)
}

#[cfg(windows)]
fn build_number() -> Option<u32> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
    };
    unsafe {
        let mut key = Default::default();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            Some(0),
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return None;
        }
        let mut buf = [0u16; 64];
        let mut size = (buf.len() * 2) as u32;
        let mut ty = REG_SZ;
        let ok = RegQueryValueExW(
            key,
            w!("CurrentBuildNumber"),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if ok.is_err() {
            return None;
        }
        let len = (size as usize / 2).saturating_sub(1).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]);
        s.trim_end_matches('\0').parse().ok()
    }
}

#[cfg(not(windows))]
pub fn windows_ml_ep_supported() -> bool {
    false
}

/// Look for already-installed Windows ML EP package DLLs.
pub fn discover_winml_ep_libraries() -> Vec<(String, PathBuf)> {
    #[cfg(windows)]
    {
        if !windows_ml_ep_supported() {
            return Vec::new();
        }
        let mut found = Vec::new();
        let roots = [
            std::env::var_os("ProgramFiles")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files")),
            std::env::var_os("ProgramFiles(x86)")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)")),
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Microsoft")
                .join("WindowsApps"),
        ];
        let patterns: &[(&str, &str)] = &[
            ("OpenVINOExecutionProvider", "onnxruntime_providers_openvino.dll"),
            ("QNNExecutionProvider", "onnxruntime_providers_qnn.dll"),
            ("VitisAIExecutionProvider", "onnxruntime_providers_vitisai.dll"),
            (
                "NvTensorRtRtxExecutionProvider",
                "onnxruntime_providers_nv_tensorrt_rtx.dll",
            ),
        ];
        for root in roots {
            if !root.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                            for (ep, dll) in patterns {
                                if name.eq_ignore_ascii_case(dll) {
                                    found.push(((*ep).to_string(), p.clone()));
                                }
                            }
                        }
                    } else if p.is_dir() {
                        if let Ok(inner) = std::fs::read_dir(&p) {
                            for e2 in inner.flatten() {
                                let p2 = e2.path();
                                if let Some(name) = p2.file_name().and_then(|n| n.to_str()) {
                                    for (ep, dll) in patterns {
                                        if name.eq_ignore_ascii_case(dll) {
                                            found.push(((*ep).to_string(), p2.clone()));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found.dedup_by(|a, b| a.0 == b.0);
        found
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Default ordering: NPU → GPU → CPU (sustained / general).
pub fn provider_candidates() -> Vec<ProviderCandidate> {
    provider_candidates_for(AiWorkload::Sustained)
}

/// Build an ordered list of provider candidates for a workload class.
pub fn provider_candidates_for(workload: AiWorkload) -> Vec<ProviderCandidate> {
    #[cfg(not(windows))]
    {
        let _ = workload;
        return vec![ProviderCandidate {
            label: "CPU".into(),
            kind: AcceleratorKind::Cpu,
            tag: "cpu".into(),
        }];
    }
    #[cfg(windows)]
    {
        let has_npu = !crate::gpu_detect::detect_npus().is_empty();
        let inv = crate::gpu_detect::detect_gpus();
        let winml = discover_winml_ep_libraries();

        let mut npu_eps = Vec::new();
        let mut gpu_eps = Vec::new();
        for (ep, _) in &winml {
            let kind = if ep.contains("QNN") || ep.contains("VitisAI") {
                AcceleratorKind::Npu
            } else if ep.contains("OpenVINO") {
                if has_npu {
                    AcceleratorKind::Npu
                } else {
                    AcceleratorKind::Gpu
                }
            } else {
                AcceleratorKind::Gpu
            };
            let candidate = ProviderCandidate {
                label: ep.clone(),
                kind,
                tag: format!("winml:{ep}"),
            };
            match kind {
                AcceleratorKind::Npu => npu_eps.push(candidate),
                AcceleratorKind::Gpu => gpu_eps.push(candidate),
                AcceleratorKind::Cpu => {}
            }
        }
        if has_npu {
            npu_eps.push(ProviderCandidate {
                label: "DirectML NPU".into(),
                kind: AcceleratorKind::Npu,
                tag: "dml:npu".into(),
            });
            npu_eps.push(ProviderCandidate {
                label: "DirectML HighPerformance".into(),
                kind: AcceleratorKind::Npu,
                tag: "dml:any_hp".into(),
            });
        }

        let mut dgpu = Vec::new();
        let mut igpu = Vec::new();
        for (i, adapter) in inv.adapters.iter().enumerate() {
            if !adapter.is_hardware {
                continue;
            }
            let candidate = ProviderCandidate {
                label: format!(
                    "DirectML {} ({})",
                    if adapter.is_discrete { "dGPU" } else { "iGPU" },
                    adapter.name
                ),
                kind: AcceleratorKind::Gpu,
                tag: format!("dml:idx:{i}"),
            };
            if adapter.is_discrete {
                dgpu.push(candidate);
            } else {
                igpu.push(candidate);
            }
        }

        let mut out = Vec::new();
        match workload {
            // Continuous embedding / ranking: NPU first (efficient), then iGPU
            // (shared power budget), then dGPU, then CPU.
            AiWorkload::Sustained => {
                out.extend(npu_eps);
                out.extend(gpu_eps);
                out.extend(igpu);
                out.extend(dgpu);
            }
            // User-facing classify: dGPU burst latency, then NPU, then iGPU.
            AiWorkload::Interactive => {
                out.extend(dgpu);
                out.extend(npu_eps);
                out.extend(gpu_eps);
                out.extend(igpu);
            }
        }
        out.push(ProviderCandidate {
            label: "DirectML default".into(),
            kind: AcceleratorKind::Gpu,
            tag: "dml:default".into(),
        });
        out.push(ProviderCandidate {
            label: "CPU".into(),
            kind: AcceleratorKind::Cpu,
            tag: "cpu".into(),
        });
        out
    }
}
