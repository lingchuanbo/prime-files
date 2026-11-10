//! The listing's filter bar: narrowing the folder on screen by kind and by a
//! name pattern, without touching the full-text search.

use nohrs_core::t;
use nohrs_services::fs::listing::FileEntryDto;

/// What kind of entry the listing shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KindFilter {
    /// Everything.
    #[default]
    All,
    /// Folders only.
    Folders,
    /// Anything that is not a folder.
    Files,
    /// Raster and vector images.
    Images,
    /// Video files.
    Videos,
    /// Audio files.
    Audio,
    /// Documents: text, office files, PDFs.
    Documents,
    /// Compressed archives and disk images.
    Archives,
    /// Programs, installers and scripts that run.
    Programs,
}

impl KindFilter {
    /// Every filter, in the order the bar shows them.
    pub const ALL: [KindFilter; 9] = [
        KindFilter::All,
        KindFilter::Folders,
        KindFilter::Files,
        KindFilter::Images,
        KindFilter::Videos,
        KindFilter::Audio,
        KindFilter::Documents,
        KindFilter::Archives,
        KindFilter::Programs,
    ];

    /// The chip's label.
    pub fn label(self) -> &'static str {
        self.menu_label()
    }

    /// The name the filter menu shows.
    pub fn menu_label(self) -> &'static str {
        match self {
            KindFilter::All => t!("Tudo"),
            KindFilter::Folders => t!("Pastas"),
            KindFilter::Files => t!("Arquivos"),
            KindFilter::Images => t!("Imagens"),
            KindFilter::Videos => t!("Vídeos"),
            KindFilter::Audio => t!("Áudio"),
            KindFilter::Documents => t!("Documentos"),
            KindFilter::Archives => t!("Compactados"),
            KindFilter::Programs => t!("Programas"),
        }
    }

    /// Whether `entry` passes this filter.
    pub fn matches(self, entry: &FileEntryDto) -> bool {
        let is_dir = entry.kind == "dir";
        let extension = std::path::Path::new(&entry.name)
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let extension = extension.as_str();
        match self {
            KindFilter::All => true,
            KindFilter::Folders => is_dir,
            KindFilter::Files => !is_dir,
            _ if is_dir => false,
            KindFilter::Images => matches!(
                extension,
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff"
                    | "heic" | "heif" | "avif" | "psd" | "raw" | "cr2" | "nef" | "dng"
            ),
            KindFilter::Videos => matches!(
                extension,
                "mp4" | "mov" | "mkv" | "avi" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg"
                    | "3gp" | "ts" | "mts" | "m2ts"
            ),
            KindFilter::Audio => matches!(
                extension,
                "mp3" | "wav" | "flac" | "aac" | "ogg" | "opus" | "m4a" | "wma" | "aiff" | "aif"
                    | "mid" | "midi"
            ),
            KindFilter::Documents => matches!(
                extension,
                "txt" | "md" | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "csv" | "ppt" | "pptx"
                    | "odt" | "ods" | "odp" | "rtf" | "json" | "xml" | "html" | "htm" | "log"
                    | "epub"
            ),
            KindFilter::Archives => matches!(
                extension,
                "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" | "cab"
            ),
            KindFilter::Programs => matches!(
                extension,
                "exe" | "msi" | "msix" | "appx" | "bat" | "cmd" | "ps1" | "lnk" | "com" | "scr"
            ),
        }
    }
}

#[cfg(test)]
fn name_matches(pattern: &str, name: &str) -> bool {
    NameMatcher::new(pattern).matches(name)
}

/// The filter bar's pattern, ready to test names against, ignoring case.
///
/// A pattern with `*` or `?` is a wildcard pattern against the whole name
/// (`*.png`, `IMG_????.jpg`); anything else matches as a substring, which is
/// what someone typing part of a name expects. A pattern of letters and
/// digits also matches the name's pinyin initials, so `wjj` finds 文件夹: see
/// [`super::pinyin`]. The pattern is prepared once per filtering, not once per
/// name: lowercasing and splitting it again for each of a folder's names was
/// most of what a key cost.
pub struct NameMatcher {
    /// The lowercase pattern, and its characters when it has wildcards;
    /// `None` when it is empty and everything matches.
    pattern: Option<(String, Option<Vec<char>>)>,
    /// Whether the pattern is letters and digits only, so a name made of Han
    /// characters can be found by typing its initials instead.
    pinyin: bool,
}

