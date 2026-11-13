// SPDX-License-Identifier: AGPL-3.0-or-later
//! A native identity may use only one existing project location for script writes.
//! Records are separate from the version database, so existing script rows never
//! need a schema rewrite. The permanent kernel-lock file is never replaced.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DIRECTORY: &str = "project-locations";
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Location {
    format: u32,
    project_id: String,
    path: PathBuf,
}

/// Run local script work only after checking the native manifest at the current
/// and previously registered locations. `read_identity` must inspect native
/// files; it must not obtain the identity from chat, the model, or path text.
///
/// A moved project keeps its identity. A copy while the old identity still
/// exists elsewhere is refused. Reusing the old folder for a *different*
/// native identity does not prevent the original project from moving.
pub fn with_project_location<T>(
    root: &Path,
    project_id: &str,
    path: &Path,
    read_identity: impl Fn(&Path) -> Result<Option<String>, String>,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if project_id.is_empty() || project_id.len() > 4096 {
        return Err("工程标识无效，文案操作已停止。".into());
    }
    let current = path
        .canonicalize()
        .map_err(|_| "当前工程位置无法读取，文案操作已停止。".to_owned())?;
    if read_identity(&current)?.as_deref() != Some(project_id) {
        return Err("当前工程标识尚未保存或已改变，文案操作已停止，请重新打开工程。".into());
    }
    regular_directory(root)?;
    let directory = root.join(DIRECTORY);
    fs::create_dir_all(&directory).map_err(io_error)?;
    regular_directory(&directory)?;
    let key = format!("{:x}", Sha256::digest(project_id.as_bytes()));
    let lock_path = directory.join(format!("{key}.lock"));
    regular_file_or_absent(&lock_path)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(io_error)?;
    regular_file_or_absent(&lock_path)?;
    super::try_lock_owner_file(&lock)
        .map_err(|_| "工程文案归属正在被另一操作核对，请稍后重试。".to_owned())?;
    let lock = LocationLock { file: Some(lock) };
    let record_path = directory.join(format!("{key}.json"));
    regular_file_or_absent(&record_path)?;
    let previous = match fs::read(&record_path) {
        Ok(bytes) if bytes.len() <= 32 * 1024 => {
            let record: Location = serde_json::from_slice(&bytes)
                .map_err(|_| "工程文案位置登记已损坏，原记录保持不变。".to_owned())?;
            if record.format != 1 || record.project_id != project_id || !record.path.is_absolute() {
                return Err("工程文案位置登记不兼容，原记录保持不变。".into());
            }
            Some(record)
        }
        Ok(_) => return Err("工程文案位置登记超出限制，原记录保持不变。".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(error)),
    };
    if let Some(previous) = &previous
        && previous.path != current
        && read_identity(&previous.path)?.as_deref() == Some(project_id)
    {
        return Err(format!(
            "检测到工程副本，原工程仍位于 {}。为防止两份工程共用文案，本副本的文案操作已停止；原生剪辑仍可使用。请打开原工程继续文案工作。",
            previous.path.display()
        ));
    }
    // Read again while holding the identity's lock, before any version action.
    if read_identity(&current)?.as_deref() != Some(project_id) {
        return Err("核对期间工程标识已改变，文案操作已停止。".into());
    }
    if previous
        .as_ref()
        .is_none_or(|record| record.path != current)
    {
        write_record(
            &record_path,
            &Location {
                format: 1,
                project_id: project_id.into(),
                path: current,
            },
        )?;
    }
    lock.finish(action())
}

// A concurrent spawn can temporarily inherit an open file description. Every
// exit explicitly unlocks before closing, including validation errors/unwind.
struct LocationLock {
    file: Option<File>,
}

impl LocationLock {
    fn finish<T>(mut self, result: Result<T, String>) -> Result<T, String> {
        let Some(file) = self.file.as_ref() else {
            return result;
        };
        match super::unlock_owner_file(file) {
            Ok(()) => {
                drop(self.file.take());
                result
            }
            Err(error) => match result {
                Ok(_) => Err(format!("文案位置登记未能完成解锁，请重新核对：{error}")),
                Err(original) => {
                    // This core crate has no logging facade. Keep the action's
                    // error authoritative and expose cleanup failure on stderr.
                    eprintln!(
                        "native script location unlock failed after an operation error: {error}"
                    );
                    Err(original)
                }
            },
        }
    }
}

impl Drop for LocationLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.as_ref()
            && let Err(error) = super::unlock_owner_file(file)
        {
            eprintln!("native script location cleanup could not unlock: {error}");
        }
    }
}

fn io_error(error: std::io::Error) -> String {
    format!("无法保存工程文案位置登记，文案操作已停止：{error}")
}

fn regular_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir() || redirected(&metadata) {
        return Err("文案位置登记目录不是可核对的普通目录。".into());
    }
    Ok(())
}

fn regular_file_or_absent(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !redirected(&metadata) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err("文案位置登记不是可核对的普通文件，原文件保持不变。".into()),
    }
}

fn redirected(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn write_record(path: &Path, record: &Location) -> Result<(), String> {
    let directory = path.parent().ok_or("文案位置登记缺少父目录。")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "当前时间无法用于保存位置登记。")?
        .as_nanos();
    let temporary = Temporary(directory.join(format!(
        ".location-{}-{stamp}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary.0)
        .map_err(io_error)?;
    let encoded = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    file.write_all(&encoded).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    drop(file);
    regular_file_or_absent(path)?;
    fs::rename(&temporary.0, path).map_err(io_error)?;
    #[cfg(unix)]
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_release_is_not_delayed_by_another_descriptor_for_the_same_file() {
        for finish in [true, false] {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bk-cat-location-unlock-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let cleanup = Temporary(path.clone());
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            super::super::try_lock_owner_file(&file).unwrap();
            let inherited = file.try_clone().unwrap();
            let guard = LocationLock { file: Some(file) };
            if finish {
                assert_eq!(
                    guard.finish(Err::<(), _>("original document failure".into())),
                    Err("original document failure".into())
                );
            } else {
                // The guard's early-return/unwind path must release as well.
                drop(guard);
            }
            assert!(
                inherited.metadata().is_ok(),
                "the duplicate descriptor is still open"
            );
            let next = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            super::super::try_lock_owner_file(&next).expect(
                "explicit unlock permits the next operation while the duplicate stays open",
            );
            super::super::unlock_owner_file(&next).unwrap();
            drop(next);
            drop(inherited);
            drop(cleanup);
        }
    }
}
