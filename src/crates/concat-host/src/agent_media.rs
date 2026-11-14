// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 BK CAT contributors

//! Real-file evidence for exact native agent additions. Paths come only from
//! the current project's imported bin, never from a model-supplied pathname.
//! Probe off the UI thread and outside the owner gate. The resulting proof is
//! native-only, kept alive until the final current-session/revision/lock gate.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use concat_project::Command;
use concat_project::commands::ExactMediaKind;
use concat_project::guard::{MAX_BATCH_DEPTH, MAX_BATCH_OPERATIONS};
use concat_project::model::{MediaItem, Project};

/// A real probe and live file handle for every source in this native plan.
/// It is intentionally neither cloneable nor serializable and confers no
/// editing permission: the caller still checks the actual native session.
#[derive(Default)]
pub struct ExactMediaChecks {
    files: Vec<CheckedFile>,
}

struct Request {
    item: MediaItem,
    kind: ExactMediaKind,
    source_end: f64,
}

struct CheckedFile {
    item: MediaItem,
    canonical: PathBuf,
    file: File,
    stamp: Stamp,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    identity: FileIdentity,
    size: u64,
    modified: SystemTime,
}

#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(windows)]
type FileIdentity = (u64, [u8; 16]);
#[cfg(not(any(unix, windows)))]
type FileIdentity = ();

impl ExactMediaChecks {
    /// Probe source files of exact additions in a captured native project.
    /// `check_active` checks cancellation before and after each potentially
    /// slow probe; it must release its gate before returning. No part of this
    /// method writes the project, imports media, or creates a private timeline.
    pub fn probe(
        project: &Project,
        command: &Command,
        check_active: impl Fn() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut requests = BTreeMap::new();
        let mut remaining = MAX_BATCH_OPERATIONS;
        collect(project, command, 0, &mut remaining, &mut requests)?;
        let mut files = Vec::with_capacity(requests.len());
        for request in requests.into_values() {
            check_active()?;
            let file = CheckedFile::open(request.item)?;
            let probed = crate::media::probe(&file.item.path)
                .map_err(|_| "无法读取实际素材，精确剪辑已停止。".to_owned())?;
            check_active()?;
            file.recheck_file()?;
            if probed.kind != request.kind.media_kind()
                || (request.kind == ExactMediaKind::Video && probed.video.is_none())
                || (request.kind == ExactMediaKind::Audio && probed.audio.is_none())
            {
                return Err("实际素材类型与剪辑计划不一致，请重新导入后生成计划。".into());
            }
            let duration = probed
                .duration
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .ok_or_else(|| "实际素材时长无法确认，未使用默认时长执行剪辑。".to_owned())?;
            if request.source_end > duration {
                return Err("剪辑计划的源结束时间超出实际素材时长，未修改工程。".into());
            }
            let imported_duration = file
                .item
                .duration
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .ok_or_else(|| "已导入素材没有有效时长，请重新导入。".to_owned())?;
            // Native import and this probe use the same function. A changed
            // duration is a changed source, not permission to rewrite bin data.
            if (duration - imported_duration).abs() > 1e-6 {
                return Err("素材时长已变化，请重新导入并重新生成剪辑计划。".into());
            }
            files.push(file);
        }
        Ok(Self { files })
    }

    /// Recheck the captured imported items and actual file objects in the
    /// final native callback. This performs metadata/identity checks only;
    /// decoding and probing never run while the native owner gate is held.
    pub fn recheck(&self, project: &Project) -> Result<(), String> {
        for file in &self.files {
            let mut current = project.media.iter().filter(|item| item.id == file.item.id);
            if current.next() != Some(&file.item) || current.next().is_some() {
                return Err("计划引用的素材已变更，请读取当前工程后重新生成计划。".into());
            }
            file.recheck_file()?;
        }
        Ok(())
    }
}

