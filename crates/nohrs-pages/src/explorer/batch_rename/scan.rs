use nohrs_services::fs::listing::FileEntryDto;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::UNIX_EPOCH,
};

#[derive(Clone, Default)]
pub(super) struct Progress {
    pub cancel: Arc<AtomicBool>,
    pub found: Arc<AtomicUsize>,
}

#[derive(Default)]
pub(super) struct ScanResult {
    pub entries: Vec<FileEntryDto>,
    pub roots: Vec<PathBuf>,
    pub issues: Vec<String>,
}

pub(super) fn scan(
    roots: &str,
    recursive: bool,
    formats: &str,
    progress: &Progress,
) -> Result<ScanResult, String> {
    let formats = Formats::parse(formats)?;
    let roots: Vec<_> = roots
        .split('|')
        .map(|root| PathBuf::from(root.trim().trim_matches('"')))
        .collect();
    if roots.iter().any(|root| !root.is_absolute()) {
        return Err("请输入完整目录路径；多个目录用 | 分隔".into());
    }
    let mut result = ScanResult::default();
    for root in roots {
        cancelled(progress)?;
        match validate_root(&root) {
            Ok(root) => result.roots.push(root),
            Err(error) => result.issues.push(format!("{}：{error}", root.display())),
        }
    }
    result
        .roots
        .sort_by_key(|root| (root.components().count(), path_key(root)));
    let mut normalized: Vec<PathBuf> = Vec::new();
    for root in result.roots.drain(..) {
        let covered = normalized.iter().any(|existing| {
            path_key(existing) == path_key(&root) || (recursive && root.starts_with(existing))
        });
        if !covered {
            normalized.push(root);
        }
    }
    result.roots = normalized;
    let mut visited = HashSet::new();
    let mut stack = result.roots.clone();
    while let Some(directory) = stack.pop() {
        cancelled(progress)?;
        if !visited.insert(path_key(&directory)) {
            continue;
        }
        scan_directory(
            &directory,
            (recursive, &formats),
            (&mut stack, &mut result),
            progress,
        )?;
    }
    cancelled(progress)?;
    result.entries.sort_by(|left, right| {
        let left_parent = Path::new(&left.path)
            .parent()
            .map(path_key)
            .unwrap_or_default();
        let right_parent = Path::new(&right.path)
            .parent()
            .map(path_key)
            .unwrap_or_default();
        natural_cmp(&left_parent, &right_parent)
            .then_with(|| natural_cmp(&left.name, &right.name))
            .then_with(|| natural_cmp(&left.path, &right.path))
    });
    result.roots.sort_by_key(|root| root.components().count());
    Ok(result)
}

fn validate_root(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if is_link(&metadata) {
        return Err("已跳过链接 / 重解析点".into());
    }
    if !metadata.is_dir() {
        return Err("路径不是目录".into());
    }
    fs::canonicalize(path).map_err(|error| error.to_string())
}

fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let left = left.to_lowercase();
    let right = right.to_lowercase();
    let mut left_index = 0;
    let mut right_index = 0;
    while left_index < left.len() && right_index < right.len() {
        let left_bytes = left.as_bytes();
        let right_bytes = right.as_bytes();
        let left_digit = left_bytes[left_index].is_ascii_digit();
        let right_digit = right_bytes[right_index].is_ascii_digit();
        if left_digit && right_digit {
            let left_end = digit_end(left_bytes, left_index);
            let right_end = digit_end(right_bytes, right_index);
            let left_run = &left[left_index..left_end];
            let right_run = &right[right_index..right_end];
            let left_significant = left_run.trim_start_matches('0');
            let right_significant = right_run.trim_start_matches('0');
            let order = left_significant
                .len()
                .cmp(&right_significant.len())
                .then_with(|| left_significant.cmp(right_significant))
                .then_with(|| left_run.len().cmp(&right_run.len()));
            if order != Ordering::Equal {
                return order;
            }
            left_index = left_end;
            right_index = right_end;
            continue;
        }
        let order = left_bytes[left_index].cmp(&right_bytes[right_index]);
        if order != Ordering::Equal {
            return order;
        }
        left_index += 1;
        right_index += 1;
    }
    left.len().cmp(&right.len())
}

fn digit_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map_or(bytes.len(), |offset| start + offset)
}
pub(super) fn path_key(path: &Path) -> String {
    let text = path_text(path);
    if cfg!(windows) {
        text.replace('/', "\\").to_lowercase()
    } else {
        text
    }
}

pub(super) fn path_text(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        if let Some(relative) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{relative}");
        }
        if let Some(relative) = text.strip_prefix(r"\\?\") {
            return relative.to_owned();
        }
    }
    text
}

fn cancelled(progress: &Progress) -> Result<(), String> {
    if progress.cancel.load(Ordering::Relaxed) {
        Err("已取消扫描，请重新扫描后再执行".into())
    } else {
        Ok(())
    }
}

fn scan_directory(
    directory: &Path,
    options: (bool, &Formats),
    output: (&mut Vec<PathBuf>, &mut ScanResult),
    progress: &Progress,
) -> Result<(), String> {
    let (stack, result) = output;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            result
                .issues
                .push(format!("{}：{error}", directory.display()));
            return Ok(());
        }
    };
    for entry in entries {
        cancelled(progress)?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result
                    .issues
                    .push(format!("{}：{error}", directory.display()));
                continue;
            }
        };
        let path = entry.path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if is_link(&metadata) => result
                .issues
                .push(format!("{}：已跳过链接 / 重解析点", path.display())),
            Ok(metadata) if metadata.is_dir() && options.0 => stack.push(path),
            Ok(metadata)
                if metadata.is_file()
                    && options.1.matches(&entry.file_name().to_string_lossy()) =>
            {
                result.entries.push(file_entry(&path, &metadata));
                progress.found.fetch_add(1, Ordering::Relaxed);
            }
            Ok(_) => {}
            Err(error) => result.issues.push(format!("{}：{error}", path.display())),
        }
    }
    Ok(())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn file_entry(path: &Path, metadata: &fs::Metadata) -> FileEntryDto {
    FileEntryDto {
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        path: path.to_string_lossy().into_owned(),
        kind: "file".into(),
        size: metadata.len(),
        modified: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs()),
    }
}

struct Formats(Vec<String>);
impl Formats {
    fn parse(text: &str) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Ok(Self(Vec::new()));
        }
        let mut formats = Vec::new();
        for part in text.split(';').map(str::trim) {
            if matches!(part, "*" | "*.*") {
                formats.push(String::new());
                continue;
            }
            let format = part
                .strip_prefix("*.")
                .or_else(|| part.strip_prefix('.'))
                .unwrap_or(part);
            if format.is_empty()
                || format.starts_with('.')
                || format.ends_with('.')
                || format
                    .chars()
                    .any(|c| !c.is_alphanumeric() && !"._-".contains(c))
            {
                return Err("格式无效，例如 png 或 *.png;*.jpg;*.webp".into());
            }
            formats.push(format!(".{}", format.to_lowercase()));
        }
        Ok(Self(formats))
    }

    fn matches(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        self.0.is_empty() || self.0.iter().any(|format| name.ends_with(format))
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
