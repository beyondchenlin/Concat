// SPDX-License-Identifier: AGPL-3.0-or-later
use crate::commands::{SingleMainPlan, TemplateMedia, TemplateText};
use crate::model::{ClipKind, MediaItem, MediaKind, TextStyle};
use crate::{Command, Editor};
use std::collections::HashSet;

fn seeded() -> Editor {
    let mut p = Editor::new().project().clone();
    for (id, kind) in [("v", MediaKind::Video), ("a", MediaKind::Audio)] {
        p.media.push(MediaItem {
            id: id.into(),
            path: format!("/fixtures/{id}"),
            name: id.into(),
            kind,
            duration: Some(12.0),
            ..MediaItem::default()
        });
    }
    Editor::from_project(p)
}
fn media(id: &str, start: f64, from: f64, to: f64) -> TemplateMedia {
    TemplateMedia {
        media_id: id.into(),
        start,
        source_start: from,
        source_end: to,
        volume: 0.5,
    }
}
fn text(content: &str, start: f64, duration: f64) -> TemplateText {
    TemplateText {
        start,
        duration,
        offset_y: 0.3,
        style: TextStyle {
            content: content.into(),
            ..TextStyle::default()
        },
    }
}
fn plan() -> SingleMainPlan {
    SingleMainPlan {
        start: 0.0,
        videos: vec![media("v", 0.0, 1.0, 3.0), media("v", 2.0, 3.0, 5.0)],
        subtitles: vec![text("first", 0.0, 2.0), text("second", 2.0, 2.0)],
        overlays: vec![text("title", 0.0, 4.0)],
        music: vec![media("a", 0.0, 0.0, 4.0)],
        voice: vec![media("a", 0.0, 4.0, 8.0)],
    }
}
fn command(p: SingleMainPlan) -> Command {
    Command::ApplySingleMainTemplate { plan: p }
}

#[test]
fn five_roles_are_editable_and_one_undo_restores_tracks_clips_and_ids() {
    let mut e = seeded();
    let before = e.project().clone();
    e.apply_guarded(command(plan()), &HashSet::new()).unwrap();
    let after = e.project().clone();
    let t = after.active();
    assert_eq!(t.tracks.len(), before.active().tracks.len() + 5);
    assert_eq!(t.clips.len(), 7);
    let roles: HashSet<_> = t.clips.iter().map(|c| &c.track_id).collect();
    assert_eq!(roles.len(), 5);
    let position = |kind: ClipKind| {
        t.tracks
            .iter()
            .position(|track| {
                t.clips
                    .iter()
                    .any(|clip| clip.track_id == track.id && clip.kind == kind)
            })
            .unwrap()
    };
    assert!(position(ClipKind::Text) > position(ClipKind::Video));
    let subtitle_track = &t
        .clips
        .iter()
        .find(|c| c.text.as_ref().is_some_and(|s| s.content == "first"))
        .unwrap()
        .track_id;
    let overlay_track = &t
        .clips
        .iter()
        .find(|c| c.text.as_ref().is_some_and(|s| s.content == "title"))
        .unwrap()
        .track_id;
    assert!(
        t.tracks
            .iter()
            .position(|t| &t.id == overlay_track)
            .unwrap()
            > t.tracks
                .iter()
                .position(|t| &t.id == subtitle_track)
                .unwrap()
    );
    assert_eq!(
        t.clips.iter().filter(|c| c.kind == ClipKind::Video).count(),
        2
    );
    assert_eq!(
        t.clips.iter().filter(|c| c.kind == ClipKind::Audio).count(),
        2
    );
    assert_eq!(
        t.clips.iter().filter(|c| c.kind == ClipKind::Text).count(),
        3
    );
    assert!(
        t.clips
            .iter()
            .filter(|c| c.kind != ClipKind::Text)
            .all(|c| c.volume == 0.5)
    );
    assert!(e.undo());
    assert_eq!(e.project(), &before);
    assert!(!e.can_undo());
    assert!(e.redo());
    assert_eq!(e.project(), &after);
    assert!(!e.can_redo());
    let serialized = serde_json::to_string(&after).unwrap();
    let reopened = serde_json::from_str(&serialized).unwrap();
    assert_eq!(after, reopened);
}

