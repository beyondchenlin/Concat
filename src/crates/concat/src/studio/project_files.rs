// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors
//! Projects: opening, saving, recents and templates.

use super::*;

impl Studio {
    // ── projects ──

    /// Opens a project as the session and leaves the launch screen, or
    /// says why it could not.
    pub fn open_project(&mut self, info: ProjectInfo) -> Result<(), String> {
        log::info!("project: opening {}", info.path);
        // File › Open and Recents from a window with a project in it: that
        // project is closed first, as a close would - saved, its folder let
        // go, its jobs stopped. Swapping the session alone dropped what the
        // last autosave had not written yet and kept the old folder locked
        // against every other Concat for the rest of the run. A save that
        // fails keeps it open and the other one shut.
        if let Some(open) = self
            .session
            .as_ref()
            .map(|session| session.path().to_owned())
        {
            if same_folder(&open, &info.path) {
                return Ok(());
            }
            self.save_and_release()
                .map_err(|error| tf("studio.couldNotSave", &[&error]))?;
        }
        match self
            .host
            .open_projects
            .claim(&info.path, concat_api::Holder::Window)
        {
            Ok(()) => {}
            Err(concat_api::Refusal::Here(_)) => {
                return Err(t("studio.projectOpenThroughRemote"));
            }
            // Another process has it - a second window, an agent's
            // `concat --mcp`, the CLI - and two saving one folder lose
            // each other's work.
            Err(concat_api::Refusal::Elsewhere(owner)) => {
                let pid = owner
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "?".to_owned());
                log::warn!("project: {} is held by process {pid}", info.path);
                return Err(tf("studio.projectOpenElsewhere", &[&pid]));
            }
        }
        match Session::open_info(&info) {
            Ok(session) => {
                if let Err(error) = projects::remember(&self.host.dirs.config, &info) {
                    log::warn!("{error}");
                }
                self.pause();
                // The proxies of media no longer in the project, or read
                // as a range they no longer are, go now: nothing else
                // sweeps cache/proxy.
                let project_dir = std::path::Path::new(session.path());
                let kept: HashSet<std::path::PathBuf> = session
                    .project()
                    .media
                    .iter()
                    .filter_map(|item| {
                        concat_host::proxy::path_for(
                            project_dir,
                            &item.path,
                            item.color_range.map(concat_export::engine_range),
                            item.color_space.is_hdr(),
                        )
                    })
                    .collect();
                let swept = concat_host::proxy::sweep(project_dir, &kept);
                if swept > 0 {
                    log::info!("{swept} stale proxies swept from {}", project_dir.display());
                }
                self.session = Some(session);
                self.script = Default::default();
                // The relink dialog belongs to the project that opened it;
                // this project's own check follows below.
                self.relink = Default::default();
                crate::host::next_project_epoch();
                self.forget_art();
                self.art_failed.clear();
                self.art_stamp = None;
                self.proxied.clear();
                self.echo = None;
                self.dirty = false;
                self.project_name = info.name.clone();
                self.export.name = projects::folder_name(&info.name);
                self.selection.clear();
                self.transition_selected = None;
                self.media.selected.clear();
                self.playhead = 0.0;
                self.handle(crate::panes::Msg::Timeline(
                    crate::panes::timeline::TimelineMsg::Reset,
                ));
                self.on_start = false;
                self.handle(crate::panes::Msg::Monitor(
                    crate::panes::monitor::MonitorMsg::Opened,
                ));
                self.recents = projects::recents(&self.host.dirs.config);
                self.host.monitor.clear();
                self.audition = None;
                self.revision += 1;
                self.flat = None;
                self.sync_audio();
                self.request_media_art();
                self.request_preview();
                self.ensure_cutouts();
                self.ensure_stabilizers();
                self.ensure_voices();
                self.ensure_regions();

                // Effects this build no longer has, named once as the project
                // opens: their links stay in the clips, marked not installed.
                let retired = self
                    .session
                    .as_ref()
                    .map(|session| retired_effects(session.project()))
                    .unwrap_or_default();
                if !retired.is_empty() {
                    self.notify(&tf("studio.retiredEffects", &[&retired.join(", ")]), false);
                }
                // Effects that open as the ones standing in for them: the
                // document changes when it is next saved, so say so.
                let upgraded = self
                    .session
                    .as_ref()
                    .map(|session| upgraded_effects(session.upgraded()))
                    .unwrap_or_default();
                if !upgraded.is_empty() {
                    self.notify(
                        &tf("studio.upgradedEffects", &[&upgraded.join(", ")]),
                        false,
                    );
                }
                // Clips that played backwards or moved by a preset in an
                // earlier version, which this one does not read: named,
                // with the way back where there is one.
                let retired = self
                    .session
                    .as_ref()
                    .map(|session| session.retired().clone())
                    .unwrap_or_default();
                if !retired.reversed.is_empty() {
                    self.notify(
                        &tf("studio.clipsNowForwards", &[&few(&retired.reversed)]),
                        false,
                    );
                }
                if !retired.animated.is_empty() {
                    self.notify(
                        &tf("studio.clipsLostAnimations", &[&few(&retired.animated)]),
                        false,
                    );
                }

                // Log missing media to file for debugging
                if let Some(session) = &self.session {
                    let missing = session.project().missing_media();
                    if !missing.is_empty() {
                        let log_path = std::path::Path::new(&info.path)
                            .join("cache")
                            .join("missing_media.log");
                        if let Ok(mut file) = std::fs::File::create(&log_path) {
                            use std::io::Write;
                            let _ = writeln!(file, "{} media files missing:", missing.len());
                            for m in &missing {
                                let _ = writeln!(file, "  - {} ({})", m.name, m.path);
                            }
                        }

                        self.handle(crate::panes::Msg::Relink(
                            crate::panes::relink::RelinkMsg::Show(missing),
                        ));
                    }
                }
                Ok(())
            }
            Err(error) => {
                self.host.open_projects.release(&info.path);
                Err(error)
            }
        }
    }

    /// Looks for media the open project can no longer find, and opens the
    /// relink dialog on whatever is missing that the user has not already
    /// dismissed.
    ///
    /// Runs when the window is brought back to the front - the moment a
    /// person returns from moving a file - since the check
    /// [`Studio::open_project`] makes cannot see a file that went away
    /// afterwards: the bin and the timeline keep the item, the preview
    /// only fails to read it, and nothing else would say why.
    ///
    /// Returns whether the dialog changed, so the caller publishes only
    /// when there is something new to show.
    pub fn check_missing_media(&mut self) -> bool {
        if self.on_start || self.session.is_none() {
            return false;
        }
        let missing = self
            .session
            .as_ref()
            .map(|session| session.project().missing_media())
            .unwrap_or_default();
        let before: Vec<String> = self
            .relink
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect();
        let was_open = self.relink.open;
        self.handle(crate::panes::Msg::Relink(
            crate::panes::relink::RelinkMsg::Recheck(missing),
        ));
        let after: Vec<String> = self
            .relink
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect();
        self.relink.open != was_open || before != after
    }

    /// Clears all cached artwork and waveforms from the current project.
    pub fn clear_project_cache(&mut self) {
        let Some(path) = self
            .session
            .as_ref()
            .map(|session| session.path().to_string())
        else {
            return;
        };
        match concat_host::projects::clear_cache(&path) {
            Ok(count) => self.notify(&format!("Cleared {count} cache files"), false),
            Err(error) => self.notify(&format!("Cache clear failed: {error}"), true),
        }
        self.request_media_art();
        self.request_preview();
    }

    /// Saves, then closes the session and returns to the launch screen.
    ///
    /// A save that fails returns its reason and stops short of the close:
    /// the session, its running jobs and the autosave timer are left as they
    /// were, so a caller that was about to hide the window or quit can keep
    /// it up rather than drop unsaved work on the floor. Not untouched, to be
    /// exact - the close flushes a half-typed commit and pauses playback
    /// before it saves, and a failed save leaves those two as done. What a
    /// failure does not reach is the teardown: the worker cancellations, the
    /// job lists, the autosave timer and the session itself.
    pub fn close_project(&mut self) -> Result<(), String> {
        self.save_and_release().inspect_err(|error| {
            self.notify(&tf("studio.couldNotSave", &[error]), true);
        })
    }

    /// [`Studio::close_project`] without the notice: the project saved, then
    /// let go, or left open when the save fails.
    fn save_and_release(&mut self) -> Result<(), String> {
        self.script.before_close()?;
        self.save_and_release_project()
    }

    /// Finish an irrevocable native termination while the current project is
    /// still available. Local document work is drained without waiting for UI
    /// callbacks, then the timeline save is attempted even if that failed.
    /// Explicit quit-without-saving continues to use `release_project`.
    pub fn save_and_release_on_termination(&mut self) -> Result<(), String> {
        let script = self.script.finish_on_termination();
        // Do not use `?` above: document storage failure must never skip the
        // existing timeline save or replace its independent failure reason.
        let project = self.save_and_release_project();
        match (script, project) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(format!("文案结束保存失败：{error}")),
            (Ok(()), Err(error)) => Err(format!("时间线结束保存失败：{error}")),
            (Err(script), Err(project)) => Err(format!(
                "文案结束保存失败：{script}；时间线结束保存失败：{project}"
            )),
        }
    }

    fn save_and_release_project(&mut self) -> Result<(), String> {
        log::info!("project: closing {}", self.project_name);
        // Words still being typed land in the save, not on the floor.
        self.flush_commit();
        self.pause();
        // Saved before the workers are cancelled: a close that cannot be
        // written is not a close, and the jobs it would have stopped are
        // not ours to throw away.
        if let Some(session) = self.session.as_mut() {
            let (path, document) = session.prepare_save(None)?;
            let ticket = projects::ticket();
            let authority = session.save_authority();
            if let Err(error) =
                projects::save_ordered_with_authority(&path, &document, ticket, &authority)
            {
                log::warn!("project: could not be saved, not closing: {error}");
                return Err(error);
            }
        }
        self.release_project();
        Ok(())
    }

    /// Asks whether to quit though the project could not be saved: the
    /// window's ways out call it when `close_project` refused.
    pub fn offer_quit_without_saving(&mut self, error: String) {
        self.confirm = Some(Confirm::QuitWithoutSaving { error });
    }

    /// Lets the open project go, unsaved: what `close_project` does once
    /// the save has landed, and what quitting without saving does instead.
    pub(super) fn release_project(&mut self) {
        if self.session.is_some()
            && let Some(agent) = self.host.agent.as_ref()
        {
            // Revoke the original bridge before dropping its project or
            // changing the UI epoch. Epoch checks alone do not invalidate
            // the runtime's native storage or its queued protocol frames.
            crate::embedded::stop_for_project_change(agent);
        }
        self.host.cutouts.cancel();
        self.host.stabilizers.cancel();
        self.host.enhancers.cancel();
        self.host.reversers.cancel();
        self.enhance_jobs.clear();
        self.enhance_preview = None;
        self.enhance_previewing = None;
        self.enhance_ticked.clear();
        self.reverse_jobs.clear();
        self.cancel_merge();
        self.cutout_jobs.clear();
        self.stabilize_jobs.clear();
        self.stabilize_retry.clear();
        self.cancel_voices();
        self.region_job = None;
        self.picking = None;
        self.autosave.stop();
        // The launch screen's card for this project is read again: the
        // edit just made may have moved its first frame and its length.
        // The old poster stays up until the new one replaces it.
        if let Some(session) = self.session.as_ref() {
            self.lengths.remove(session.path());
        }
        if let Some(session) = self.session.as_ref() {
            self.host.open_projects.release(session.path());
        }
        self.session = None;
        self.script = Default::default();
        // Whatever a worker still brings back for this project is dropped
        // at delivery; the sheets that were waiting on one stop waiting.
        crate::host::next_project_epoch();
        self.forget_art();
        self.art_failed.clear();
        self.art_stamp = None;
        self.proxied.clear();
        self.captions.running = false;
        self.captions.pace = Default::default();
        self.speech.running = false;
        self.speech.pace = Default::default();
        // No dialog about files the project that just closed could not
        // find stays up over the launch screen.
        self.relink = Default::default();
        self.source = None;
        // A take running as the project closes stops, keeping its file;
        // the next project's speakers are not left silent.
        self.voiceover = Default::default();
        self.host.playback.set_muted(false);
        self.echo = None;
        self.dirty = false;
        self.selection.clear();
        self.transition_selected = None;
        self.gesture = Gesture::None;
        self.handle(crate::panes::Msg::Monitor(
            crate::panes::monitor::MonitorMsg::Closed,
        ));
        self.host.monitor.clear();
        self.audition = None;
        self.revision += 1;
        self.flat = None;
        self.host
            .playback
            .set_clips(std::path::PathBuf::new(), Vec::new());
        self.on_start = true;
        self.recents = projects::recents(&self.host.dirs.config);
    }

    /// Posters, and the running time, for the recents not yet looked at,
    /// read on a worker. A missing folder has neither to give.
    pub(super) fn request_posters(&mut self) {
        let wanted: Vec<String> = self
            .recents
            .iter()
            .filter(|recent| recent.present)
            .map(|recent| recent.info.path.clone())
            .filter(|path| !self.lengths.contains_key(path) && !self.posters_pending.contains(path))
            .collect();
        for path in wanted {
            self.posters_pending.insert(path.clone());
            spawn(
                move || {
                    let cached = media::poster_cache(&path);
                    // A project with no poster - nothing on it, or nothing
                    // but black - keeps the film mark; see `poster_frame`.
                    let made = media::poster_frame(&path).is_ok();
                    let length = media::timeline_seconds(&path);
                    (path, made.then_some(cached), length)
                },
                |studio, _, _, (path, cached, length)| {
                    studio.posters_pending.remove(&path);
                    if let Some(poster) = cached.and_then(|cached| image_at(&cached)) {
                        studio.posters.insert(path.clone(), poster);
                    }
                    studio.lengths.insert(path, length);
                },
            );
        }
    }

    /// Routes one message to its pane and applies it. The pane is taken out
    /// of the studio for the duration, so its `update` can be handed the
    /// rest of the window without borrowing itself twice; a pane never
    /// reads its own slot on the studio.
    pub fn handle(&mut self, msg: crate::panes::Msg) {
        // The activity trail: what the person did, for the log. A burst
        // - a wheel's zooms - is one line and a count; see `Burst`.
        if let Some(activity) = crate::panes::activity(&msg) {
            self.note_activity(activity);
        }
        match msg {
            crate::panes::Msg::Export(msg) => {
                let mut pane = std::mem::take(&mut self.export);
                pane.update(msg, self);
                self.export = pane;
            }
            crate::panes::Msg::Settings(msg) => {
                let mut pane = std::mem::take(&mut self.settings);
                pane.update(msg, self);
                self.settings = pane;
            }
            crate::panes::Msg::Captions(msg) => {
                let mut pane = std::mem::take(&mut self.captions);
                pane.update(msg, self);
                self.captions = pane;
            }
            crate::panes::Msg::Speech(msg) => {
                let mut pane = std::mem::take(&mut self.speech);
                pane.update(msg, self);
                self.speech = pane;
            }
            crate::panes::Msg::Voiceover(msg) => {
                let mut pane = std::mem::take(&mut self.voiceover);
                pane.update(msg, self);
                self.voiceover = pane;
            }
            crate::panes::Msg::Sounds(msg) => {
                let mut pane = std::mem::take(&mut self.sounds);
                pane.update(msg, self);
                self.sounds = pane;
            }
            crate::panes::Msg::Relink(msg) => {
                let mut pane = std::mem::take(&mut self.relink);
                pane.update(msg, self);
                self.relink = pane;
            }
            crate::panes::Msg::Project(msg) => {
                let mut pane = std::mem::take(&mut self.project_sheet);
                pane.update(msg, self);
                self.project_sheet = pane;
            }
            crate::panes::Msg::Start(msg) => {
                let mut pane = std::mem::take(&mut self.start);
                pane.update(msg, self);
                self.start = pane;
            }
            crate::panes::Msg::Media(msg) => {
                let mut pane = std::mem::take(&mut self.media);
                pane.update(msg, self);
                self.media = pane;
            }
            crate::panes::Msg::Monitor(msg) => {
                let mut pane = std::mem::take(&mut self.monitor);
                pane.update(msg, self);
                self.monitor = pane;
            }
            crate::panes::Msg::Timeline(msg) => {
                let mut pane = std::mem::take(&mut self.lanes);
                pane.update(msg, self);
                self.lanes = pane;
            }
        }
    }
}

#[cfg(test)]
mod project_retirement_tests {
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
}
