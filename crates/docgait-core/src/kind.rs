//! File kind from the extension. At the names-only level this is all docgait knows; the full
//! level checks magic bytes later.

/// Lower-case extension without the dot, or "" when there is none. Dotfiles like `.bashrc`
/// have no extension.
pub fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) => name[i + 1..].to_ascii_lowercase(),
    }
}

pub fn kind_of(ext: &str) -> &'static str {
    match ext {
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "markdown" | "wpd" | "wps"
        | "wri" | "lwp" | "pages" | "xls" | "xlsx" | "ods" | "csv" | "tsv" | "numbers" | "ppt"
        | "pptx" | "odp" | "key" | "html" | "htm" | "eml" | "msg" | "mbox" | "tex" | "org"
        | "rst" => "document",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "heic" | "heif"
        | "avif" | "svg" | "ico" | "psd" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "raf"
        | "orf" | "rw2" => "image",
        "mp3" | "m4a" | "m4b" | "aac" | "ogg" | "oga" | "opus" | "flac" | "wav" | "aiff"
        | "aif" | "wma" | "alac" => "audio",
        "mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "wmv" | "mpg" | "mpeg" | "3gp" => "video",
        "epub" | "mobi" | "azw" | "azw3" | "fb2" | "cbz" | "cbr" | "djvu" => "ebook",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "dmg" | "iso" => {
            "archive"
        }
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "cc" | "java"
        | "kt" | "go" | "rb" | "php" | "cs" | "swift" | "sh" | "zsh" | "bash" | "ps1" | "sql"
        | "json" | "yaml" | "yml" | "toml" | "xml" | "css" | "scss" | "lua" | "pl" | "r"
        | "scala" | "ex" | "exs" => "code",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension("Report.PDF"), "pdf");
        assert_eq!(extension("archive.tar.gz"), "gz");
        assert_eq!(extension(".bashrc"), "");
        assert_eq!(extension("README"), "");
        assert_eq!(kind_of("pdf"), "document");
        assert_eq!(kind_of("m4b"), "audio");
        assert_eq!(kind_of("xyz"), "other");
    }
}
