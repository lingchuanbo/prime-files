use nohrs_services::fs::listing::FileEntryDto;
use std::path::Path;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum CaseMode {
    #[default]
    Keep,
    Lower,
    Upper,
    Title,
}

impl CaseMode {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Keep => "保持不变",
            Self::Lower => "全部小写",
            Self::Upper => "全部大写",
            Self::Title => "单词首字母大写",
        }
    }

    fn apply(self, text: String) -> String {
        match self {
            Self::Keep => text,
            Self::Lower => text.to_lowercase(),
            Self::Upper => text.to_uppercase(),
            Self::Title => {
                let mut start = true;
                text.chars()
                    .flat_map(|character| {
                        let result = if start {
                            character.to_uppercase().to_string()
                        } else {
                            character.to_lowercase().to_string()
                        };
                        start = !character.is_alphanumeric();
                        result.chars().collect::<Vec<_>>()
                    })
                    .collect()
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct Rules {
    pub name: String,
    pub extension: String,
    pub find: String,
    pub replacement: String,
    pub start: u64,
    pub step: u64,
    pub digits: usize,
    pub case: CaseMode,
    pub first_only: bool,
    pub include_extension: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            name: "[N]".into(),
            extension: "[E]".into(),
            find: String::new(),
            replacement: String::new(),
            start: 1,
            step: 1,
            digits: 3,
            case: CaseMode::Keep,
            first_only: false,
            include_extension: false,
        }
    }
}

impl Rules {
    pub(super) fn filename(&self, entry: &FileEntryDto, index: usize) -> Result<String, String> {
        if !(1..=18).contains(&self.digits) {
            return Err("计数器位数应为 1–18".into());
        }
        let mut name = self.expand(&self.name, entry, index)?;
        let mut extension = if entry.kind == "dir" {
            String::new()
        } else {
            self.expand(&self.extension, entry, index)?
        };
        if !self.find.is_empty() {
            name = self.replace(&name);
            if self.include_extension {
                extension = self.replace(&extension);
            }
        }
        let filename = if extension.is_empty() {
            name
        } else {
            format!("{name}.{extension}")
        };
        Ok(self.case.apply(filename))
    }

    fn replace(&self, text: &str) -> String {
        if self.first_only {
            text.replacen(&self.find, &self.replacement, 1)
        } else {
            text.replace(&self.find, &self.replacement)
        }
    }

    fn expand(&self, mask: &str, entry: &FileEntryDto, index: usize) -> Result<String, String> {
        let mut remaining = mask;
        let mut output = String::new();
        while let Some(open) = remaining.find('[') {
            output.push_str(&remaining[..open]);
            remaining = &remaining[open + 1..];
            let close = remaining.find(']').ok_or("占位符缺少右方括号 ]")?;
            output.push_str(&self.token(&remaining[..close], entry, index)?);
            remaining = &remaining[close + 1..];
        }
        output.push_str(remaining);
        Ok(output)
    }

    fn token(&self, token: &str, entry: &FileEntryDto, index: usize) -> Result<String, String> {
        let path = Path::new(&entry.path);
        let filename = Path::new(&entry.name);
        let stem = if entry.kind == "dir" {
            entry.name.as_str()
        } else {
            filename
                .file_stem()
                .and_then(|part| part.to_str())
                .unwrap_or(&entry.name)
        };
        let extension = if entry.kind == "dir" {
            ""
        } else {
            filename
                .extension()
                .and_then(|part| part.to_str())
                .unwrap_or("")
        };
        match token {
            "N" => Ok(stem.into()),
            "E" => Ok(extension.into()),
            "A" => Ok(entry.name.clone()),
            "C" => self
                .start
                .checked_add(
                    self.step
                        .checked_mul(index as u64)
                        .ok_or("计数器超出范围")?,
                )
                .map(|value| format!("{value:0width$}", width = self.digits))
                .ok_or_else(|| "计数器超出范围".into()),
            "YMD" | "hms" => date_token(entry.modified, token),
            _ if token.starts_with('B') => ancestor(path, &token[1..]),
            _ if token.starts_with('N') => slice_token(stem, &token[1..]),
            _ if token.starts_with('E') => slice_token(extension, &token[1..]),
            _ => Err(format!("未知占位符 [{token}]")),
        }
    }
}

fn ancestor(path: &Path, depth: &str) -> Result<String, String> {
    let depth: usize = if depth.is_empty() {
        0
    } else {
        depth
            .parse()
            .map_err(|_| "目录占位符格式应为 [B]、[B1]、[B2]")?
    };
    path.ancestors()
        .skip(1)
        .nth(depth)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "所需的上级目录不存在".into())
}

fn slice_token(text: &str, range: &str) -> Result<String, String> {
    let (start, end) = range
        .split_once('-')
        .ok_or("范围格式应为 [N1-3] 或 [N3-]")?;
    let start: usize = start.parse().map_err(|_| "范围起点应为正整数")?;
    let end: usize = if end.is_empty() {
        text.chars().count()
    } else {
        end.parse().map_err(|_| "范围终点应为正整数")?
    };
    if start == 0 || end < start {
        return Err("字符范围无效（从 1 开始）".into());
    }
    Ok(text.chars().skip(start - 1).take(end - start + 1).collect())
}

fn date_token(seconds: u64, token: &str) -> Result<String, String> {
    let seconds = i64::try_from(seconds).map_err(|_| "文件时间超出范围")?;
    let date = time::OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| "文件时间无效")?;
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let date = date.to_offset(offset);
    Ok(if token == "YMD" {
        format!(
            "{:04}{:02}{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        )
    } else {
        format!("{:02}{:02}{:02}", date.hour(), date.minute(), date.second())
    })
}
