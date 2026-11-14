// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native save authority: a persistent identity, or one exact first binding.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// A native session's save authority. Its fields and constructor are private
/// to the host: it is neither document metadata nor a model-supplied option.
/// Clones share the first-binding state, which is consumed permanently once
/// this identity is observed on disk or a write successfully persists it.
#[derive(Clone)]
pub struct SaveAuthority(Arc<Authority>);

struct Authority {
    identity: String,
    key: PathBuf,
    initial: Mutex<Option<Initial>>,
}

struct Initial {
    directory: Option<Directory>,
    document: Option<[u8; 32]>,
}

/// The very document read for `Session::open`, plus its actual directory.
/// No later read can silently become proof for a previously opened session.
pub(crate) struct Source {
    key: PathBuf,
    directory: Option<Directory>,
    document: Option<[u8; 32]>,
}

impl SaveAuthority {
    pub(crate) fn opened(identity: String, source: Source, already_identified: bool) -> Self {
        Self(Arc::new(Authority {
            identity,
            key: source.key,
            initial: Mutex::new((!already_identified).then_some(Initial {
                directory: source.directory,
                document: source.document,
            })),
        }))
    }

    fn initial(&self) -> std::sync::MutexGuard<'_, Option<Initial>> {
        self.0
            .initial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn check_target(&self, key: &Path, identity: Option<&str>) -> Result<(), String> {
        if key != self.0.key || identity != Some(self.0.identity.as_str()) {
            return Err("保存快照与原生会话的工程归属不一致，未写入工程。".into());
        }
        Ok(())
    }

    /// Called under the ordinary per-project write lock. A session which
    /// originally saw no folder may create only the final directory itself;
    /// it never adopts a directory which appeared in the meantime.
    fn create_missing_directory(&self, root: &Path) -> Result<(), String> {
        let mut initial = self.initial();
        let Some(initial) = initial.as_mut() else {
            return Err("原工程目录已不存在，保存已停止；请先恢复或重新打开工程。".into());
        };
        if initial.directory.is_some() || initial.document.is_some() {
            return Err("首次保存前原工程目录已被移走，未创建替代工程。".into());
        }
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("无法准备工程父目录：{error}"))?;
        }
        std::fs::create_dir(root)
            .map_err(|error| format!("无法独占创建首次保存的工程目录：{error}"))?;
        // Retain the newly created object even if a later write fails. A retry
        // may use this directory, but cannot adopt its replacement.
        initial.directory = Some(Directory::open(root)?);
        Ok(())
    }

    fn allows_first_binding(&self, root: &Path, document: Option<&Value>) -> Result<(), String> {
        let initial = self.initial();
        let Some(initial) = initial.as_ref() else {
            return Err("工程持久标识已丢失或被移除，保存已停止，现有文件保持不变。".into());
        };
        let Some(directory) = &initial.directory else {
            return Err("首次保存的位置已出现另一目录，未认领其中的工程。".into());
        };
        if !directory.matches(root)? || initial.document != fingerprint(document)? {
            return Err("首次保存前工程内容或目录已被替换，现有文件保持不变。".into());
        }
        Ok(())
    }

    pub(super) fn persisted(&self) {
        *self.initial() = None;
    }
}

/// Resolve an existing path, or resolve the nearest existing parent and keep
/// the missing tail. This is the same write-lock key before and after a first
/// save creates its directory; the key itself is never ownership evidence.
pub(super) fn target_key(root: &Path) -> Result<PathBuf, String> {
    let mut at = root;
    let mut tail = Vec::new();
    loop {
        let probe = if at.as_os_str().is_empty() {
            Path::new(".")
        } else {
            at
        };
        match std::fs::canonicalize(probe) {
            Ok(mut key) => {
                for name in tail.iter().rev() {
                    key.push(name);
                }
                return Ok(key);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = at
                    .file_name()
                    .ok_or_else(|| format!("无法解析工程保存位置 {}", root.display()))?;
                tail.push(name.to_os_string());
                at = at.parent().ok_or("工程保存位置没有可核对的父目录")?;
            }
            Err(error) => return Err(format!("无法核对工程保存位置：{error}")),
        }
    }
}

pub(crate) fn read_for_session(path: &str) -> Result<(Option<Value>, Source), String> {
    let key = target_key(Path::new(path))?;
    // Capture before reading, but require object identity only for a first
    // binding. An identified project does not gain a new filesystem support
    // requirement merely because the initial-binding check needs this API.
    let directory = match std::fs::symlink_metadata(&key) {
        Ok(_) => Directory::open(&key).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("无法读取工程目录：{error}")),
    };
    let (_, document) = read_manifest(&key)?;
    if target_key(Path::new(path))? != key {
        return Err("读取期间工程目录已被替换，请重新打开工程。".into());
    }
    let identified = document
        .as_ref()
        .map(crate::project_identity::from_document)
        .transpose()?
        .flatten()
        .is_some();
    if identified {
        return Ok((
            document,
            Source {
                key,
                directory: None,
                document: None,
            },
        ));
    }
    let directory = directory?;
    let unchanged = match &directory {
        Some(directory) => directory.matches(&key)?,
        None => {
            matches!(std::fs::symlink_metadata(&key), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        }
    };
    if !unchanged {
        return Err("读取期间工程目录已被替换，请重新打开工程。".into());
    }
    let source = Source {
        key,
        directory,
        document: fingerprint(document.as_ref())?,
    };
    Ok((document, source))
}

