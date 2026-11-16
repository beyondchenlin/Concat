// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use concat_agent::{Phase, Reconciliation, RecoveryDecision};
use concat_api::{AppDirs, Holder, OpenProjects};
use concat_host::export::Exporter;
use concat_host::playback::{Playback, PlaybackEvents};
use serde_json::json;

use super::*;
use crate::agent::AgentBridge;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "bk-cat-project-retirement-{}",
            crate::embedded::random_capability().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn project(&self, name: &str) -> ProjectInfo {
        projects::create(
            self.0.to_str().unwrap(),
            name,
            64,
            64,
            30,
            1,
            [0, 0, 0, 255],
        )
        .unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct QuietPlayback;

impl PlaybackEvents for QuietPlayback {
    fn position(&self, _seconds: f64) {}
    fn error(&self, _message: String) {}
}

// These tests call the actual Studio open/close paths with real Session
// objects, filesystem writes, project claims and agent ownership. They do
// not install a Shell or pretend to exercise a native window/WebView.
fn studio_in(scratch: &Scratch) -> Studio {
    static PLAYBACK: OnceLock<Arc<Playback>> = OnceLock::new();
    let dirs = AppDirs::under(&scratch.0.join("app"));
    std::fs::create_dir_all(&dirs.config).unwrap();
    let open_projects = OpenProjects::default();
    let exporter = Exporter::new();
    let agent =
        Arc::new(AgentBridge::acquire(&dirs, open_projects.clone(), exporter.clone()).unwrap());
    Studio::new(Host {
        titles: Arc::new(concat_host::Titles::new(&dirs)),
        cutouts: Arc::new(concat_host::Cutouts::new(&dirs.data)),
        stabilizers: Arc::new(concat_host::Stabilizers::new()),
        brushes: Arc::new(concat_host::Brushes::new(&dirs.data)),
        enhancers: Arc::new(concat_host::Enhancers::new(&dirs.data)),
        reversers: Arc::new(concat_host::Reversers::new()),
        dirs,
        playback: Arc::clone(
            PLAYBACK.get_or_init(|| Playback::start(Arc::new(QuietPlayback)).unwrap()),
        ),
        monitor: concat_host::preview::Monitor::new(),
        exporter,
        transcriber: Arc::new(concat_speech::Transcriber::new()),
        speech: Arc::new(concat_speech::Speech::new()),
        server: None,
        open_projects,
        agent: Some(agent),
    })
}

fn ready_bridge(studio: &Studio) -> Arc<AgentBridge> {
    let bridge = Arc::clone(studio.host.agent.as_ref().unwrap());
    let session = studio.session.as_ref().unwrap();
    bridge
        .confirm_recovery(
            Reconciliation {
                project_identity: session.path().to_owned(),
                project_revision: session.revision().to_string(),
                session_revision: session.session_id().to_string(),
            },
            RecoveryDecision::DiscardInterrupted,
        )
        .unwrap();
    let reply = bridge
        .handle(json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":"project-retirement-test", "version":"1"}}
        }))
        .unwrap()
        .unwrap();
    assert!(reply.get("result").is_some(), "{reply}");
    assert_eq!(bridge.status().unwrap().phase, Phase::Ready);
    bridge
}

fn ping(bridge: &AgentBridge) -> Result<Option<serde_json::Value>, String> {
    bridge.handle(json!({"jsonrpc":"2.0", "id":2, "method":"ping"}))
}

fn tracks(session: &Session) -> usize {
    session.project().active().tracks.len()
}