#[test]
fn invalid_plan_and_late_source_failure_leave_no_tracks_or_history_or_minted_ids() {
    let mut bad = Vec::new();
    let mut p = plan();
    p.videos[1].start = 2.5;
    bad.push(p);
    let mut p = plan();
    p.voice[0].source_start = 11.0;
    p.voice[0].source_end = 15.0;
    bad.push(p);
    let mut p = plan();
    p.voice[0].media_id = "missing".into();
    bad.push(p);
    let mut p = plan();
    p.music[0].media_id = "v".into();
    bad.push(p);
    let mut p = plan();
    p.music[0].volume = f64::NAN;
    bad.push(p);
    let mut p = plan();
    p.subtitles[1].start = 1.0;
    bad.push(p);
    let mut p = plan();
    p.overlays[0].duration = 5.0;
    bad.push(p);
    let mut p = plan();
    p.videos.clear();
    bad.push(p);
    let mut p = plan();
    p.overlays = vec![text("x", 0.0, 1.0); 1024];
    bad.push(p);
    for p in bad {
        let mut e = seeded();
        let before = e.project().clone();
        assert!(e.apply_guarded(command(p), &HashSet::new()).is_err());
        assert_eq!(e.project(), &before);
        assert!(!e.can_undo());
        assert!(!e.can_redo());
        e.apply_guarded(command(plan()), &HashSet::new()).unwrap();
        let mut reference = seeded();
        reference
            .apply_guarded(command(plan()), &HashSet::new())
            .unwrap();
        assert_eq!(e.project(), reference.project());
    }
}

#[test]
fn existing_edits_and_nested_templates_are_refused_without_changing_history() {
    let mut e = seeded();
    e.apply_guarded(command(plan()), &HashSet::new()).unwrap();
    let before = e.project().clone();
    assert!(e.apply_guarded(command(plan()), &HashSet::new()).is_err());
    assert_eq!(e.project(), &before);
    let mut empty = seeded();
    let before = empty.project().clone();
    assert!(
        empty
            .apply_guarded(
                Command::Batch {
                    commands: vec![command(plan())]
                },
                &HashSet::new()
            )
            .is_err()
    );
    assert!(
        empty
            .apply_guarded(Command::AddTrack { bottom: false }, &HashSet::new())
            .is_err()
    );
    assert_eq!(empty.project(), &before);
}

#[test]
fn existing_locked_lanes_are_untouched_and_caller_cannot_supply_paths_or_role_ids() {
    let mut e = seeded();
    let before = e.project().clone();
    let locks = before
        .active()
        .tracks
        .iter()
        .map(|t| t.id.clone())
        .collect();
    e.apply_guarded(command(plan()), &locks).unwrap();
    assert_eq!(
        &e.project().active().tracks[..before.active().tracks.len()],
        &before.active().tracks[..]
    );
    let mut json = serde_json::to_value(plan()).unwrap();
    json["videos"][0]["path"] = "/private".into();
    assert!(serde_json::from_value::<SingleMainPlan>(json).is_err());
    let mut json = serde_json::to_value(plan()).unwrap();
    json["trackId"] = "T1".into();
    assert!(serde_json::from_value::<SingleMainPlan>(json).is_err());
}

