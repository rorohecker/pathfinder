//! Hybrid CPU topology (Intel P/E, AMD prefer/efficiency cores) and thread QoS.
//!
//! On Windows 10+ `GetLogicalProcessorInformationEx` reports an
//! `EfficiencyClass` per core. Microsoft documents that a *higher* class means
//! intrinsically greater performance (and less efficiency). We pin background
//! work (thumbnails, indexing) to the lowest class with EcoQoS, and keep burst
//! / ONNX-CPU work on the highest class with HighQoS so interactive paths stay
//! snappy without starving the UI.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuVendor {
    Intel,
    Amd,
    Other,
}

impl CpuVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intel => "Intel",
            Self::Amd => "AMD",
            Self::Other => "CPU",
        }
    }
}

/// How a worker thread should schedule itself on hybrid hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRole {
    /// Thumbnails, indexing, idle scans — prefer efficiency cores + EcoQoS.
    Background,
    /// Interactive bursts (first search page, storage kickoff) — P-cores + HighQoS.
    Burst,
    /// ONNX Runtime CPU EP — P-cores, HighQoS, no Eco throttle.
    AiCpu,
}

#[derive(Debug, Clone)]
pub struct CpuTopology {
    pub vendor: CpuVendor,
    pub logical_cpus: u32,
    pub performance_count: u32,
    pub efficiency_count: u32,
    pub is_hybrid: bool,
    /// Bitmask of logical processors in group 0 (Windows affinity).
    pub performance_mask: usize,
    pub efficiency_mask: usize,
    pub all_mask: usize,
    pub brand: String,
    pub summary: String,
}

impl CpuTopology {
    pub fn background_threads(&self) -> usize {
        let n = if self.is_hybrid && self.efficiency_count > 0 {
            self.efficiency_count as usize
        } else {
            (self.logical_cpus as usize).saturating_sub(1).max(2)
        };
        n.clamp(2, 8)
    }

    pub fn burst_threads(&self) -> usize {
        let n = if self.is_hybrid && self.performance_count > 0 {
            self.performance_count as usize
        } else {
            (self.logical_cpus as usize / 2).max(2)
        };
        n.clamp(2, 8)
    }

    /// Intra-op threads for ORT CPU sessions — never oversubscribe E-cores.
    pub fn ai_cpu_threads(&self) -> usize {
        let n = if self.is_hybrid && self.performance_count > 0 {
            self.performance_count as usize
        } else {
            (self.logical_cpus as usize).saturating_sub(1).max(1)
        };
        n.clamp(1, 8)
    }
}

static TOPOLOGY: OnceLock<CpuTopology> = OnceLock::new();

pub fn topology() -> &'static CpuTopology {
    TOPOLOGY.get_or_init(detect_topology)
}

/// Apply affinity + power QoS for the calling thread. Best-effort; never panics.
pub fn configure_current_thread(role: ThreadRole) {
    #[cfg(windows)]
    {
        apply_windows_thread_role(role);
    }
    #[cfg(not(windows))]
    {
        let _ = role;
    }
}

fn detect_topology() -> CpuTopology {
    #[cfg(windows)]
    {
        detect_windows_topology()
    }
    #[cfg(not(windows))]
    {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4)
            .max(1);
        let all_mask = mask_for_count(logical);
        CpuTopology {
            vendor: CpuVendor::Other,
            logical_cpus: logical,
            performance_count: logical,
            efficiency_count: 0,
            is_hybrid: false,
            performance_mask: all_mask,
            efficiency_mask: 0,
            all_mask,
            brand: String::new(),
            summary: format!("{logical} logical processors (homogeneous)"),
        }
    }
}

fn mask_for_count(logical: u32) -> usize {
    if logical >= usize::BITS {
        usize::MAX
    } else {
        (1usize << logical) - 1
    }
}

