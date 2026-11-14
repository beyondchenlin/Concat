// SPDX-License-Identifier: AGPL-3.0-or-later
//! Persistent native project identity, independent of its folder and undo history.

use std::path::Path;

use serde_json::Value;

/// Host-owned top-level metadata. It travels with the native project manifest.
pub const FIELD: &str = "bkCatProjectId";
const PREFIX: &str = "bkcat-project:";

/// Whether an identity has exactly the native format, without path characters.
pub fn valid(identity: &str) -> bool {
    identity.strip_prefix(PREFIX).is_some_and(|value| {
        value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Read existing metadata. Missing means an older project, not damaged metadata.
pub fn from_document(document: &Value) -> Result<Option<String>, String> {
    let object = document
        .as_object()
        .ok_or("工程文档不是有效对象，无法核对文案归属。")?;
    match object.get(FIELD) {
        None => Ok(None),
        Some(Value::String(identity)) if valid(identity) => Ok(Some(identity.clone())),
        Some(_) => Err("工程持久标识已损坏，原工程和文案保持不变。".into()),
    }
}

/// Mint one identity for a new/legacy native session; the normal save persists it.
pub(crate) fn new() -> Result<String, String> {
    use std::fmt::Write;
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("无法生成工程标识：{error}"))?;
    let mut identity = String::from(PREFIX);
    for byte in bytes {
        write!(&mut identity, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(identity)
}

/// Inspect the actual manifest, refusing links, malformed metadata and unreadable
/// locations. A removed folder or a different/legacy project returns no match.
pub fn read_persisted(root: &Path) -> Result<Option<String>, String> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !redirected(&metadata) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        _ => return Err("工程位置无法安全核对，文案操作已停止。".into()),
    }
    let manifest = crate::projects::manifest_path(root);
    match std::fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.is_file() && !redirected(&metadata) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        _ => return Err("工程文档不是可核对的普通文件，文案操作已停止。".into()),
    }
    let bytes = std::fs::read(&manifest)
        .map_err(|error| format!("无法读取工程标识，文案操作已停止：{error}"))?;
    let document = serde_json::from_slice(&bytes)
        .map_err(|error| format!("工程文档已损坏，文案操作已停止：{error}"))?;
    from_document(&document)
}

pub(crate) fn redirected(metadata: &std::fs::Metadata) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stable_metadata_is_validated_without_repairing_unknown_values() {
        let identity = new().unwrap();
        assert!(valid(&identity));
        assert_eq!(
            from_document(&json!({FIELD: identity})).unwrap(),
            Some(identity)
        );
        assert_eq!(from_document(&json!({"name": "legacy"})).unwrap(), None);
        for value in [
            Value::Null,
            json!(42),
            json!("../outside"),
            json!("bkcat-project:bad"),
        ] {
            assert!(from_document(&json!({FIELD: value})).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn persisted_identity_refuses_a_linked_manifest() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside.json");
        let bytes = serde_json::to_vec(&json!({FIELD: new().unwrap()})).unwrap();
        std::fs::write(&outside, &bytes).unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::os::unix::fs::symlink(&outside, project.join("concat.json")).unwrap();
        assert!(read_persisted(&project).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), bytes);
    }

    #[cfg(windows)]
    #[test]
    fn persisted_identity_refuses_a_windows_directory_junction() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let junction = root.path().join("junction");
        std::fs::create_dir(&real).unwrap();
        let bytes = serde_json::to_vec(&json!({FIELD: new().unwrap()})).unwrap();
        std::fs::write(real.join("concat.json"), &bytes).unwrap();
        let result = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&real)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(read_persisted(&junction).is_err());
        assert_eq!(std::fs::read(real.join("concat.json")).unwrap(), bytes);
        std::fs::remove_dir(junction).unwrap();
    }
}