#[test]
fn decimal_roundoff_is_canonicalized_but_real_gaps_and_overlaps_refuse() {
    let mut p = plan();
    p.videos = vec![
        media("v", 0.0, 1.0, 1.1),
        media("v", 0.1, 0.0, 0.2),
        media("v", 0.3, 0.0, 0.1),
    ];
    p.music.clear();
    p.voice.clear();
    p.overlays.clear();
    p.subtitles = vec![text("decimal", 0.0, 0.4)];
    let mut e = seeded();
    e.apply_guarded(command(p.clone()), &HashSet::new())
        .unwrap();
    let videos: Vec<_> = e
        .project()
        .active()
        .clips
        .iter()
        .filter(|c| c.kind == ClipKind::Video)
        .collect();
    assert_eq!(videos[1].start, videos[0].start + videos[0].duration);
    assert_eq!(videos[2].start, videos[1].start + videos[1].duration);
    for start in [0.09, 0.11] {
        let mut bad = p.clone();
        bad.videos[1].start = start;
        assert!(
            seeded()
                .apply_guarded(command(bad), &HashSet::new())
                .is_err()
        );
    }
    // The other direction: a decimal subtitle sum exceeds the binary film sum.
    let mut p = plan();
    p.videos = vec![media("v", 0.0, 0.0, 0.3)];
    p.music.clear();
    p.voice.clear();
    p.overlays.clear();
    p.subtitles = vec![text("a", 0.0, 0.1), text("b", 0.1, 0.2)];
    seeded().apply_guarded(command(p), &HashSet::new()).unwrap();
}

#[test]
fn template_cannot_implicitly_change_sdr_timeline_to_hdr() {
    let mut p = seeded().project().clone();
    p.media[0].color_space = crate::model::ColorSpace::Hlg;
    let mut e = Editor::from_project(p);
    let before = e.project().clone();
    assert!(matches!(
        e.apply_guarded(command(plan()), &HashSet::new()),
        Err(crate::GuardError::UnsupportedEffect)
    ));
    assert_eq!(e.project(), &before);
    assert!(!e.can_undo());
    assert!(!e.can_redo());
}

#[test]
fn native_first_hdr_template_matches_only_a_waiting_draft() {
    for state in [
        crate::initial_format::State::Waiting,
        crate::initial_format::State::UserSet,
        crate::initial_format::State::Disabled,
    ] {
        let mut project = seeded().project().clone();
        project.media[0].color_space = crate::model::ColorSpace::Pq;
        project.media[0].width = Some(1080);
        project.media[0].height = Some(1920);
        project.media[0].frame_rate_fraction = Some("30000/1001".into());
        crate::initial_format::initialize(
            &mut project,
            state != crate::initial_format::State::Disabled,
        );
        if state == crate::initial_format::State::UserSet {
            crate::initial_format::user_set(&mut project);
        }
        let mut editor = Editor::from_project(project.clone());
        let result = editor.apply_guarded(command(plan()), &HashSet::new());
        if state == crate::initial_format::State::Waiting {
            result.unwrap();
            assert_eq!(
                editor.project().active().video.color_space,
                crate::model::ColorSpace::Hlg
            );
            assert_eq!(editor.project().active().video.width, 1080);
            assert_eq!(editor.project().active().video.height, 1920);
            assert_eq!(editor.project().active().video.rate_den, 1001);
            assert!(editor.undo());
            assert_eq!(editor.project(), &project);
        } else {
            assert!(matches!(result, Err(crate::GuardError::UnsupportedEffect)));
            assert_eq!(editor.project(), &project);
            assert!(!editor.can_undo());
        }
    }
}

#[test]
fn rounded_audio_source_tail_is_trimmed_inward_not_rejected_or_expanded() {
    let mut p = plan();
    p.videos = vec![media("v", 0.0, 0.0, 0.3)];
    p.music = vec![media("a", 0.0, 1.0, 1.3)];
    p.voice.clear();
    p.overlays.clear();
    p.subtitles.clear();
    let mut e = seeded();
    e.apply_guarded(command(p.clone()), &HashSet::new())
        .unwrap();
    let audio = e
        .project()
        .active()
        .clips
        .iter()
        .find(|c| c.kind == ClipKind::Audio)
        .unwrap();
    assert!(audio.start + audio.duration <= 0.3);
    assert!(audio.duration > 0.3 - 1e-9);
    assert!(audio.duration <= p.music[0].source_end - p.music[0].source_start);
    // Material overruns are still refused without any state or ID effects.
    p.music[0].source_end = 1.300001;
    let mut rejected = seeded();
    let before = rejected.project().clone();
    assert!(rejected.apply_guarded(command(p), &HashSet::new()).is_err());
    assert_eq!(rejected.project(), &before);
    assert!(!rejected.can_undo());
}

