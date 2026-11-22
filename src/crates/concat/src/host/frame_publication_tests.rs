// SPDX-License-Identifier: AGPL-3.0-or-later

//! Exercise the production preview publisher against real native properties
//! and models. Decoding is outside this test: the supplied image represents a
//! completed worker result, not an alternative implementation of the publisher.

use super::*;
use slint::{ComponentHandle, Model};

use crate::ui::{Editor, TimelineTabData, ToastKind};

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct QuietPlayback;

impl PlaybackEvents for QuietPlayback {
    fn position(&self, _: f64) {}
    fn error(&self, _: String) {}
}

fn with_window(test: impl FnOnce(&App, &mut Studio, &Models) + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(32 << 20)
        .spawn(move || {
            use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
            slint::platform::set_platform(Box::new(
                crate::agent::window::tests::NativeTestPlatform(MinimalSoftwareWindow::new(
                    RepaintBufferType::NewBuffer,
                )),
            ))
            .unwrap();
            let app = App::new().unwrap();
            app.set_phone(true);
            app.window().set_size(slint::LogicalSize::new(400., 800.));
            let scratch = Scratch(std::env::temp_dir().join(format!(
                "bk-cat-preview-publication-{}",
                crate::embedded::random_capability().unwrap()
            )));
            let dirs = AppDirs::under(&scratch.0.join("app"));
            std::fs::create_dir_all(&dirs.config).unwrap();
            let mut studio = Studio::new(Host {
                titles: Arc::new(concat_host::Titles::new(&dirs)),
                cutouts: Arc::new(concat_host::Cutouts::new(&dirs.data)),
                stabilizers: Arc::new(concat_host::Stabilizers::new()),
                brushes: Arc::new(concat_host::Brushes::new(&dirs.data)),
                enhancers: Arc::new(concat_host::Enhancers::new(&dirs.data)),
                reversers: Arc::new(concat_host::Reversers::new()),
                dirs,
                playback: Playback::start(Arc::new(QuietPlayback)).unwrap(),
                monitor: Monitor::new(),
                exporter: Exporter::new(),
                transcriber: Arc::new(Transcriber::new()),
                speech: Arc::new(Speech::new()),
                server: None,
                open_projects: Default::default(),
                agent: None,
            });
            let project = concat_host::projects::create(
                scratch.0.to_str().unwrap(),
                "Preview publication",
                640,
                360,
                30,
                1,
                [0, 0, 0, 255],
            )
            .unwrap();
            // A real session supplies editing/revision semantics without
            // starting decoders or the embedded assistant in this fixture.
            studio.session = Some(concat_host::Session::open_info(&project).unwrap());
            studio.on_start = false;
            studio.set_phone(true);
            let models = Models::new();
            {
                let editor = app.global::<Editor>();
                editor.set_timeline_tabs(slint::ModelRc::from(Rc::clone(&models.tabs)));
                editor.set_tracks(slint::ModelRc::from(Rc::clone(&models.tracks)));
                editor.set_clips(slint::ModelRc::from(Rc::clone(&models.clips)));
                editor.set_stage_items(slint::ModelRc::from(Rc::clone(&models.stage)));
            }
            studio.publish(&app, &models);
            test(&app, &mut studio, &models);
            drop(studio);
            drop(app);
            drop(scratch);
        })
        .unwrap()
        .join()
        .unwrap();
}

fn mark_unrelated_model(models: &Models) {
    models.tabs.set_row_data(
        0,
        TimelineTabData {
            id: "preview-publication-sentinel".into(),
            name: "Untouched by an image result".into(),
        },
    );
}

fn marker_present(models: &Models) -> bool {
    models
        .tabs
        .row_data(0)
        .is_some_and(|row| row.id == "preview-publication-sentinel")
}