#[test]
fn real_project_switch_and_close_revoke_the_original_agents() {
    let scratch = Scratch::new();
    let first = scratch.project("First");
    let second = scratch.project("Second");
    let second_before = tracks(&Session::open_info(&second).unwrap());
    let mut studio = studio_in(&scratch);
    studio.open_project(first.clone()).unwrap();
    let old = ready_bridge(&studio);
    let first_before = tracks(studio.session.as_ref().unwrap());
    studio
        .session
        .as_mut()
        .unwrap()
        .apply(Command::AddTrack { bottom: false })
        .unwrap();

    studio.open_project(second.clone()).unwrap();
    assert_eq!(old.status().unwrap().phase, Phase::Draining);
    assert!(ping(&old).is_err(), "late frames lost their old authority");
    assert_eq!(studio.session.as_ref().unwrap().path(), second.path);
    assert_eq!(studio.host.open_projects.holder(&first.path), None);
    assert_eq!(
        studio.host.open_projects.holder(&second.path),
        Some(Holder::Window)
    );
    assert_eq!(
        tracks(&Session::open_info(&first).unwrap()),
        first_before + 1,
        "switching saved the first project's edit"
    );
    assert_eq!(tracks(studio.session.as_ref().unwrap()), second_before);

    // There is no platform in this fixture; explicitly finish its old
    // owner before creating the new project's recovery-only bridge.
    old.finish_handoff(Duration::from_secs(1)).unwrap();
    let replacement = Arc::new(
        AgentBridge::acquire(
            &studio.host.dirs,
            studio.host.open_projects.clone(),
            studio.host.exporter.clone(),
        )
        .unwrap(),
    );
    assert!(replacement.status().unwrap().generation > old.status().unwrap().generation);
    studio.host.agent = Some(replacement);
    let current = ready_bridge(&studio);
    studio
        .session
        .as_mut()
        .unwrap()
        .apply(Command::AddTrack { bottom: false })
        .unwrap();
    studio.close_project().unwrap();
    assert_eq!(current.status().unwrap().phase, Phase::Draining);
    assert!(ping(&current).is_err());
    assert!(ping(&old).is_err());
    assert!(studio.session.is_none());
    assert_eq!(studio.host.open_projects.holder(&second.path), None);
    assert_eq!(
        tracks(&Session::open_info(&second).unwrap()),
        second_before + 1,
        "closing saved the second project's edit"
    );
    current.finish_handoff(Duration::from_secs(1)).unwrap();
}

#[test]
fn failed_real_close_and_switch_keep_the_ready_agent_and_unsaved_edit() {
    let scratch = Scratch::new();
    let first = scratch.project("Blocked");
    let second = scratch.project("Still closed");
    let mut studio = studio_in(&scratch);
    studio.open_project(first.clone()).unwrap();
    let bridge = ready_bridge(&studio);
    let before = bridge.status().unwrap();
    let first_session = studio.session.as_ref().unwrap().session_id();
    let baseline_tracks = tracks(studio.session.as_ref().unwrap());
    let manifest = projects::manifest_path(Path::new(&first.path));
    let baseline = std::fs::read(&manifest).unwrap();
    studio
        .session
        .as_mut()
        .unwrap()
        .apply(Command::AddTrack { bottom: false })
        .unwrap();

    // A real destination failure, independent of privileges and of the
    // temporary filename chosen by projects::save_ordered.
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();
    assert!(studio.close_project().is_err());
    assert!(studio.open_project(second.clone()).is_err());
    let session = studio.session.as_ref().unwrap();
    assert_eq!(session.path(), first.path);
    assert_eq!(session.session_id(), first_session);
    assert_eq!(tracks(session), baseline_tracks + 1);
    assert_eq!(bridge.status().unwrap(), before);
    assert!(ping(&bridge).unwrap().unwrap().get("result").is_some());
    assert_eq!(
        studio.host.open_projects.holder(&first.path),
        Some(Holder::Window)
    );
    assert_eq!(studio.host.open_projects.holder(&second.path), None);

    std::fs::remove_dir(&manifest).unwrap();
    std::fs::write(&manifest, baseline).unwrap();
    studio.close_project().unwrap();
    assert_eq!(bridge.status().unwrap().phase, Phase::Draining);
    assert!(ping(&bridge).is_err());
    assert_eq!(
        tracks(&Session::open_info(&first).unwrap()),
        baseline_tracks + 1
    );
    bridge.finish_handoff(Duration::from_secs(1)).unwrap();
}

