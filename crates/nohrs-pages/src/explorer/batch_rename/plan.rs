use super::rules::Rules;
use nohrs_services::fs::listing::FileEntryDto;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub(super) struct Preview {
    pub entry: FileEntryDto,
    pub name: String,
    pub error: Option<String>,
    pub selected: bool,
}

impl Preview {
    pub(super) fn changed(&self) -> bool {
        self.entry.name != self.name
    }
    pub(super) fn change(&self) -> (PathBuf, PathBuf) {
        let source = PathBuf::from(&self.entry.path);
        let target = source.with_file_name(&self.name);
        (source, target)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ListApply {
    Name,
    Extension,
    #[default]
    FullName,
}

/// One row per file of the list: line `i` names file `i`, and a blank or
/// missing line leaves that file's name alone.
pub(super) fn preview_list(
    entries: &[FileEntryDto],
    lines: &[String],
    apply: ListApply,
    excluded: &HashSet<String>,
) -> Vec<Preview> {
    let mut rows = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let selected = !excluded.contains(&entry.path);
        if !selected {
            rows.push(Preview {
                entry: entry.clone(),
                name: entry.name.clone(),
                error: None,
                selected: false,
            });
            continue;
        }
        // A blank line, or no line left, keeps the entry as it is: a list half
        // typed is not an error, and the preview greys the name it keeps
        // instead of reddening every file that has no name yet.
        let Some(line) = lines.get(index).filter(|line| !line.trim().is_empty()) else {
            rows.push(Preview {
                entry: entry.clone(),
                name: entry.name.clone(),
                error: None,
                selected,
            });
            continue;
        };
        let target = match apply {
            ListApply::Name if entry.kind == "dir" => line.clone(),
            ListApply::Name => match Path::new(&entry.name)
                .extension()
                .and_then(|value| value.to_str())
            {
                Some(extension) => format!("{line}.{extension}"),
                None => line.clone(),
            },
            ListApply::Extension if entry.kind == "dir" => entry.name.clone(),
            ListApply::Extension => {
                let stem = Path::new(&entry.name)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or(&entry.name);
                let extension = line.trim_start_matches('.');
                if extension.is_empty() {
                    stem.to_owned()
                } else {
                    format!("{stem}.{extension}")
                }
            }
            ListApply::FullName => line.clone(),
        };
        let (name, error) = match validate_name(&target) {
            Ok(()) => (target, None),
            Err(error) => (String::new(), Some(error)),
        };
        rows.push(Preview {
            entry: entry.clone(),
            name,
            error,
            selected,
        });
    }
    let changes: Vec<_> = rows
        .iter()
        .filter(|row| row.error.is_none() && row.changed())
        .map(Preview::change)
        .collect();
    let errors = check(&changes);
    for row in &mut rows {
        if row.error.is_none() && row.changed() {
            row.error = errors.get(&key(Path::new(&row.entry.path))).cloned();
        }
    }
    rows
}
#[cfg(test)]
pub(super) fn preview(entries: &[FileEntryDto], rules: &Rules) -> Vec<Preview> {
    preview_selected(entries, rules, &HashSet::new(), false)
}

pub(super) fn preview_selected(
    entries: &[FileEntryDto],
    rules: &Rules,
    excluded: &HashSet<String>,
    restart: bool,
) -> Vec<Preview> {
    let mut counters = HashMap::new();
    let mut sequence = 0;
    let mut rows: Vec<_> = entries
        .iter()
        .map(|entry| {
            if excluded.contains(&entry.path) {
                return Preview {
                    entry: entry.clone(),
                    name: entry.name.clone(),
                    error: None,
                    selected: false,
                };
            }
            let count = if restart {
                counters.entry(Path::new(&entry.path).parent()).or_insert(0)
            } else {
                &mut sequence
            };
            let index = *count;
            *count += 1;
            match rules.filename(entry, index).and_then(|name| {
                validate_name(&name)?;
                Ok(name)
            }) {
                Ok(name) => Preview {
                    entry: entry.clone(),
                    name,
                    error: None,
                    selected: true,
                },
                Err(error) => Preview {
                    entry: entry.clone(),
                    name: String::new(),
                    error: Some(error),
                    selected: true,
                },
            }
        })
        .collect();
    let changes: Vec<_> = rows
        .iter()
        .filter(|row| row.error.is_none() && row.changed())
        .map(Preview::change)
        .collect();
    let errors = check(&changes);
    for row in &mut rows {
        if row.error.is_none() && row.changed() {
            row.error = errors.get(&key(Path::new(&row.entry.path))).cloned();
        }
    }
    rows
}

pub(super) fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err("名称不能为空或为 . / ..".into());
    }
    if name.ends_with([' ', '.']) {
        return Err("名称不能以空格或句点结尾".into());
    }
    if name.encode_utf16().count() > 255 {
        return Err("名称超过 255 个字符".into());
    }
    if name
        .chars()
        .any(|character| character.is_control() || "<>:\"/\\|?*".contains(character))
    {
        return Err("名称包含 Windows 不允许的字符".into());
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    let numbered = base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"));
    if matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered.is_some_and(|suffix| {
        ["1", "2", "3", "4", "5", "6", "7", "8", "9", "¹", "²", "³"].contains(&suffix)
    }) {
        return Err("名称属于 Windows 保留设备名".into());
    }
    Ok(())
}

pub(super) fn key(path: &Path) -> String {
    let parent = path.parent().unwrap_or(Path::new(""));
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let path = parent
        .join(path.file_name().unwrap_or_default())
        .to_string_lossy()
        .into_owned();
    if cfg!(windows) {
        path.replace('/', "\\").to_lowercase()
    } else {
        path
    }
}

pub(super) fn check(changes: &[(PathBuf, PathBuf)]) -> HashMap<String, String> {
    let mut errors = HashMap::new();
    let sources: HashMap<_, _> = changes
        .iter()
        .map(|(source, _)| (key(source), source))
        .collect();
    let mut targets: HashMap<String, Vec<&PathBuf>> = HashMap::new();
    for (source, target) in changes {
        let source_key = key(source);
        targets.entry(key(target)).or_default().push(source);
        if source.parent() != target.parent() {
            errors.insert(source_key.clone(), "仅支持在原目录内重命名".into());
        }
        if let Err(error) = std::fs::symlink_metadata(source) {
            errors.insert(source_key.clone(), format!("无法读取原文件：{error}"));
        }
        if let Some(name) = target.file_name().and_then(|name| name.to_str()) {
            if let Err(error) = validate_name(name) {
                errors.insert(source_key.clone(), error);
            }
        } else {
            errors.insert(source_key.clone(), "目标名称无效".into());
        }
        if !sources.contains_key(&key(target)) {
            match std::fs::symlink_metadata(target) {
                Ok(_) => {
                    errors.insert(source_key.clone(), "目标名称已存在".into());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    errors.insert(source_key.clone(), format!("无法检查目标：{error}"));
                }
            }
        }
        if source
            .ancestors()
            .skip(1)
            .any(|parent| sources.contains_key(&key(parent)))
        {
            errors.insert(
                source_key,
                "不能同时重命名文件夹及其内部项目，请分开操作".into(),
            );
        }
    }
    for group in targets.values().filter(|group| group.len() > 1) {
        for source in group {
            errors.insert(key(source), "多个项目生成相同的目标名称".into());
        }
    }
    if sources.len() != changes.len() {
        for (source, _) in changes {
            errors.insert(key(source), "选中的项目包含重复路径".into());
        }
    }
    errors
}