#[test]
fn paused_phone_preview_updates_frames_without_rebuilding_editor_models() {
    with_window(|app, studio, models| {
        assert!(!studio.playing);
        let revision = studio.session.as_ref().unwrap().revision();
        mark_unrelated_model(models);
        let mut frames_published = 0;
        let mut unrelated_rebuilds = 0;
        for index in 0..120 {
            let width = 2 + (index % 2) * 2;
            studio.monitor.image =
                slint::Image::from_rgba8(slint::SharedPixelBuffer::new(width, 2));
            publish_completed_frame(studio, app, models, false);
            frames_published +=
                u32::from(app.global::<Editor>().get_preview_frame().size().width == width);
            unrelated_rebuilds += u32::from(!marker_present(models));
        }
        assert_eq!(
            frames_published, 120,
            "every completed frame reaches the native image property"
        );
        assert_eq!(
            unrelated_rebuilds, 0,
            "preview results must not bypass the gesture's model throttle"
        );
        assert_eq!(
            app.global::<Editor>()
                .get_timeline_tabs()
                .row_data(0)
                .unwrap()
                .id,
            "preview-publication-sentinel"
        );
        assert_eq!(studio.session.as_ref().unwrap().revision(), revision);

        let tracks = models.tracks.row_count();
        assert!(
            studio
                .apply(concat_project::Command::AddTrack { bottom: false })
                .is_some()
        );
        studio.publish(app, models);
        assert_eq!(
            models.tracks.row_count(),
            tracks + 1,
            "the ordinary edit publisher still updates track models"
        );
        assert_eq!(app.global::<Editor>().get_tracks().row_count(), tracks + 1);
        assert!(
            !marker_present(models),
            "a document edit still publishes the editor"
        );
        assert!(studio.session.as_ref().unwrap().revision() > revision);

        mark_unrelated_model(models);
        studio.notify("Preview failure fixture", true);
        publish_completed_frame(studio, app, models, true);
        assert!(
            !marker_present(models),
            "a failed preview retains the full error publication path"
        );
        assert_eq!(app.get_toast().message, "Preview failure fixture");
        assert_eq!(app.get_toast().kind, ToastKind::Failed);

        // Keep the existing paused desktop behavior. This regression is
        // scoped to the phone; it does not change desktop completion rules.
        app.set_phone(false);
        mark_unrelated_model(models);
        publish_completed_frame(studio, app, models, false);
        assert!(!marker_present(models));
    });
}

#[test]
fn phone_preview_publishes_measured_title_handles_without_timeline_rebuild() {
    with_window(|app, studio, models| {
        let id = studio
            .session
            .as_mut()
            .unwrap()
            .apply(concat_project::Command::AddTextClip {
                track_id: None,
                above: true,
                start: 0.,
                style: None,
                duration: Some(3.),
                offset_y: None,
            })
            .unwrap()
            .created_id
            .unwrap();
        studio.selection = vec![id.clone()];
        studio.publish(app, models);
        assert_eq!(models.stage.row_count(), 1);
        let estimated = models.stage.row_data(0).unwrap();
        assert!(estimated.selected);
        mark_unrelated_model(models);

        // This is the measured geometry supplied by preview_clips when
        // an asynchronous title painter finishes. A picture-only update
        // would leave the interactive selection handles at the estimate.
        studio.title_blocks.insert(id, ((160, 90), (0, 0)));
        publish_completed_frame(studio, app, models, false);
        let measured = app
            .global::<Editor>()
            .get_stage_items()
            .row_data(0)
            .unwrap();
        assert!((measured.w - 0.25).abs() < 1e-5);
        assert!((measured.h - 0.25).abs() < 1e-5);
        assert!(estimated.w != measured.w || estimated.h != measured.h);
        assert!(measured.selected);
        assert!(
            marker_present(models),
            "title measurement cannot force a timeline rebuild"
        );

        studio.playhead = 4.;
        publish_completed_frame(studio, app, models, false);
        assert_eq!(
            app.global::<Editor>().get_stage_items().row_count(),
            0,
            "scrubbing past the title removes its handles"
        );
        assert!(marker_present(models));
    });
}