#[cfg(windows)]
fn detect_windows_topology() -> CpuTopology {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    let brand = processor_brand_string();
    let vendor = classify_vendor(&brand);

    let mut length = 0u32;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut length);
    }
    if length == 0 {
        return fallback_topology(vendor, brand);
    }

    let mut buffer = vec![0u8; length as usize];
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buffer.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()),
            &mut length,
        )
    };
    if ok.is_err() {
        return fallback_topology(vendor, brand);
    }

    // (efficiency class, group-0 affinity mask or 0, logical processor count)
    let mut cores: Vec<(u8, usize, u32)> = Vec::new();
    let mut offset = 0usize;
    while offset + std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() <= buffer.len()
    {
        let info = unsafe {
            &*(buffer.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
        let size = info.Size as usize;
        if size == 0 || offset + size > buffer.len() {
            break;
        }
        if info.Relationship == RelationProcessorCore {
            let processor = unsafe { info.Anonymous.Processor };
            if processor.GroupCount > 0 {
                let group = processor.GroupMask[0];
                if group.Mask != 0 {
                    // SetThreadAffinityMask only accepts group-0 masks. Still
                    // count logical processors in other groups for budgets.
                    let affinity_mask = if group.Group == 0 { group.Mask } else { 0 };
                    cores.push((
                        processor.EfficiencyClass,
                        affinity_mask,
                        group.Mask.count_ones(),
                    ));
                }
            }
        }
        offset += size;
    }

    if cores.is_empty() {
        return fallback_topology(vendor, brand);
    }

    let max_class = cores.iter().map(|(c, _, _)| *c).max().unwrap_or(0);
    let min_class = cores.iter().map(|(c, _, _)| *c).min().unwrap_or(0);
    let is_hybrid = max_class > min_class;

    let mut performance_mask = 0usize;
    let mut efficiency_mask = 0usize;
    let mut all_mask = 0usize;
    let mut performance_count = 0u32;
    let mut efficiency_count = 0u32;
    let mut logical_cpus = 0u32;

    for (class, mask, bits) in &cores {
        all_mask |= mask;
        logical_cpus += bits;
        if is_hybrid {
            if *class == max_class {
                performance_mask |= mask;
                performance_count += bits;
            }
            if *class == min_class {
                efficiency_mask |= mask;
                efficiency_count += bits;
            }
        } else {
            performance_mask |= mask;
            performance_count += bits;
        }
    }

    let logical_cpus = logical_cpus.max(1);
    if !is_hybrid {
        efficiency_mask = 0;
        efficiency_count = 0;
    }

    let summary = if is_hybrid {
        format!(
            "{} hybrid: {}P / {}E logical ({} total) — background→E, burst/AI→P",
            vendor.as_str(),
            performance_count,
            efficiency_count,
            logical_cpus
        )
    } else {
        format!(
            "{}: {} logical processors (homogeneous) — EcoQoS on background work",
            vendor.as_str(),
            logical_cpus
        )
    };

    CpuTopology {
        vendor,
        logical_cpus,
        performance_count,
        efficiency_count,
        is_hybrid,
        performance_mask,
        efficiency_mask,
        all_mask,
        brand,
        summary,
    }
}

#[cfg(windows)]
fn fallback_topology(vendor: CpuVendor, brand: String) -> CpuTopology {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
        .max(1);
    let all_mask = mask_for_count(logical);
    CpuTopology {
        vendor,
        logical_cpus: logical,
        performance_count: logical,
        efficiency_count: 0,
        is_hybrid: false,
        performance_mask: all_mask,
        efficiency_mask: 0,
        all_mask,
        brand,
        summary: format!(
            "{}: {} logical processors (topology probe unavailable)",
            vendor.as_str(),
            logical
        ),
    }
}

#[cfg(windows)]
fn processor_brand_string() -> String {
    std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("PROCESSOR_ARCHITECTURE").ok())
        .unwrap_or_default()
}

fn classify_vendor(brand: &str) -> CpuVendor {
    let b = brand.to_ascii_lowercase();
    if b.contains("intel") || b.contains("genuineintel") {
        CpuVendor::Intel
    } else if b.contains("amd") || b.contains("authenticamd") {
        CpuVendor::Amd
    } else {
        CpuVendor::Other
    }
}

#[cfg(windows)]
fn apply_windows_thread_role(role: ThreadRole) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadAffinityMask, SetThreadInformation, SetThreadPriority,
        ThreadPowerThrottling, THREAD_POWER_THROTTLING_CURRENT_VERSION,
        THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
        THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
    };

    let topo = topology();
    let (mask, eco, priority) = match role {
        ThreadRole::Background => {
            let mask = if topo.is_hybrid && topo.efficiency_mask != 0 {
                topo.efficiency_mask
            } else {
                0 // leave affinity alone; EcoQoS still helps on homogeneous CPUs
            };
            (mask, true, THREAD_PRIORITY_BELOW_NORMAL)
        }
        ThreadRole::Burst => {
            let mask = if topo.is_hybrid && topo.performance_mask != 0 {
                topo.performance_mask
            } else {
                0
            };
            (mask, false, THREAD_PRIORITY_ABOVE_NORMAL)
        }
        ThreadRole::AiCpu => {
            let mask = if topo.is_hybrid && topo.performance_mask != 0 {
                topo.performance_mask
            } else {
                0
            };
            (mask, false, THREAD_PRIORITY_NORMAL)
        }
    };

    unsafe {
        let thread = GetCurrentThread();
        if mask != 0 {
            let _ = SetThreadAffinityMask(thread, mask);
        }
        let _ = SetThreadPriority(thread, priority);

        let power = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if eco {
                THREAD_POWER_THROTTLING_EXECUTION_SPEED
            } else {
                0
            },
        };
        let _ = SetThreadInformation(
            thread,
            ThreadPowerThrottling,
            &power as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_from_brand() {
        assert_eq!(classify_vendor("Intel64 Family 6"), CpuVendor::Intel);
        assert_eq!(classify_vendor("AMD64 Family 25"), CpuVendor::Amd);
        assert_eq!(classify_vendor("Unknown"), CpuVendor::Other);
    }

    #[test]
    fn topology_reports_positive_cpus() {
        let t = topology();
        assert!(t.logical_cpus >= 1);
        assert!(!t.summary.is_empty());
        assert!(t.background_threads() >= 2);
        assert!(t.ai_cpu_threads() >= 1);
    }
}
