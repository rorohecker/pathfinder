//! Direct GPU and NPU detection via Win32, no PowerShell.
//!
//! The old code spawned PowerShell with `Get-CimInstance Win32_VideoController` and
//! `Get-PnpDevice` at startup, which paid a ~1 second process spawn cost twice and
//! delayed the AI tab status. DXGI enumeration runs in single-digit milliseconds
//! and gives us richer info (per-adapter `DedicatedVideoMemory`, vendor IDs, and
//! the software/remote flag) that the PowerShell path couldn't see. SetupDiGetClassDevs
//! against the ComputeAccelerator class GUID does the same for NPUs.
//!
//! Caching: every probe is wrapped in OnceLock so each session only runs the Win32
//! calls once. `reset_caches()` is provided for future code paths that need a fresh
//! probe after install/uninstall.
//!
//! Why DedicatedVideoMemory is enough for dGPU classification: every real discrete
//! GPU shipped this decade has at least 2 GB dedicated VRAM. Integrated GPUs report
//! a tiny dedicated allocation (a few hundred MB carve-out from system RAM, often
//! 512 MB or less). The threshold below cleanly splits them without needing brittle
//! string matching on adapter names.

#[cfg(windows)]
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct GpuAdapter {
    pub name: String,
    pub vendor_id: u32,
    pub dedicated_video_mb: u64,
    pub dedicated_system_mb: u64,
    pub shared_system_mb: u64,
    /// True when the IHV vendor ID matches a known hardware vendor and the
    /// adapter is not flagged as software-only / remote.
    pub is_hardware: bool,
    pub is_discrete: bool,
}

impl GpuAdapter {
    pub fn vendor_name(&self) -> &'static str {
        match self.vendor_id {
            0x10DE => "NVIDIA",
            0x1002 | 0x1022 => "AMD",
            0x8086 => "Intel",
            0x14E4 => "Broadcom",
            0x1B36 => "Red Hat (Virtio)",
            0x1AF4 => "Virtio",
            0x1414 => "Microsoft Basic",
            _ => "Unknown vendor",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GpuInventory {
    pub adapters: Vec<GpuAdapter>,
}

impl GpuInventory {
    pub fn discrete(&self) -> Vec<&GpuAdapter> {
        self.adapters.iter().filter(|a| a.is_hardware && a.is_discrete).collect()
    }

    pub fn integrated(&self) -> Vec<&GpuAdapter> {
        self.adapters.iter().filter(|a| a.is_hardware && !a.is_discrete).collect()
    }

    pub fn primary_directml_target(&self) -> Option<&GpuAdapter> {
        self.discrete().into_iter().next().or_else(|| self.integrated().into_iter().next())
    }
}

#[cfg(windows)]
static GPU_INVENTORY: OnceLock<GpuInventory> = OnceLock::new();

/// 2 GiB dedicated VRAM split between iGPU and dGPU. The biggest current iGPUs
/// (AMD Radeon 780M / 880M, Intel Arc graphics in Lunar Lake / Meteor Lake) report
/// well under this. Every shipping dGPU since the GTX 10-series clears it easily.
const DGPU_DEDICATED_MB_THRESHOLD: u64 = 1500;

pub fn detect_gpus() -> GpuInventory {
    #[cfg(windows)]
    {
        if let Some(cached) = GPU_INVENTORY.get() {
            return cached.clone();
        }
        let inv = enumerate_dxgi();
        let _ = GPU_INVENTORY.set(inv.clone());
        inv
    }
    #[cfg(not(windows))]
    {
        GpuInventory { adapters: Vec::new() }
    }
}

#[cfg(windows)]
fn enumerate_dxgi() -> GpuInventory {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_REMOTE,
        DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory1,
    };
    let mut adapters: Vec<GpuAdapter> = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(_) => return GpuInventory { adapters },
        };
        let mut i = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(i) {
                Ok(a) => a,
                Err(_) => break,
            };
            let desc: DXGI_ADAPTER_DESC1 = match adapter.GetDesc1() {
                Ok(d) => d,
                Err(_) => {
                    i += 1;
                    continue;
                }
            };
            // Description is wide-char, null-terminated.
            let name_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..name_len]);
            let flags = desc.Flags;
            let is_software = (flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
            let is_remote = (flags & DXGI_ADAPTER_FLAG_REMOTE.0 as u32) != 0;
            let known_vendor = matches!(desc.VendorId, 0x10DE | 0x1002 | 0x1022 | 0x8086);
            let dedicated_video_mb = (desc.DedicatedVideoMemory as u64) / (1024 * 1024);
            let dedicated_system_mb = (desc.DedicatedSystemMemory as u64) / (1024 * 1024);
            let shared_system_mb = (desc.SharedSystemMemory as u64) / (1024 * 1024);
            let is_hardware = !is_software && !is_remote && known_vendor;
            let is_discrete = is_hardware && dedicated_video_mb >= DGPU_DEDICATED_MB_THRESHOLD;
            adapters.push(GpuAdapter {
                name,
                vendor_id: desc.VendorId,
                dedicated_video_mb,
                dedicated_system_mb,
                shared_system_mb,
                is_hardware,
                is_discrete,
            });
            i += 1;
        }
    }
    GpuInventory { adapters }
}

