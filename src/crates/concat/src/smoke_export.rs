// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah

//! CI's export run: a project made of one clip and exported, by the same
//! messages the launch screen, the media bin and the Export sheet send, in
//! the window's event loop and from inside its events, the way a person's
//! taps start them. A launch-only smoke run never reached an export, and
//! 0.2.10 and 0.2.11 shipped with every export on every phone freezing the
//! window the moment it started (#747, #786).
//!
//! Each step waits for the one before to have landed - asynchronous draft
//! creation, then the first import's native automatic placement - and the run
//! ends when the export does: status 0 with "smoke: export finished" in the
//! log, 1 with the reason. A step that does not land within
//! [`STEP_LIMIT`] ends it as well, with the step named; a window that has
//! stopped answering is what the caller's own clock and the system's
//! not-responding report are for, since a stuck event loop runs no timer.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use concat_project::model::{Clip, MediaItem};

use crate::host::Shell;
use crate::panes::Msg;
use crate::panes::export::ExportMsg;
use crate::panes::start::StartMsg;
use crate::ui::ExportPhase;

/// How long one step may take before the run fails on it. An export of a
/// short clip on an emulator in software is well under a minute.
const STEP_LIMIT: Duration = Duration::from_secs(240);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// Submit the fixture once through the launch screen's media entry.
    Begin,
    /// Wait for the asynchronous draft creation to settle.
    Make,
    /// Wait for the atomic first import, including its single clip.
    Place,
    /// The fixture is on the timeline; open Export and start it once.
    Export,
    /// The export is running.
    Watch,
}

impl Step {
    fn waiting_for(self) -> &'static str {
        match self {
            Step::Begin => "the launch screen to accept the fixture",
            Step::Make => "the project to open",
            Step::Place => "the first import to place its clip",
            Step::Export => "the export to start",
            Step::Watch => "the export to end",
        }
    }
}

/// Starts the run's steps on a timer of the window's event loop; the timer
/// is held for as long as the loop runs.
pub(crate) fn drive(clip: PathBuf) -> slint::Timer {
    log::info!("smoke: exporting {}", clip.display());
    let step = Cell::new(Step::Begin);
    let since = Cell::new(Instant::now());
    let began = RefCell::new(None::<Instant>);
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            Shell::with(|shell, app| {
                // Something else of the window's has the studio this tick.
                let Ok(mut studio) = shell.studio.try_borrow_mut() else {
                    return;
                };
                let now = step.get();
                let next = match now {
                    Step::Begin => {
                        if !studio.on_start || studio.session.is_some() || studio.start.busy {
                            fail("smoke export requires an idle launch screen");
                        }
                        if !clip.is_file() {
                            fail("the smoke export fixture is missing");
                        }
                        // On desktop use the isolated app profile, not the
                        // user's usual Documents folder, for smoke projects.
                        if crate::platform::phone() {
                            studio.handle(Msg::Start(StartMsg::Dropped(vec![clip.clone()])));
                        } else {
                            let location = studio.host.dirs.data.join("smoke-projects");
                            let prior = studio.prefs.new_project_defaults.location.replace(
                                location.to_string_lossy().into_owned(),
                            );
                            studio.prefs.empty_draft_path = None;
                            studio.handle(Msg::Start(StartMsg::Dropped(vec![clip.clone()])));
                            studio.prefs.new_project_defaults.location = prior;
                        }
                        Some(Step::Make)
                    }
                    Step::Make => match project_ready(
                        studio.start.busy,
                        studio.session.is_some(),
                        studio.on_start,
                        &studio.start.error,
                    ) {
                        Ok(true) => Some(Step::Place),
                        Ok(false) => None,
                        Err(error) => fail(&error),
                    },
                    Step::Place => match first_import_ready(
                        &studio.project().media,
                        &studio.timeline().clips,
                        &clip,
                    ) {
                        Ok(true) => Some(Step::Export),
                        Ok(false) => None,
                        Err(error) => fail(error),
                    },
                    Step::Export => {
                        studio.handle(Msg::Export(ExportMsg::Open));
                        studio.handle(Msg::Export(ExportMsg::Start));
                        // Left from a run before on the same device.
                        if matches!(
                            studio.confirm,
                            Some(crate::studio::Confirm::ReplaceExport { .. })
                        ) {
                            studio.confirm = None;
                            studio.handle(Msg::Export(ExportMsg::Overwrite));
                        }
                        log::info!("smoke: export started");
                        *began.borrow_mut() = Some(Instant::now());
                        Some(Step::Watch)
                    }
                    Step::Watch => match studio.export.phase {
                        ExportPhase::Done => {
                            let written = studio.export.written.clone();
                            let bytes = std::fs::metadata(&written).map(|meta| meta.len()).ok();
                            let secs = began.borrow().map_or(0.0, |at| at.elapsed().as_secs_f32());
                            match bytes {
                                Some(bytes) if bytes > 0 => {
                                    log::info!(
                                        "smoke: export finished in {secs:.1} s · {written} · {bytes} bytes"
                                    );
                                    crate::leave(0);
                                }
                                // On a phone the file may have gone on into
                                // the gallery, where no path reaches it.
                                _ if written.is_empty() || cfg!(target_os = "android") => {
                                    log::info!("smoke: export finished in {secs:.1} s · {written}");
                                    crate::leave(0);
                                }
                                _ => fail(&format!(
                                    "the export said it was done, and {written} is missing or empty"
                                )),
                            }
                        }
                        ExportPhase::Failed => {
                            fail(&format!("the export failed: {}", studio.export.message))
                        }
                        ExportPhase::Idle | ExportPhase::Running => None,
                    },
                };
                match next {
                    Some(next) => {
                        step.set(next);
                        since.set(Instant::now());
                    }
                    None if since.get().elapsed() > STEP_LIMIT => fail(&format!(
                        "waited {} s for {}",
                        STEP_LIMIT.as_secs(),
                        now.waiting_for()
                    )),
                    None => {}
                }
                drop(studio);
                shell.studio.borrow().publish(&app, &shell.models);
            });
        },
    );
    timer
}

