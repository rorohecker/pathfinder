//! Cloud placeholder detection (OneDrive, iCloud Drive, Dropbox, etc.).

use std::path::Path;

#[cfg(target_os = "windows")]
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
#[cfg(target_os = "windows")]
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
#[cfg(target_os = "windows")]
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// True when reading/decoding this path would likely hydrate a cloud placeholder.
pub fn hydration_risk(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            return hydration_risk_from_attrs(meta.file_attributes());
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        false
    }
}

#[cfg(target_os = "windows")]
pub fn hydration_risk_from_attrs(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_OFFLINE != 0
        || attrs & FILE_ATTRIBUTE_RECALL_ON_OPEN != 0
        || attrs & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0
}

/// Path is under a well-known desktop sync folder (OneDrive Personal, etc.).
#[cfg(target_os = "windows")]
fn path_under_cloud_sync_root(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    // Normalise to backslashes so `Users\me\OneDrive\...` matches reliably.
    let norm = lower.replace('/', "\\");
    const MARKERS: &[&str] = [
        "\\onedrive\\",
        "\\onedrive - ",
        "\\icloudrive\\",
        "\\dropbox\\",
        "\\google drive\\",
        "\\my drive\\",
        "\\proton drive\\",
        "\\protondrive\\",
        "\\box\\",
        "\\mega\\",
    ];
    MARKERS.iter().any(|m| norm.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_attrs_flagged() {
        #[cfg(target_os = "windows")]
        {
            assert!(hydration_risk_from_attrs(
                FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            ));
            assert!(hydration_risk_from_attrs(FILE_ATTRIBUTE_OFFLINE));
            assert!(!hydration_risk_from_attrs(0x20)); // FILE_ATTRIBUTE_ARCHIVE
        }
    }

    #[test]
    fn onedrive_path_heuristic() {
        #[cfg(target_os = "windows")]
        {
            let p = Path::new(r"C:\Users\me\OneDrive\Pictures\vacation.jpg");
            assert!(path_under_cloud_sync_root(p));
        }
    }
}