#[test]
fn complete_termination_refuses_a_replaced_project_and_preserves_both_unsaved_edits() {
    let scratch = Scratch::new();
    let first = scratch.project("Original");
    let second = scratch.project("Replacement");
    let mut studio = studio_in(&scratch);
    studio.open_project(first.clone()).unwrap();
    let bridge = ready_bridge(&studio);
    let identity = studio
        .session
        .as_ref()
        .unwrap()
        .project_identity()
        .to_owned();
    let session_id = studio.session.as_ref().unwrap().session_id();
    let baseline_tracks = tracks(studio.session.as_ref().unwrap());
    let manifest = projects::manifest_path(Path::new(&first.path));
    let original_bytes = std::fs::read(&manifest).unwrap();
    let replacement_manifest = projects::manifest_path(Path::new(&second.path));
    let replacement_bytes = std::fs::read(&replacement_manifest).unwrap();
    assert_ne!(original_bytes, replacement_bytes);
    let root = studio
        .host
        .dirs
        .data
        .join("bk-cat")
        .join("script-workspaces");
    let draft = "完整退出也必须保留这份尚未保存的文案。";
    studio.script = crate::panes::script::ScriptPane::disk_draft_for_test(
        root.clone(),
        first.path.clone(),
        identity.clone(),
        draft,
    );
    studio
        .session
        .as_mut()
        .unwrap()
        .apply(Command::AddTrack { bottom: false })
        .unwrap();
    studio.dirty = true;

    // Replace the actual native manifest after both editors captured the
    // original project. This must exercise the complete termination path,
    // not only the script pane's independent refusal.
    std::fs::write(&manifest, &replacement_bytes).unwrap();
    let error = studio.save_and_release_on_termination().unwrap_err();
    assert!(error.contains("文案结束保存失败"), "{error}");
    assert!(error.contains("时间线结束保存失败"), "{error}");
    assert_eq!(std::fs::read(&manifest).unwrap(), replacement_bytes);
    assert_eq!(
        std::fs::read(&replacement_manifest).unwrap(),
        replacement_bytes
    );
    assert_eq!(studio.session.as_ref().unwrap().session_id(), session_id);
    assert_eq!(
        tracks(studio.session.as_ref().unwrap()),
        baseline_tracks + 1
    );
    assert!(studio.dirty);
    assert_eq!(studio.script.draft_text_for_test(), draft);
    assert_eq!(
        studio.host.open_projects.holder(&first.path),
        Some(Holder::Window)
    );
    assert_eq!(bridge.status().unwrap().phase, Phase::Ready);
    assert!(
        concat_agent::scripts::ScriptStore::open(&root)
            .unwrap()
            .load(&identity)
            .unwrap()
            .is_none()
    );

    // Restoring the original file represents an explicit resolution of the
    // conflict. Both retained edits can then reach disk via normal teardown.
    std::fs::write(&manifest, &original_bytes).unwrap();
    studio.save_and_release_on_termination().unwrap();
    assert!(studio.session.is_none());
    assert_eq!(
        tracks(&Session::open_info(&first).unwrap()),
        baseline_tracks + 1
    );
    assert_eq!(
        concat_agent::scripts::ScriptStore::open(&root)
            .unwrap()
            .load(&identity)
            .unwrap()
            .unwrap()
            .latest
            .content,
        draft
    );
    assert_eq!(
        std::fs::read(&replacement_manifest).unwrap(),
        replacement_bytes
    );
    assert_eq!(bridge.status().unwrap().phase, Phase::Draining);
    bridge.finish_handoff(Duration::from_secs(1)).unwrap();
}

#[test]
fn explicit_discard_revokes_the_agent_without_saving_the_edit() {
    let scratch = Scratch::new();
    let first = scratch.project("Discarded");
    let mut studio = studio_in(&scratch);
    studio.open_project(first.clone()).unwrap();
    let bridge = ready_bridge(&studio);
    let baseline = tracks(studio.session.as_ref().unwrap());
    studio
        .session
        .as_mut()
        .unwrap()
        .apply(Command::AddTrack { bottom: false })
        .unwrap();

    studio.release_project();
    assert_eq!(bridge.status().unwrap().phase, Phase::Draining);
    assert!(ping(&bridge).is_err());
    assert!(studio.session.is_none());
    assert_eq!(studio.host.open_projects.holder(&first.path), None);
    assert_eq!(tracks(&Session::open_info(&first).unwrap()), baseline);
    bridge.finish_handoff(Duration::from_secs(1)).unwrap();
}

#[test]
fn empty_close_and_same_project_open_preserve_the_current_agent() {
    let scratch = Scratch::new();
    let first = scratch.project("Unchanged");
    let mut studio = studio_in(&scratch);
    let initial = studio.host.agent.as_ref().unwrap().status().unwrap();
    studio.close_project().unwrap();
    assert_eq!(
        studio.host.agent.as_ref().unwrap().status().unwrap(),
        initial
    );

    studio.open_project(first.clone()).unwrap();
    let bridge = ready_bridge(&studio);
    let before = bridge.status().unwrap();
    let session_id = studio.session.as_ref().unwrap().session_id();
    studio.open_project(first).unwrap();
    assert_eq!(bridge.status().unwrap(), before);
    assert_eq!(studio.session.as_ref().unwrap().session_id(), session_id);
    assert!(ping(&bridge).unwrap().unwrap().get("result").is_some());
    studio.close_project().unwrap();
    bridge.finish_handoff(Duration::from_secs(1)).unwrap();
}
