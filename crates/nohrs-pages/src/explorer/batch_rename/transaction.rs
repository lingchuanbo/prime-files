use super::plan;
use nohrs_services::fs::ops;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(super) struct Undo {
    changes: Vec<(PathBuf, PathBuf)>,
    stamps: Vec<Stamp>,
}

impl Undo {
    pub(super) fn reverse_changes(&self) -> Vec<(PathBuf, PathBuf)> {
        self.changes
            .iter()
            .map(|(source, target)| (target.clone(), source.clone()))
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    directory: bool,
}

fn stamp(path: &Path) -> Result<Stamp, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    Ok(Stamp {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        directory: metadata.is_dir(),
    })
}

struct Staged {
    source: PathBuf,
    target: PathBuf,
    temporary: PathBuf,
    committed: bool,
}

pub(super) fn execute(changes: &[(PathBuf, PathBuf)]) -> Result<Undo, String> {
    execute_with(changes, move_one)
}

fn move_one(source: &Path, target: &Path) -> Result<(), String> {
    ops::move_path_no_replace(source, target)
        .map(|_| ())
        .map_err(|error| format!("{} → {}：{error}", source.display(), target.display()))
}

fn execute_with(
    changes: &[(PathBuf, PathBuf)],
    mut move_path: impl FnMut(&Path, &Path) -> Result<(), String>,
) -> Result<Undo, String> {
    let errors = plan::check(changes);
    if let Some(error) = errors.values().next() {
        return Err(error.clone());
    }
    let mut staged = Vec::with_capacity(changes.len());
    for (source, target) in changes {
        let temporary = temporary_path(source);
        if let Err(error) = move_path(source, &temporary) {
            return Err(rollback(&mut staged, error));
        }
        staged.push(Staged {
            source: source.clone(),
            target: target.clone(),
            temporary,
            committed: false,
        });
    }
    for index in 0..staged.len() {
        let item = &staged[index];
        if let Err(error) = move_path(&item.temporary, &item.target) {
            return Err(rollback(&mut staged, error));
        }
        staged[index].committed = true;
    }
    let stamps = changes
        .iter()
        .map(|(_, target)| stamp(target))
        .collect::<Result<Vec<_>, _>>();
    match stamps {
        Ok(stamps) => Ok(Undo {
            changes: changes.to_vec(),
            stamps,
        }),
        Err(error) => Err(rollback(&mut staged, error)),
    }
}

fn temporary_path(source: &Path) -> PathBuf {
    let sequence = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    source.with_file_name(format!(
        ".prime-rename-{}-{now}-{sequence}.tmp",
        std::process::id()
    ))
}

fn rollback(staged: &mut [Staged], mut error: String) -> String {
    // 先将已落地的文件退回各自的暂存名，避免互换名称时原名仍被占用。
    for item in staged.iter_mut().filter(|item| item.committed) {
        match move_one(&item.target, &item.temporary) {
            Ok(()) => item.committed = false,
            Err(failure) => error.push_str(&format!(
                "\n回退失败，文件保留在 {}：{failure}",
                item.target.display()
            )),
        }
    }
    for item in staged.iter().filter(|item| !item.committed) {
        if let Err(failure) = move_one(&item.temporary, &item.source) {
            error.push_str(&format!(
                "\n回退失败，文件保留在 {}：{failure}",
                item.temporary.display()
            ));
        }
    }
    error
}

pub(super) fn undo(record: &Undo) -> Result<(), String> {
    for ((_, target), expected) in record.changes.iter().zip(&record.stamps) {
        if &stamp(target)? != expected {
            return Err(format!(
                "{} 已发生变化，为保护文件已停止撤销",
                target.display()
            ));
        }
    }
    let reverse = record.reverse_changes();
    execute(&reverse).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_rename_commit_failure_restores_all_originals() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.txt");
        let b = directory.path().join("b.txt");
        fs::write(&a, "AAA").unwrap();
        fs::write(&b, "BBB").unwrap();
        let mut calls = 0;
        let result = execute_with(
            &[(a.clone(), b.clone()), (b.clone(), a.clone())],
            |source, target| {
                calls += 1;
                if calls == 4 {
                    Err("模拟第二个目标写入失败".into())
                } else {
                    move_one(source, target)
                }
            },
        );
        assert!(result.is_err());
        assert_eq!("AAA", fs::read_to_string(a).unwrap());
        assert_eq!("BBB", fs::read_to_string(b).unwrap());
        assert_eq!(2, fs::read_dir(directory.path()).unwrap().count());
    }
}