/// A busy launch request is not a refusal. Never submit a second Create
/// while the first request's probe or draft worker is still in flight.
fn project_ready(
    busy: bool,
    has_session: bool,
    on_start: bool,
    error: &str,
) -> Result<bool, String> {
    if busy {
        return Ok(false);
    }
    if !error.is_empty() {
        return Err(format!("making the project failed: {error}"));
    }
    if has_session && !on_start {
        Ok(true)
    } else {
        Err("the project request ended without opening its draft".into())
    }
}

/// A fresh draft's first source must land in the bin and on its timeline in
/// the same transaction. Do not manually add a second clip, or mask a
/// regression that stopped automatic placement.
fn first_import_ready(
    media: &[MediaItem],
    clips: &[Arc<Clip>],
    expected: &Path,
) -> Result<bool, &'static str> {
    if media.is_empty() && clips.is_empty() {
        return Ok(false);
    }
    let ([item], [clip]) = (media, clips) else {
        return Err("first import did not atomically create exactly one media item and one clip");
    };
    let actual = Path::new(&item.path);
    let same_path = actual == expected
        || actual
            .canonicalize()
            .ok()
            .zip(expected.canonicalize().ok())
            .is_some_and(|(a, b)| a == b);
    if !same_path || item.id.is_empty() || clip.media_id != item.id {
        return Err("the first timeline clip does not reference the smoke fixture");
    }
    if clip.start != 0.0 || !clip.duration.is_finite() || clip.duration <= 0.0 {
        return Err("the first timeline clip has an invalid placement or duration");
    }
    Ok(true)
}

/// Ends the run as failed, with the reason in the log.
fn fail(why: &str) -> ! {
    log::error!("smoke: the export run failed: {why}");
    crate::leave(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asynchronous_draft_completion_is_not_a_refusal() {
        assert_eq!(project_ready(true, false, true, ""), Ok(false));
        assert_eq!(project_ready(true, true, true, ""), Ok(false));
        assert_eq!(project_ready(false, true, false, ""), Ok(true));
        assert!(project_ready(false, false, true, "").is_err());
        assert!(project_ready(false, true, true, "").is_err());
        assert_eq!(
            project_ready(false, false, true, "disk full"),
            Err("making the project failed: disk full".into())
        );
    }

    #[test]
    fn first_import_must_land_once_without_manual_placement() {
        let fixture = Path::new("fixture.mov");
        let item = MediaItem {
            id: "m1".into(),
            path: fixture.to_string_lossy().into_owned(),
            ..MediaItem::default()
        };
        let clip = Arc::new(Clip {
            media_id: item.id.clone(),
            start: 0.0,
            duration: 2.0,
            ..Clip::default()
        });
        let items = std::slice::from_ref(&item);
        assert_eq!(first_import_ready(&[], &[], fixture), Ok(false));
        assert!(first_import_ready(items, &[], fixture).is_err());
        assert_eq!(
            first_import_ready(items, std::slice::from_ref(&clip), fixture),
            Ok(true)
        );
        assert!(first_import_ready(items, &[clip.clone(), clip.clone()], fixture).is_err());
        let wrong = Arc::new(Clip {
            media_id: "m2".into(),
            ..(*clip).clone()
        });
        assert!(first_import_ready(items, &[wrong], fixture).is_err());
        let shifted = Arc::new(Clip {
            start: 1.0,
            ..(*clip).clone()
        });
        assert!(first_import_ready(items, &[shifted], fixture).is_err());
        assert!(first_import_ready(items, &[clip], Path::new("other.mov")).is_err());
    }
}