/// Returns the zero-based DXGI adapter index for the preferred DirectML target
/// (first discrete GPU, falling back to first integrated). DirectML accepts this
/// index directly via its execution provider options. None when no real hardware
/// adapter is reachable (only software/virtual adapters).
#[cfg(windows)]
pub fn preferred_directml_adapter_index() -> Option<u32> {
    let inv = detect_gpus();
    let target_name = inv.primary_directml_target()?.name.clone();
    inv.adapters
        .iter()
        .position(|a| a.name == target_name)
        .map(|p| p as u32)
}

#[cfg(not(windows))]
pub fn preferred_directml_adapter_index() -> Option<u32> {
    None
}

/// NPU enumeration. Three-stage probe to catch every variant Windows uses:
///
///   1. ComputeAccelerator class GUID variant A (`...c8a7004be10c`). This is the
///      GUID actually present on AMD Ryzen AI / XDNA driver installs verified
///      on a Ryzen 9 7940HS box. The class name in Device Manager is
///      ComputeAccelerator and the device shows up as "NPU Compute Accelerator
///      Device".
///   2. ComputeAccelerator class GUID variant B (`...e8c40d6664c8`). This is
///      the GUID that some Microsoft documentation lists. We try it second so
///      future driver releases that use it still resolve.
///   3. All-classes fallback. If neither class GUID returns anything, we scan
///      every present device on the system and filter by friendly-name pattern
///      so we still catch NPUs registered under System or any other class.
///
/// Diagnostic logging from the example probe in /examples/probe_npu.rs makes
/// it easy to find the right GUID when porting to new hardware.
#[cfg(windows)]
pub fn detect_npus() -> Vec<String> {
    use windows::core::GUID;
    // Variant A: actual GUID observed on real Ryzen AI hardware (verified via
    // Get-PnpDevice ClassGuid). Try this first because it's the value drivers
    // actually use right now.
    let guid_a = GUID::from_values(
        0xf01a9d53,
        0x3ff6,
        0x48d2,
        [0x9f, 0x97, 0xc8, 0xa7, 0x00, 0x4b, 0xe1, 0x0c],
    );
    // Variant B: GUID that appears in some Microsoft documentation. Kept as a
    // belt-and-suspenders fallback in case any driver actually uses it.
    let guid_b = GUID::from_values(
        0xf01a9d53,
        0x3ff6,
        0x48d2,
        [0x9f, 0x97, 0xe8, 0xc4, 0x0d, 0x66, 0x64, 0xc8],
    );
    let mut results = enumerate_npus_in_class(Some(guid_a));
    if results.is_empty() {
        results = enumerate_npus_in_class(Some(guid_b));
    }
    if results.is_empty() {
        results = enumerate_npus_in_class(None);
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    results.retain(|n| seen.insert(n.clone()));
    results
}

#[cfg(windows)]
fn enumerate_npus_in_class(class_guid: Option<windows::core::GUID>) -> Vec<String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA,
    };
    use windows::Win32::Devices::Properties::{
        DEVPKEY_Device_DeviceDesc, DEVPKEY_Device_FriendlyName,
    };

    let flags = if class_guid.is_some() {
        DIGCF_PRESENT
    } else {
        DIGCF_PRESENT | DIGCF_ALLCLASSES
    };
    let mut results: Vec<String> = Vec::new();
    unsafe {
        let guid_ptr: Option<*const windows::core::GUID> =
            class_guid.as_ref().map(|g| g as *const _);
        let h = match SetupDiGetClassDevsW(guid_ptr, None, None, flags) {
            Ok(h) => h,
            Err(_) => return results,
        };
        let mut index: u32 = 0;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(h, index, &mut data).is_err() {
                break;
            }
            // FriendlyName isn't always set - the AMD Ryzen AI XDNA NPU on a
            // Ryzen 9 7940HS for example only has DeviceDesc populated. Try
            // FriendlyName first (more user-facing when present) and fall back
            // to DeviceDesc otherwise so we don't silently skip the NPU.
            let name = read_dev_prop(h, &data, &DEVPKEY_Device_FriendlyName)
                .or_else(|| read_dev_prop(h, &data, &DEVPKEY_Device_DeviceDesc));
            if let Some(name) = name {
                let lower = name.to_ascii_lowercase();
                let positive = lower.contains("npu")
                    || lower.contains("neural processing")
                    || lower.contains("neural processor")
                    || lower.contains(" ai boost")
                    || lower.contains("(r) ai boost")
                    || lower.contains("amd ipu")
                    || lower.contains("ryzen ai")
                    || lower.contains("xdna")
                    || lower.contains("hexagon npu")
                    || lower.contains("hexagon ai")
                    || lower.contains("hailo")
                    || lower.contains("movidius");
                let negative = lower.contains("audio")
                    || lower.contains("display adapter")
                    || lower.contains("wireless")
                    || lower.contains("bluetooth")
                    || lower.contains("network adapter");
                if positive && !negative {
                    results.push(name);
                }
            }
            index += 1;
        }
        let _ = SetupDiDestroyDeviceInfoList(h);
    }
    results
}

