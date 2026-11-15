use super::rules::Rules;
use nohrs_services::fs::listing::FileEntryDto;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub(super) struct Preview {
    pub entry: FileEntryDto,
    pub name: String,
    pub error: Option<String>,
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

pub(super) fn preview(entries: &[FileEntryDto], rules: &Rules) -> Vec<Preview> {
    let mut rows: Vec<_> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            match rules.filename(entry, index).and_then(|name| {
                validate_name(&name)?;
                Ok(name)
            }) {
                Ok(name) => Preview {
                    entry: entry.clone(),
                    name,
                    error: None,
                },
                Err(error) => Preview {
                    entry: entry.clone(),
                    name: String::new(),
                    error: Some(error),
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
