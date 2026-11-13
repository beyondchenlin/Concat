// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;
use std::path::PathBuf;

use concat_api::AppDirs;
use serde_json::Value;
use sha2::{Digest, Sha256};

mod bundled {
    include!(concat!(env!("OUT_DIR"), "/agent_assets.rs"));
}

const PI_COMMIT: &str = "d36266e06669c6991a7c9509d1da04d18d30378d";
const MAX_ASSET_BYTES: usize = 16 * 1024 * 1024;

/// Only reviewed, compiled-in resources are visible to a platform web view.
/// No resource name is interpreted as a filesystem path or a network URL.
pub struct RuntimeResources {
    files: BTreeMap<String, Vec<u8>>,
    profile: PathBuf,
}

impl RuntimeResources {
    #[cfg(test)]
    pub(crate) fn for_failed_factory() -> Self {
        // Used only by a factory that fails before requesting any resources.
        Self {
            files: BTreeMap::new(),
            profile: PathBuf::new(),
        }
    }

    pub(crate) fn bundled(dirs: &AppDirs, component: &str) -> Result<Self, String> {
        Self::checked(
            bundled::ASSETS,
            dirs.data.join("bk-cat").join("web-runtime").join(component),
            component,
        )
    }

    fn checked(
        assets: &[(&str, &[u8])],
        profile: PathBuf,
        component: &str,
    ) -> Result<Self, String> {
        if assets.is_empty() {
            return Err("本构建未包含聊天运行资源；请先构建固定版本的智能体资源。".into());
        }
        if component.len() != 64 || !component.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid native component capability".into());
        }
        let files: BTreeMap<_, _> = assets
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
            .collect();
        if files.len() != assets.len() {
            return Err("duplicate compiled runtime asset".into());
        }
        let manifest: Value = serde_json::from_slice(
            files
                .get("manifest.json")
                .ok_or("runtime manifest is absent")?,
        )
        .map_err(|_| "runtime manifest is invalid")?;
        if manifest["piCommit"] != PI_COMMIT
            || manifest["piVersion"] != "1.5.2"
            || manifest["runtimeVersion"] != "0.1.0"
            || manifest["protocolVersion"] != "2025-11-25"
            || manifest["sqliteSchemaVersion"] != 2
        {
            return Err("runtime resources differ from the reviewed pinned version".into());
        }
        for name in ["index.html", "runtime.js", "bootstrap.js"] {
            let bytes = files.get(name).ok_or("compiled runtime asset is absent")?;
            let expected = &manifest["files"][name];
            if bytes.is_empty()
                || bytes.len() > MAX_ASSET_BYTES
                || expected["bytes"].as_u64() != Some(bytes.len() as u64)
                || expected["sha256"].as_str()
                    != Some(format!("{:x}", Sha256::digest(bytes)).as_str())
                || std::str::from_utf8(bytes).is_err()
            {
                return Err(format!("runtime asset integrity check failed: {name}"));
            }
        }
        if files.get("PI-LICENSE.txt").is_none_or(Vec::is_empty) {
            return Err("the original component license is absent".into());
        }
        let mut files = files;
        let transport = include_str!("transport.js").replace(
            "__BKCAT_COMPONENT__",
            &serde_json::to_string(component).map_err(|error| error.to_string())?,
        );
        files.insert("transport.js".into(), transport.into_bytes());
        Ok(Self { files, profile })
    }

    /// An exact resource path; queries, fragments, encodings and traversal fail.
    pub fn resource(&self, path: &str) -> Option<(&'static str, Vec<u8>)> {
        let (name, mime) = match path {
            "/index.html" => ("index.html", "text/html; charset=utf-8"),
            "/runtime.js" => ("runtime.js", "application/javascript; charset=utf-8"),
            "/bootstrap.js" => ("bootstrap.js", "application/javascript; charset=utf-8"),
            "/transport.js" => ("transport.js", "application/javascript; charset=utf-8"),
            _ => return None,
        };
        self.files.get(name).cloned().map(|bytes| (mime, bytes))
    }

    /// Application-owned engine cache, separate from the sole session database.
    pub fn user_data_dir(&self) -> PathBuf {
        self.profile.clone()
    }
}

pub(crate) fn random_capability() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("native randomness unavailable: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assets() -> Vec<(String, Vec<u8>)> {
        let mut files = vec![
            ("index.html".to_owned(), b"<html></html>".to_vec()),
            ("runtime.js".to_owned(), b"void 0;".to_vec()),
            ("bootstrap.js".to_owned(), b"void 1;".to_vec()),
            ("PI-LICENSE.txt".to_owned(), b"original license".to_vec()),
        ];
        let mut metadata = serde_json::Map::new();
        for (name, bytes) in &files[..3] {
            metadata.insert(
                name.clone(),
                json!({
                    "bytes": bytes.len(), "sha256": format!("{:x}", Sha256::digest(bytes))
                }),
            );
        }
        files.push((
            "manifest.json".into(),
            serde_json::to_vec(&json!({
                "piCommit": PI_COMMIT, "piVersion": "1.5.2", "runtimeVersion": "0.1.0",
                "protocolVersion": "2025-11-25", "sqliteSchemaVersion": 2, "files": metadata
            }))
            .unwrap(),
        ));
        files
    }

    fn checked(files: &[(String, Vec<u8>)]) -> Result<RuntimeResources, String> {
        let refs: Vec<_> = files
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect();
        RuntimeResources::checked(&refs, PathBuf::from("isolated-cache"), &"a".repeat(64))
    }

    #[test]
    fn resource_allowlist_rejects_network_and_path_confusion() {
        let resources = checked(&assets()).unwrap();
        for path in [
            "/index.html",
            "/runtime.js",
            "/bootstrap.js",
            "/transport.js",
        ] {
            assert!(resources.resource(path).is_some(), "{path}");
        }
        for path in [
            "/",
            "/../runtime.js",
            "/%72untime.js",
            "/runtime.js?v=1",
            "/runtime.js#x",
            "https://other.invalid/runtime.js",
            "/manifest.json",
            "/PI-LICENSE.txt",
        ] {
            assert!(resources.resource(path).is_none(), "{path}");
        }
    }

    #[test]
    fn modified_or_missing_bundles_never_load() {
        assert!(checked(&[]).is_err());
        let mut files = assets();
        files[1].1.push(b'!');
        assert!(checked(&files).is_err());
        assert!(checked(&assets()[1..]).is_err());
        let mut files = assets();
        files.retain(|(name, _)| name != "PI-LICENSE.txt");
        assert!(checked(&files).is_err());
    }

    #[test]
    fn each_native_component_has_a_distinct_unpredictable_capability() {
        let first = random_capability().unwrap();
        let second = random_capability().unwrap();
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
    }
}