#[test]
fn template_styles_reject_non_finite_and_tidy_changes_atomically() {
    use crate::model::{RunStyle, TextRun};
    let mut styles = Vec::new();
    // Every floating-point field, including the background fields omitted by
    // the legacy command's finite check, must reject all non-finite values.
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for field in 0..11 {
            let mut style = text("valid", 0.0, 1.0).style;
            let fields = [
                &mut style.font_size,
                &mut style.font_weight,
                &mut style.opacity,
                &mut style.stroke_width,
                &mut style.background_radius,
                &mut style.background_padding_x,
                &mut style.background_padding_y,
                &mut style.line_height,
                &mut style.tracking,
                &mut style.max_width,
                &mut style.max_height,
            ];
            *fields.into_iter().nth(field).unwrap() = value;
            styles.push(style);
        }
        for weight in [true, false] {
            let mut style = text("valid", 0.0, 1.0).style;
            style.runs.push(TextRun {
                start: 0,
                end: 1,
                style: RunStyle {
                    font_weight: weight.then_some(value),
                    font_scale: (!weight).then_some(value),
                    ..RunStyle::default()
                },
            });
            styles.push(style);
        }
    }
    let mutations: &[fn(&mut TextStyle)] = &[
        |s| s.font_size = 0.0,
        |s| s.font_weight = 1.0,
        |s| s.opacity = 2.0,
        |s| s.background_radius = -1.0,
        |s| s.background_padding_x = -1.0,
        |s| s.background_padding_y = -1.0,
        |s| {
            s.runs.push(TextRun {
                start: 0,
                end: 1,
                style: RunStyle {
                    font_weight: Some(1.0),
                    ..RunStyle::default()
                },
            })
        },
        |s| {
            s.runs.push(TextRun {
                start: 0,
                end: 1,
                style: RunStyle {
                    font_scale: Some(0.0),
                    ..RunStyle::default()
                },
            })
        },
        |s| {
            s.runs.push(TextRun {
                start: 0,
                end: 99,
                style: RunStyle {
                    font_weight: Some(700.0),
                    ..RunStyle::default()
                },
            })
        },
    ];
    for mutate in mutations {
        let mut style = text("valid", 0.0, 1.0).style;
        mutate(&mut style);
        styles.push(style);
    }
    for style in styles {
        // Both text roles have the same strict admission boundary.
        for overlay in [false, true] {
            let mut p = plan();
            if overlay {
                p.overlays[0].style = style.clone();
            } else {
                p.subtitles[0].style = style.clone();
            }
            let mut e = seeded();
            let before = e.project().clone();
            assert!(e.apply_guarded(command(p), &HashSet::new()).is_err());
            assert_eq!(e.project(), &before);
            assert!(!e.can_undo());
            assert!(!e.can_redo());
            e.apply_guarded(command(plan()), &HashSet::new()).unwrap();
            let mut reference = seeded();
            reference
                .apply_guarded(command(plan()), &HashSet::new())
                .unwrap();
            assert_eq!(e.project(), reference.project());
        }
    }
}