impl NameMatcher {
    pub fn new(pattern: &str) -> Self {
        let pattern = pattern.trim().to_lowercase();
        if pattern.is_empty() {
            return Self { pattern: None, pinyin: false };
        }
        let wildcards = pattern
            .contains(['*', '?'])
            .then(|| pattern.chars().collect());
        // Initials have no wildcards to match against; a pattern that spells
        // them out is the only one worth a second look.
        let pinyin = wildcards.is_none() && pattern.chars().all(|ch| ch.is_ascii_alphanumeric());
        Self {
            pattern: Some((pattern, wildcards)),
            pinyin,
        }
    }

    pub fn matches(&self, name: &str) -> bool {
        let Some((pattern, wildcards)) = &self.pattern else {
            return true;
        };
        let name = name.to_lowercase();
        match wildcards {
            None => {
                name.contains(pattern.as_str())
                    || (self.pinyin
                        && super::pinyin::initials(&name).is_some_and(|initials| initials.contains(pattern.as_str())))
            }
            Some(pattern) => wildcard(pattern, &name.chars().collect::<Vec<_>>()),
        }
    }
}

// Iterative wildcard matching with single-star backtracking: linear in
// practice, and no recursion for a long name to exhaust.
fn wildcard(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: &str) -> FileEntryDto {
        FileEntryDto {
            name: name.to_string(),
            path: format!("C:\\x\\{name}"),
            kind: kind.to_string(),
            size: 0,
            modified: 0,
        }
    }

    #[test]
    fn kinds_sort_entries_by_extension_and_folders_stay_out_of_file_kinds() {
        assert!(KindFilter::Images.matches(&entry("A.PNG", "file")));
        assert!(KindFilter::Videos.matches(&entry("clip.mp4", "file")));
        assert!(KindFilter::Programs.matches(&entry("setup.exe", "file")));
        assert!(!KindFilter::Images.matches(&entry("photos.png", "dir")));
        assert!(KindFilter::Folders.matches(&entry("photos.png", "dir")));
        assert!(!KindFilter::Files.matches(&entry("docs", "dir")));
        assert!(KindFilter::All.matches(&entry("docs", "dir")));
    }

    #[test]
    fn plain_text_is_a_substring_and_wildcards_match_the_whole_name() {
        assert!(name_matches("", "anything"));
        assert!(name_matches("REPORT", "q3-report.pdf"));
        assert!(name_matches("*.png", "Shot.PNG"));
        assert!(!name_matches("*.png", "shot.png.bak"));
        assert!(name_matches("img_????.jpg", "IMG_0042.jpg"));
        assert!(!name_matches("img_????.jpg", "IMG_42.jpg"));
        assert!(name_matches("*a*b*", "xxaYYbzz"));
        assert!(!name_matches("*a*b", "xxaYYbzz"));
    }

    // Windows' zh-CN collation is what supplies the initials; elsewhere the
    // pattern is matched against the name alone.
    #[cfg(windows)]
    #[test]
    fn han_names_are_found_by_their_pinyin_initials() {
        assert!(name_matches("wjj", "文件夹"));
        assert!(name_matches("WJJ", "文件夹"));
        // Part of the initials, as typing narrows down.
        assert!(name_matches("wj", "文件夹"));
        assert!(name_matches("bg", "工作报告.docx"));
        // Letters around the Han characters still have to line up.
        assert!(name_matches("xmabg", "项目A报告"));
        assert!(!name_matches("wjz", "文件.txt"), "only what the name is typed as counts");
        assert!(!name_matches("zzz", "文件夹"));
        // A name with no Han characters is matched as it always was.
        assert!(name_matches("report", "q3-report.pdf"));
        // Wildcards match the name itself, never initials.
        assert!(!name_matches("w*j*", "文件夹"));
    }
}