fn collect(
    project: &Project,
    command: &Command,
    depth: usize,
    remaining: &mut usize,
    requests: &mut BTreeMap<String, Request>,
) -> Result<(), String> {
    if depth > MAX_BATCH_DEPTH || *remaining == 0 {
        return Err("剪辑计划超过允许的嵌套或操作数量，未读取素材。".into());
    }
    *remaining -= 1;
    match command {
        Command::Batch { commands } => {
            for command in commands {
                collect(project, command, depth + 1, remaining, requests)?;
            }
        }
        Command::AddMediaClipExact {
            media_id,
            kind,
            start,
            source_start,
            source_end,
            ..
        } => {
            use concat_project::model::ranges::{MAX_TIME, MIN_CLIP_DURATION};
            if ![start, source_start, source_end]
                .iter()
                .all(|time| time.is_finite())
                || *start < 0.0
                || *source_start < 0.0
                || source_end - source_start < MIN_CLIP_DURATION
                || *source_end > MAX_TIME
                || start + source_end - source_start > MAX_TIME
            {
                return Err("精确剪辑时间范围无效，未读取素材。".into());
            }
            let mut candidates = project.media.iter().filter(|item| item.id == *media_id);
            let item = candidates
                .next()
                .ok_or_else(|| "计划引用的素材已不存在。".to_owned())?;
            if candidates.next().is_some() || item.placeholder || item.kind != kind.media_kind() {
                return Err("计划引用的素材身份或类型不一致，未读取文件。".into());
            }
            if item
                .duration
                .is_none_or(|duration| !duration.is_finite() || *source_end > duration)
            {
                return Err("计划超过已导入素材时长，请重新读取素材。".into());
            }
            if let Some(request) = requests.get_mut(media_id) {
                request.source_end = request.source_end.max(*source_end);
            } else {
                requests.insert(
                    media_id.clone(),
                    Request {
                        item: item.clone(),
                        kind: *kind,
                        source_end: *source_end,
                    },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

impl CheckedFile {
    fn open(item: MediaItem) -> Result<Self, String> {
        let path = Path::new(&item.path);
        if !path.is_absolute() {
            return Err("精确剪辑只使用已导入的本地绝对路径素材。".into());
        }
        let canonical = std::fs::canonicalize(path)
            .map_err(|_| "素材文件已缺失或不可读，请重新定位素材。".to_owned())?;
        let file =
            File::open(&canonical).map_err(|_| "素材文件不可读，精确剪辑已停止。".to_owned())?;
        let stamp = Stamp::read(&file)?;
        let checked = Self {
            item,
            canonical,
            file,
            stamp,
        };
        checked.recheck_file()?;
        Ok(checked)
    }

    fn recheck_file(&self) -> Result<(), String> {
        let changed = || "素材文件在规划或检查期间被替换或修改，未提交剪辑。".to_owned();
        let canonical = std::fs::canonicalize(&self.item.path).map_err(|_| changed())?;
        if canonical != self.canonical || Stamp::read(&self.file)? != self.stamp {
            return Err(changed());
        }
        let named = File::open(&canonical).map_err(|_| changed())?;
        if Stamp::read(&named)? != self.stamp {
            return Err(changed());
        }
        Ok(())
    }
}

impl Stamp {
    fn read(file: &File) -> Result<Self, String> {
        let metadata = file
            .metadata()
            .map_err(|_| "无法核对素材文件属性。".to_owned())?;
        if !metadata.is_file() {
            return Err("精确剪辑只接受普通素材文件。".into());
        }
        Ok(Self {
            identity: identity(file).map_err(|_| "无法核对素材文件对象。".to_owned())?,
            size: metadata.len(),
            modified: metadata
                .modified()
                .map_err(|_| "无法核对素材修改时间。".to_owned())?,
        })
    }
}

#[cfg(unix)]
fn identity(file: &File) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn identity(file: &File) -> std::io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the live File owns this handle. info is aligned writable storage
    // with the exact FILE_ID_INFO size selected by FileIdInfo; no pointer escapes.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

#[cfg(not(any(unix, windows)))]
fn identity(_file: &File) -> std::io::Result<FileIdentity> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台不能核对素材对象",
    ))
}

#[cfg(test)]
mod tests;