#[test]
fn template_wire_rejects_invalid_align_and_runs_without_project_recovery_defaults() {
    use serde_json::json;
    let invalid = [
        json!({"align": "sideways"}),
        json!({"align": null}),
        json!({"align": 2}),
        json!({"runs": [{"start": 0, "end": 1, "fontWeight": "bad"}]}),
        json!({"runs": [{"start": 0, "end": 1, "fontScale": "bad"}]}),
        json!({"runs": [{"start": 0}]}),
        json!({"runs": [{"start": -1, "end": 1}]}),
        json!({"runs": [null]}),
        json!({"runs": null}),
        json!({"runs": {}}),
        json!({"backgroundRadius": "bad"}),
        json!({"backgroundPaddingX": null}),
        json!({"textCase": "unknown"}),
    ];
    for fields in invalid {
        for role in ["subtitles", "overlays"] {
            let mut wire = serde_json::to_value(command(plan())).unwrap();
            // Use the real command wire shape, not an already-decoded TextStyle.
            for (key, value) in fields.as_object().unwrap() {
                wire["plan"][role][0]["style"][key] = value.clone();
            }
            let mut e = seeded();
            let before = e.project().clone();
            let decoded = serde_json::from_value::<Command>(wire);
            assert!(decoded.is_err(), "accepted invalid fields: {fields:?}");
            if let Ok(command) = decoded {
                e.apply_guarded(command, &HashSet::new()).unwrap();
            }
            assert_eq!(e.project(), &before);
            assert!(!e.can_undo());
            e.apply_guarded(command(plan()), &HashSet::new()).unwrap();
            let mut reference = seeded();
            reference
                .apply_guarded(command(plan()), &HashSet::new())
                .unwrap();
            assert_eq!(e.project(), reference.project());
        }
    }
    // Legal omitted fields retain native defaults, and valid rich text survives.
    let mut wire = serde_json::to_value(command(plan())).unwrap();
    wire["plan"]["subtitles"][0]["style"] = json!({"content": "valid"});
    wire["plan"]["overlays"][0]["style"] = json!({
        "content": "valid", "align": "left",
        "runs": [{"start": 0, "end": 1, "fontWeight": 700, "fontScale": 1.5}]
    });
    let decoded = serde_json::from_value::<Command>(wire).unwrap();
    let mut e = seeded();
    e.apply_guarded(decoded, &HashSet::new()).unwrap();
    assert_eq!(e.project().active().clips.len(), 7);
}

#[test]
fn template_colours_match_renderer_syntax_without_silent_fallbacks() {
    use crate::model::{RunStyle, TextRun};
    for colour in [
        "",
        "white",
        "#fff",
        "#12GG00",
        "#123456789",
        "123456",
        "#１２３４５６",
    ] {
        for field in 0..5 {
            if colour.is_empty() && field == 2 {
                continue;
            }
            let mut p = plan();
            let style = &mut p.subtitles[0].style;
            match field {
                0 => style.color = colour.into(),
                1 => style.stroke_color = colour.into(),
                2 => style.background = colour.into(),
                3 | 4 => style.runs.push(TextRun {
                    start: 0,
                    end: 1,
                    style: RunStyle {
                        color: (field == 3).then(|| colour.into()),
                        stroke_color: (field == 4).then(|| colour.into()),
                        ..RunStyle::default()
                    },
                }),
                _ => unreachable!(),
            }
            let mut e = seeded();
            let before = e.project().clone();
            assert!(e.apply_guarded(command(p), &HashSet::new()).is_err());
            assert_eq!(e.project(), &before);
            assert!(!e.can_undo());
            e.apply_guarded(command(plan()), &HashSet::new()).unwrap();
            let mut reference = seeded();
            reference
                .apply_guarded(command(plan()), &HashSet::new())
                .unwrap();
            assert_eq!(e.project(), reference.project());
        }
    }
    for colour in ["#AbCdEf", "#abcdef00", " #ABCDEF80 "] {
        let mut p = plan();
        let style = &mut p.overlays[0].style;
        style.color = colour.into();
        style.stroke_color = colour.into();
        style.background = colour.into();
        style.runs = vec![TextRun {
            start: 0,
            end: 1,
            style: RunStyle {
                color: Some(colour.into()),
                stroke_color: Some(colour.into()),
                ..RunStyle::default()
            },
        }];
        let expected = style.clone();
        let mut e = seeded();
        e.apply_guarded(command(p), &HashSet::new()).unwrap();
        assert!(
            e.project()
                .active()
                .clips
                .iter()
                .any(|clip| clip.text.as_ref() == Some(&expected))
        );
    }
}