/// Validate the actual destination while the caller holds the same mutex used
/// for its temporary write and rename. Return that exact manifest path, so the
/// commit cannot choose a different legacy/current name after the check.
pub(super) fn validate(
    root: &Path,
    document: &Value,
    authority: Option<&SaveAuthority>,
) -> Result<PathBuf, String> {
    let incoming = crate::project_identity::from_document(document)?;
    if let Some(authority) = authority {
        authority.check_target(root, incoming.as_deref())?;
    }
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !crate::project_identity::redirected(&metadata) => {}
        Ok(_) => return Err(format!("工程保存位置不是可核对的目录：{}", root.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match authority {
            Some(authority) => authority.create_missing_directory(root)?,
            None if incoming.is_none() => std::fs::create_dir_all(root)
                .map_err(|error| format!("无法创建工程目录：{error}"))?,
            None => return Err("工程目录已不存在，通用保存不能重新绑定工程身份。".into()),
        },
        Err(error) => return Err(format!("无法核对工程目录：{error}")),
    }
    let (manifest, current) = read_manifest(root)?;
    let existing = current
        .as_ref()
        .map(crate::project_identity::from_document)
        .transpose()?
        .flatten();
    match (incoming.as_deref(), existing.as_deref()) {
        (Some(incoming), Some(existing)) if incoming == existing => {
            if let Some(authority) = authority {
                // Seeing this identity persisted is enough to retire the
                // permit, even for a superseded or subsequently failed write.
                authority.persisted();
            }
        }
        (Some(_), Some(_)) => return Err("工程位置已属于另一份工程，原工程快照未写入。".into()),
        (None, Some(_)) => return Err("保存快照缺少持久工程标识，未覆盖已有工程。".into()),
        (Some(_), None) => authority
            .ok_or("磁盘工程没有对应持久标识，首次绑定需要原生会话的读取凭据。")?
            .allows_first_binding(root, current.as_ref())?,
        (None, None) => {}
    }
    if let Some(current) = &current {
        require_readable(current, &manifest)?;
    }
    require_readable(document, &manifest)?;
    Ok(manifest)
}

fn fingerprint(document: Option<&Value>) -> Result<Option<[u8; 32]>, String> {
    document
        .map(|document| {
            serde_json::to_vec(document)
                .map(|encoded| Sha256::digest(encoded).into())
                .map_err(|error| format!("无法核对原始工程内容：{error}"))
        })
        .transpose()
}

fn require_readable(document: &Value, manifest: &Path) -> Result<(), String> {
    let readable = concat_project::document_version(document) <= concat_project::DOCUMENT_VERSION
        && if super::is_settings_only(document) {
            serde_json::from_value::<super::Manifest>(document.clone())
                .is_ok_and(|manifest| manifest.video.rate_den != 0)
        } else {
            concat_project::from_document(document).is_some()
        };
    if readable {
        Ok(())
    } else {
        Err(format!(
            "{} 的工程文档无法安全读取，保存已停止，原文件保持不变。",
            manifest.display()
        ))
    }
}

fn read_manifest(root: &Path) -> Result<(PathBuf, Option<Value>), String> {
    for name in std::iter::once(super::MANIFEST).chain(super::LEGACY_MANIFESTS.iter().copied()) {
        let manifest = root.join(name);
        match std::fs::symlink_metadata(&manifest) {
            Ok(metadata)
                if metadata.is_file() && !crate::project_identity::redirected(&metadata) => {}
            Ok(_) => {
                return Err(format!(
                    "工程清单不是可核对的普通文件：{}",
                    manifest.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("无法核对工程清单 {}：{error}", manifest.display())),
        }
        let bytes = std::fs::read(&manifest)
            .map_err(|error| format!("无法读取工程清单 {}：{error}", manifest.display()))?;
        let document = serde_json::from_slice(&bytes)
            .map_err(|error| format!("工程清单 {} 已损坏，未覆盖：{error}", manifest.display()))?;
        return Ok((manifest, Some(document)));
    }
    Ok((root.join(super::MANIFEST), None))
}

struct Directory {
    // Keep the object alive while its identity is evidence for initialization.
    _file: File,
    identity: DirectoryIdentity,
}

#[cfg(unix)]
type DirectoryIdentity = (u64, u64);
#[cfg(windows)]
type DirectoryIdentity = (u64, [u8; 16]);
#[cfg(not(any(unix, windows)))]
type DirectoryIdentity = ();

impl Directory {
    fn open(root: &Path) -> Result<Self, String> {
        open_directory(root)
            .map(|(file, identity)| Self {
                _file: file,
                identity,
            })
            .map_err(|error| format!("无法核对工程目录对象 {}：{error}", root.display()))
    }

    fn matches(&self, root: &Path) -> Result<bool, String> {
        Ok(self.identity == Self::open(root)?.identity)
    }
}

#[cfg(unix)]
fn open_directory(root: &Path) -> std::io::Result<(File, DirectoryIdentity)> {
    use std::os::unix::fs::MetadataExt;
    let file = File::open(root)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "工程位置不是目录",
        ));
    }
    let identity = (metadata.dev(), metadata.ino());
    Ok((file, identity))
}

#[cfg(windows)]
fn open_directory(root: &Path) -> std::io::Result<(File, DirectoryIdentity)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(root)?;
    if !file.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "工程位置不是目录",
        ));
    }
    let mut info = FILE_ID_INFO::default();
    // SAFETY: file owns a live handle; info is writable, aligned storage of
    // the exact FILE_ID_INFO size selected by FileIdInfo. Neither is retained.
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
    Ok((file, (info.VolumeSerialNumber, info.FileId.Identifier)))
}

#[cfg(not(any(unix, windows)))]
fn open_directory(_root: &Path) -> std::io::Result<(File, DirectoryIdentity)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台无法核对工程目录对象",
    ))
}
