//! Monitor refresh detection and animation tick budgets.
//!
//! Slint + winit already vsync to the display. Decorative theme timers used a
//! fixed ~30 Hz cadence; on 144/240 Hz panels that left motion looking stepped.
//! Settings → Performance → Display refresh (Auto / 60 / 120 / 144 / 165 / 240)
//! resolves a target Hz, then we derive:
//!   - `theme_tick_ms` — Timer interval for continuous atmosphere clocks
//!   - `theme_step_scale` — multiply per-tick deltas so wall-clock speed stays
//!     constant when the tick rate changes (baseline = 33 ms ≈ 30 Hz)

/// Baseline atmosphere tick used when the original timers were authored.
pub const BASELINE_TICK_MS: u32 = 33;

#[derive(Debug, Clone)]
pub struct DisplayTiming {
    /// Highest refresh rate among attached displays (best effort).
    pub monitor_hz: u32,
    /// Resolved pacing target after applying the user setting.
    pub target_hz: u32,
    /// Timer interval for continuous theme motion.
    pub theme_tick_ms: u32,
    /// Scale factor for per-tick phase deltas (`tick_ms / 33`).
    pub theme_step_scale: f32,
    pub summary: String,
}

pub fn resolve(setting: &str) -> DisplayTiming {
    resolve_with_monitor(setting, detect_monitor_hz())
}

pub fn resolve_with_monitor(setting: &str, monitor_hz: u32) -> DisplayTiming {
    let monitor_hz = monitor_hz.clamp(30, 360);
    let target_hz = match normalize_setting(setting) {
        "60" => 60,
        "120" => 120.min(monitor_hz.max(60)),
        "144" => 144.min(monitor_hz.max(60)),
        "165" => 165.min(monitor_hz.max(60)),
        "240" => 240.min(monitor_hz.max(60)),
        // Auto: match the panel, but never below 60 for motion smoothness.
        _ => monitor_hz.max(60),
    };

    // Cap decorative timers at the display rate. Floor at 4 ms (~250 Hz) so we
    // never schedule tighter than a 360 Hz panel would need; ceiling at 33 ms
    // so low-Hz / Auto-on-60 still matches the original atmosphere authoring.
    let theme_tick_ms = (1000u32 / target_hz.max(1)).clamp(4, BASELINE_TICK_MS);
    let theme_step_scale = theme_tick_ms as f32 / BASELINE_TICK_MS as f32;

    let summary = format!(
        "Display {monitor_hz} Hz · pacing {target_hz} Hz ({theme_tick_ms} ms ticks)"
    );

    DisplayTiming {
        monitor_hz,
        target_hz,
        theme_tick_ms,
        theme_step_scale,
        summary,
    }
}

pub fn normalize_setting(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "60" => "60",
        "120" => "120",
        "144" => "144",
        "165" => "165",
        "240" => "240",
        _ => "auto",
    }
}

pub fn detect_monitor_hz() -> u32 {
    #[cfg(windows)]
    {
        detect_windows_monitor_hz().unwrap_or(60).max(30)
    }
    #[cfg(not(windows))]
    {
        60
    }
}

#[cfg(windows)]
fn detect_windows_monitor_hz() -> Option<u32> {
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, EnumDisplaySettingsW, DEVMODEW, DISPLAY_DEVICEW,
        ENUM_CURRENT_SETTINGS,
    };

    let mut max_hz = 0u32;

    // Primary display (null device name).
    let mut mode = DEVMODEW::default();
    mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR::null(),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    if ok.as_bool() {
        let hz = mode.dmDisplayFrequency;
        if (1..1000).contains(&hz) {
            max_hz = max_hz.max(hz);
        }
    }

    // Walk attached adapters for the highest active mode.
    let mut adapter = 0u32;
    while adapter <= 16 {
        let mut device = DISPLAY_DEVICEW::default();
        device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        let listed = unsafe { EnumDisplayDevicesW(None, adapter, &mut device, 0) };
        if !listed.as_bool() {
            break;
        }
        let mut mode = DEVMODEW::default();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        let name = windows::core::PCWSTR(device.DeviceName.as_ptr());
        let mode_ok = unsafe { EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut mode) };
        if mode_ok.as_bool() {
            let hz = mode.dmDisplayFrequency;
            if (1..1000).contains(&hz) {
                max_hz = max_hz.max(hz);
            }
        }
        adapter += 1;
    }

    (max_hz > 0).then_some(max_hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_matches_monitor_floor_60() {
        let t = resolve_with_monitor("auto", 240);
        assert_eq!(t.target_hz, 240);
        assert_eq!(t.theme_tick_ms, 4); // 1000/240 ≈ 4
        assert!((t.theme_step_scale - 4.0 / 33.0).abs() < 0.001);
    }

    #[test]
    fn setting_240_caps_to_monitor() {
        let t = resolve_with_monitor("240", 144);
        assert_eq!(t.target_hz, 144);
    }

    #[test]
    fn setting_60_fixed() {
        let t = resolve_with_monitor("60", 240);
        assert_eq!(t.target_hz, 60);
        assert_eq!(t.theme_tick_ms, 16); // 1000/60
    }

    #[test]
    fn normalize_unknown_is_auto() {
        assert_eq!(normalize_setting("bogus"), "auto");
        assert_eq!(normalize_setting("144"), "144");
    }
}
