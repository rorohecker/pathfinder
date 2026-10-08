//! Filters cached file metadata without opening files or rescanning the folder.

#[derive(Default)]
pub struct FileFilter {
    name: String,
    extensions: Vec<String>,
    kind: i32,
    size: i32,
    modified: i32,
}

impl FileFilter {
    pub fn new(name: &str, extensions: &str, kind: i32, size: i32, modified: i32) -> Self {
        Self {
            name: name.trim().to_lowercase(),
            extensions: extensions
                .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '|'))
                .map(|ext| {
                    ext.strip_prefix("*.")
                        .unwrap_or(ext)
                        .trim_start_matches('.')
                        .to_lowercase()
                })
                .filter(|ext| !ext.is_empty())
                .collect(),
            kind: kind.clamp(0, 10),
            size: size.clamp(0, 5),
            modified: modified.clamp(0, 4),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.name.is_empty()
            || !self.extensions.is_empty()
            || self.kind != 0
            || self.size != 0
            || self.modified != 0
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn matches(
        &self,
        name_lower: &str,
        extension: &str,
        is_dir: bool,
        size: u64,
        modified: u64,
        now: u64,
    ) -> bool {
        if !name_lower.contains(&self.name) {
            return false;
        }
        if !self.extensions.is_empty()
            && (is_dir
                || !self
                    .extensions
                    .iter()
                    .any(|ext| ext.eq_ignore_ascii_case(extension)))
        {
            return false;
        }
        let has_ext = |choices: &[&str]| {
            choices
                .iter()
                .any(|ext| ext.eq_ignore_ascii_case(extension))
        };
        // Indices follow the translated category choices in FolderFilterBar.
        let kind_matches = match self.kind {
            0 => true,
            1 => !is_dir,
            2 => is_dir,
            3 => {
                !is_dir
                    && has_ext(&[
                        "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "csv", "xls", "xlsx",
                        "ods", "ppt", "pptx", "odp",
                    ])
            }
            4 => {
                !is_dir
                    && has_ext(&[
                        "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "ico", "tif", "tiff",
                        "tga", "heic", "heif", "avif",
                    ])
            }
            5 => !is_dir && has_ext(&["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus"]),
            6 => {
                !is_dir
                    && has_ext(&[
                        "mp4", "mov", "mkv", "avi", "webm", "wmv", "m4v", "mpeg", "mpg",
                    ])
            }
            7 => !is_dir && has_ext(&["zip", "7z", "rar", "tar", "gz", "xz", "bz2", "zst"]),
            8 => !is_dir && has_ext(&["exe", "msi", "msix", "appx", "bat", "cmd", "ps1"]),
            9 => !is_dir && has_ext(&["sys", "dll", "drv", "ocx"]),
            10 => !is_dir && extension.is_empty(),
            _ => unreachable!(),
        };
        if !kind_matches {
            return false;
        }
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * MB;
        let size_matches = match self.size {
            0 => true,
            1 => !is_dir && size == 0,
            2 => !is_dir && size < MB,
            3 => !is_dir && (MB..100 * MB).contains(&size),
            4 => !is_dir && (100 * MB..GB).contains(&size),
            5 => !is_dir && size >= GB,
            _ => unreachable!(),
        };
        if !size_matches {
            return false;
        }
        const DAY: u64 = 24 * 60 * 60;
        match self.modified {
            0 => true,
            // A zero timestamp means metadata was unavailable, not an old file.
            1 => modified != 0 && modified >= now.saturating_sub(DAY),
            2 => modified != 0 && modified >= now.saturating_sub(7 * DAY),
            3 => modified != 0 && modified >= now.saturating_sub(30 * DAY),
            4 => modified != 0 && modified < now.saturating_sub(365 * DAY),
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn extensions_accept_dots_case_and_multiple_choices() {
        let filter = FileFilter::new("", " .PDF, *.sys; txt | .png ", 0, 0, 0);
        for ext in ["pdf", "SYS", "txt", "png"] {
            assert!(filter.matches("file", ext, false, 0, NOW, NOW));
        }
        assert!(!filter.matches("pdf-notes.doc", "doc", false, 0, NOW, NOW));
        assert!(!filter.matches("folder.pdf", "pdf", true, 0, NOW, NOW));
    }

    #[test]
    fn filters_combine_and_reset_restores_all_entries() {
        let mut filter = FileFilter::new(" REPORT ", ".pdf", 3, 3, 2);
        assert!(filter.matches("annual report.pdf", "pdf", false, 2_000_000, NOW, NOW));
        assert!(!filter.matches("notes.pdf", "pdf", false, 2_000_000, NOW, NOW));
        assert!(!filter.matches("report.docx", "docx", false, 2_000_000, NOW, NOW));
        assert!(!filter.matches("report.pdf", "pdf", false, 100, NOW, NOW));
        assert!(!filter.matches("report.pdf", "pdf", false, 2_000_000, NOW - 8 * 86400, NOW));
        assert!(filter.is_active());
        filter.clear();
        assert!(!filter.is_active());
        assert!(filter.matches("folder", "", true, 0, 0, NOW));
    }

    #[test]
    fn size_boundaries_and_unknown_dates_are_explicit() {
        let small = FileFilter::new("", "", 0, 2, 0);
        let medium = FileFilter::new("", "", 0, 3, 0);
        assert!(small.matches("a", "", false, 1_048_575, NOW, NOW));
        assert!(!small.matches("a", "", false, 1_048_576, NOW, NOW));
        assert!(medium.matches("a", "", false, 1_048_576, NOW, NOW));
        assert!(!medium.matches("a", "", false, 100 * 1_048_576, NOW, NOW));
        assert!(!small.matches("folder", "", true, 0, NOW, NOW));
        let recent = FileFilter::new("", "", 0, 0, 1);
        assert!(recent.matches("a", "", false, 0, NOW - 86400, NOW));
        assert!(!recent.matches("a", "", false, 0, NOW - 86401, NOW));
        let old = FileFilter::new("", "", 0, 0, 4);
        assert!(!old.matches("a", "", false, 0, 0, NOW));
        assert!(old.matches("a", "", false, 0, NOW - 366 * 86400, NOW));
    }

    #[test]
    fn categories_distinguish_files_folders_and_extensionless_files() {
        for (kind, ext) in [
            (3, "pdf"),
            (4, "png"),
            (5, "flac"),
            (6, "mkv"),
            (7, "zip"),
            (8, "exe"),
            (9, "sys"),
            (10, ""),
        ] {
            let filter = FileFilter::new("", "", kind, 0, 0);
            assert!(filter.matches("file", ext, false, 0, NOW, NOW));
            assert!(!filter.matches("folder", ext, true, 0, NOW, NOW));
        }
        let folders = FileFilter::new("", "", 2, 0, 0);
        assert!(folders.matches("folder", "", true, 0, NOW, NOW));
        assert!(!folders.matches("file", "", false, 0, NOW, NOW));
        let files = FileFilter::new("", "", 1, 0, 0);
        assert!(files.matches("file", "", false, 0, NOW, NOW));
        assert!(!files.matches("folder", "", true, 0, NOW, NOW));
    }
}