#[cfg(not(windows))]
pub fn detect_npus() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
fn read_dev_prop(
    h: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    data: &windows::Win32::Devices::DeviceAndDriverInstallation::SP_DEVINFO_DATA,
    key: &windows::Win32::Foundation::DEVPROPKEY,
) -> Option<String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::SetupDiGetDevicePropertyW;
    use windows::Win32::Devices::Properties::DEVPROPTYPE;
    unsafe {
        let mut prop_type = DEVPROPTYPE(0);
        let mut needed: u32 = 0;
        let _ = SetupDiGetDevicePropertyW(
            h, data, key, &mut prop_type, None, Some(&mut needed), 0,
        );
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        if SetupDiGetDevicePropertyW(
            h, data, key, &mut prop_type, Some(&mut buf), None, 0,
        )
        .is_err()
        {
            return None;
        }
        let wide: &[u16] = std::slice::from_raw_parts(
            buf.as_ptr() as *const u16,
            buf.len() / 2,
        );
        let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        let s = String::from_utf16_lossy(&wide[..len]).trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// Debug helper used by `cargo run --example probe_npu` to enumerate every
/// device under each class GUID + the all-classes scan and print friendly
/// names. Lets us see exactly which SetupDi call is returning nothing on a
/// machine where NPU detection fails.
#[cfg(windows)]
pub fn detect_npus_verbose() {
    use windows::core::GUID;
    let guid_a = GUID::from_values(
        0xf01a9d53,
        0x3ff6,
        0x48d2,
        [0x9f, 0x97, 0xc8, 0xa7, 0x00, 0x4b, 0xe1, 0x0c],
    );
    let guid_b = GUID::from_values(
        0xf01a9d53,
        0x3ff6,
        0x48d2,
        [0x9f, 0x97, 0xe8, 0xc4, 0x0d, 0x66, 0x64, 0xc8],
    );
    println!("--- ComputeAccelerator variant A ---");
    dump_class(Some(guid_a));
    println!("--- ComputeAccelerator variant B ---");
    dump_class(Some(guid_b));
    println!("--- All classes (filtered) ---");
    dump_class(None);
}

#[cfg(windows)]
fn dump_class(class_guid: Option<windows::core::GUID>) {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA,
    };
    use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;

    let flags = if class_guid.is_some() {
        DIGCF_PRESENT
    } else {
        DIGCF_PRESENT | DIGCF_ALLCLASSES
    };
    unsafe {
        let guid_ptr: Option<*const windows::core::GUID> =
            class_guid.as_ref().map(|g| g as *const _);
        let h = match SetupDiGetClassDevsW(guid_ptr, None, None, flags) {
            Ok(h) => h,
            Err(e) => {
                println!("  SetupDiGetClassDevsW failed: {:?}", e);
                return;
            }
        };
        println!("  handle ok, enumerating");
        let mut index: u32 = 0;
        let mut total = 0;
        let mut npu_like = 0;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(h, index, &mut data).is_err() {
                break;
            }
            total += 1;
            use windows::Win32::Devices::Properties::DEVPKEY_Device_DeviceDesc;
            let name_fn = read_dev_prop(h, &data, &DEVPKEY_Device_FriendlyName)
                .or_else(|| read_dev_prop(h, &data, &DEVPKEY_Device_DeviceDesc));
            if let Some(name) = name_fn {
                let lower = name.to_ascii_lowercase();
                if lower.contains("npu")
                    || lower.contains("neural")
                    || lower.contains("xdna")
                    || lower.contains("ipu")
                    || lower.contains("accelerator")
                    || lower.contains("ai boost")
                {
                    println!("  [{}] {}", index, name);
                    npu_like += 1;
                } else if class_guid.is_some() {
                    println!("  [{}] {} (not NPU-like)", index, name);
                }
            } else if class_guid.is_some() {
                println!("  [{}] <no name>", index);
            }
            index += 1;
        }
        println!("  total devices: {}, NPU-like: {}", total, npu_like);
        let _ = SetupDiDestroyDeviceInfoList(h);
    }
}

/// Reset caches. Currently only the GPU inventory is memoized; the NPU probe is
/// already fast enough that re-running on every call is fine.
#[cfg(windows)]
pub fn reset_caches() {
    // OnceLock does not expose a public reset; cache survives for the process.
    // Plumbed for future use if we ever need a runtime refresh.
}
