//! File categories and default sub-directories.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Video,
    Music,
    Compressed,
    Programs,
    Documents,
    General,
}

const VIDEO: &[&str] = &["mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "ts", "m2ts", "mpg", "mpeg", "wmv", "asf", "3gp", "ogv"];
const MUSIC: &[&str] = &["mp3", "m4a", "aac", "ogg", "opus", "flac", "wav", "wma"];
const COMPRESSED: &[&str] = &["zip", "7z", "rar", "gz", "tar", "xz", "bz2", "tgz", "zst", "iso"];
const PROGRAMS: &[&str] = &["exe", "msi", "deb", "rpm", "appimage", "apk", "dmg", "sh", "run", "flatpakref"];
const DOCUMENTS: &[&str] = &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt", "epub", "md"];

impl Category {
    pub fn from_ext(ext: &str) -> Category {
        let e = ext.to_ascii_lowercase();
        let e = e.as_str();
        if VIDEO.contains(&e) {
            Category::Video
        } else if MUSIC.contains(&e) {
            Category::Music
        } else if COMPRESSED.contains(&e) {
            Category::Compressed
        } else if PROGRAMS.contains(&e) {
            Category::Programs
        } else if DOCUMENTS.contains(&e) {
            Category::Documents
        } else {
            Category::General
        }
    }

    pub fn from_id(id: &str) -> Option<Category> {
        Some(match id {
            "video" => Category::Video,
            "music" => Category::Music,
            "compressed" => Category::Compressed,
            "programs" => Category::Programs,
            "documents" => Category::Documents,
            "general" => Category::General,
            _ => return None,
        })
    }

    pub fn id(self) -> &'static str {
        match self {
            Category::Video => "video",
            Category::Music => "music",
            Category::Compressed => "compressed",
            Category::Programs => "programs",
            Category::Documents => "documents",
            Category::General => "general",
        }
    }

    /// Sub-directory name below the download directory.
    pub fn subdir(self) -> &'static str {
        match self {
            Category::Video => "Video",
            Category::Music => "Music",
            Category::Compressed => "Compressed",
            Category::Programs => "Programs",
            Category::Documents => "Documents",
            Category::General => "General",
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Category::Video => "视频",
            Category::Music => "音乐",
            Category::Compressed => "压缩包",
            Category::Programs => "程序",
            Category::Documents => "文档",
            Category::General => "其他",
        }
    }
}

/// Target directory for a file name.
pub fn target_dir(download_dir: &Path, by_category: bool, filename: &str) -> PathBuf {
    if !by_category {
        return download_dir.to_path_buf();
    }
    let ext = super::filename::extension_of(filename);
    download_dir.join(Category::from_ext(&ext).subdir())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn categorize() {
        assert_eq!(Category::from_ext("MP4"), Category::Video);
        assert_eq!(Category::from_ext("flac"), Category::Music);
        assert_eq!(Category::from_ext("deb"), Category::Programs);
        assert_eq!(Category::from_ext("xyz"), Category::General);
        assert_eq!(target_dir(Path::new("/d"), true, "a.zip"), PathBuf::from("/d/Compressed"));
        assert_eq!(target_dir(Path::new("/d"), false, "a.zip"), PathBuf::from("/d"));
    }
}
