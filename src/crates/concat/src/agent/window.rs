// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pi callbacks attached to the native editor window.

use crate::{
    embedded,
    host::Shell,
    i18n::{t, tf},
    panes,
    ui::{self, AgentUi, App},
};
use slint::ComponentHandle;

pub(crate) fn install(app: &App) -> slint::Timer {
    panes::agent_settings::install(app);
    install_transcript_scroll_observer(app);

    // State comes from the same native owner as protocol admission. A rendered
    // enabled button is never authority: the send callback reads it again.
    panes::script::install(app);
    panes::agent::install_composer(app);
    let agent_ui = app.global::<AgentUi>();
    agent_ui.on_send_requested(|draft| {
        embedded::show_ui_error(embedded::submit_chat_from_ui(draft.to_string(), false));
    });
    agent_ui.on_edit_requested(|draft| {
        embedded::show_ui_error(embedded::submit_chat_from_ui(draft.to_string(), true));
    });
    agent_ui.on_stop_requested(embedded::stop_from_ui);
    app.global::<ui::AgentCallUi>()
        .on_authorize_requested(embedded::authorize_call_from_ui);
    app.global::<AgentUi>().on_tool_mode_requested(|index| {
        embedded::show_ui_error(embedded::set_tool_mode_from_ui(index));
    });
    app.global::<AgentUi>()
        .on_tool_decision_requested(|request_id, approve| {
            embedded::show_ui_error(embedded::decide_tool_from_ui(
                request_id.to_string(),
                approve,
            ));
        });
    agent_ui.on_review_requested(|| {
        let mut result = Err(t("agent.chatNotReady"));
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.inspect();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_review_current_requested(|| {
        let mut result = Err(t("agent.chatNotReady"));
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.confirm_current_review();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_continue_requested(|| {
        let mut result = Err(t("agent.chatNotReady"));
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.confirm();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_discard_requested(|| {
        let mut result = Err(t("agent.chatNotReady"));
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.discard_interrupted();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_assistant_open_requested(|| {
        #[cfg(all(
            any(target_os = "windows", target_os = "android"),
            not(feature = "bk-cat-native-probe")
        ))]
        embedded::activate_from_user_intent();
    });
    // Runtime messages, permission decisions and view switches are pushed to
    // the native event loop. Ordinary editing starts NO periodic agent timer.
    slint::Timer::default()
}

/// Observe intent before the backend starts a smooth upward wheel animation.
/// Flickable's position-only callback also fires when downward inertia starts
/// at the lower bound, so that callback cannot distinguish these two inputs.
/// This observer never accepts an event or changes any editor/model state.
#[cfg(not(target_os = "android"))]
fn install_transcript_scroll_observer(app: &App) {
    use slint::winit_030::winit::event::{TouchPhase, WindowEvent};
    use slint::winit_030::{EventResult, WinitWindowAccessor};

    let weak = app.as_weak();
    let mut cursor = None;
    let mut shift = false;
    let mut target = TranscriptScrollTarget::default();
    // This is the window's only on_winit_window_event registration. It is
    // separate from platform::DropHandler's application handler. Registration
    // lives on the window adapter and also works before its native window is
    // shown; a non-winit adapter simply ignores it.
    app.window().on_winit_window_event(move |window, event| {
        match event {
            WindowEvent::CursorMoved { position, .. } => cursor = Some(*position),
            WindowEvent::CursorLeft { .. } | WindowEvent::Focused(false) => cursor = None,
            WindowEvent::ModifiersChanged(modifiers) => shift = modifiers.state().shift_key(),
            WindowEvent::MouseWheel { delta, phase, .. } if *phase != TouchPhase::Ended => {
                if let Some(position) = cursor
                    && let Some(app) = weak.upgrade()
                {
                    pause_transcript_follow_for_native_wheel(
                        &app,
                        window,
                        &mut target,
                        position,
                        delta,
                        shift,
                    );
                }
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

#[cfg(target_os = "android")]
fn install_transcript_scroll_observer(_app: &App) {}

#[cfg(not(target_os = "android"))]
#[derive(Default)]
struct TranscriptScrollTarget {
    // The transcript is located once per mounted chat component. Subsequent
    // wheel input, including input over the native editor, only checks this
    // weak item's current geometry; it does not walk a large timeline tree.
    item: Option<slint::private_unstable_api::re_exports::ItemWeak>,
}

#[cfg(not(target_os = "android"))]
fn pause_transcript_follow_for_native_wheel(
    app: &App,
    window: &slint::Window,
    target: &mut TranscriptScrollTarget,
    position: slint::winit_030::winit::dpi::PhysicalPosition<f64>,
    delta: &slint::winit_030::winit::event::MouseScrollDelta,
    shift: bool,
) {
    let position = position.to_logical::<f32>(f64::from(window.scale_factor()));
    pause_transcript_follow_for_wheel(
        app,
        target,
        slint::LogicalPosition::new(position.x, position.y),
        transcript_wheel_delta_y(delta, shift),
    );
}

#[cfg(not(target_os = "android"))]
fn transcript_wheel_delta_y(
    delta: &slint::winit_030::winit::event::MouseScrollDelta,
    shift: bool,
) -> f32 {
    use slint::winit_030::winit::event::MouseScrollDelta;
    let (x, y) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (*x, *y),
        MouseScrollDelta::PixelDelta(position) => (position.x as f32, position.y as f32),
    };
    // Match Flickable's existing Shift-axis convention, including macOS.
    if shift && !cfg!(target_os = "macos") {
        x
    } else {
        y
    }
}

#[cfg(not(target_os = "android"))]
fn pause_transcript_follow_for_wheel(
    app: &App,
    target: &mut TranscriptScrollTarget,
    position: slint::LogicalPosition,
    delta_y: f32,
) {
    use slint::private_unstable_api::re_exports::{
        AccessibleStringProperty, Flickable, WindowInner,
    };
    use std::ops::ControlFlow;

    if delta_y <= 0. || !delta_y.is_finite() || !position.x.is_finite() || !position.y.is_finite() {
        return;
    }
    let agent = app.global::<AgentUi>();
    if app.get_on_start()
        || (app.get_phone() && (agent.get_phone_sheet_state() == 0 || agent.get_phone_script()))
        || (!app.get_phone()
            && (agent.get_view() == ui::AgentView::Script || !agent.get_sidebar_open()))
    {
        target.item = None;
        agent.set_native_scroll_up_pending(false);
        return;
    }
    let size = app.window().size().to_logical(app.window().scale_factor());
    if position.x < 0. || position.y < 0. || position.x >= size.width || position.y >= size.height {
        return;
    }
    let window = WindowInner::from_pub(app.window());
    if app.invoke_content_overlay_covers(position.x, position.y)
        || !window.active_popups().is_empty()
    {
        return;
    }
    let mut item = target
        .item
        .as_ref()
        .and_then(|item| item.upgrade())
        .filter(|item| {
            item.is_visible()
                && item
                    .accessible_string_property(AccessibleStringProperty::Id)
                    .is_some_and(|id| id == "bkcut-chat-transcript")
        });
    if item.is_none() {
        let Some(root) = window.window_item_rc() else {
            return;
        };
        item = root.visit_descendants(|item| {
            if item.is_visible()
                && item.downcast::<Flickable>().is_some()
                && item
                    .accessible_string_property(AccessibleStringProperty::Id)
                    .is_some_and(|id| id == "bkcut-chat-transcript")
            {
                return ControlFlow::Break(item.clone());
            }
            ControlFlow::Continue(())
        });
        target.item = item.as_ref().map(|item| item.downgrade());
    }
    let Some(item) = item.filter(|item| item.is_visible()) else {
        return;
    };
    let geometry = item.geometry();
    let origin = item.map_to_window(geometry.origin);
    if position.x >= origin.x
        && position.y >= origin.y
        && position.x < origin.x + geometry.size.width
        && position.y < origin.y + geometry.size.height
    {
        let Some(flick) = item.downcast::<Flickable>() else {
            return;
        };
        let pin = flick.as_pin_ref();
        let content_height = Flickable::FIELD_OFFSETS
            .content_height()
            .apply_pin(pin)
            .get()
            .0;
        let content_y = Flickable::FIELD_OFFSETS.content_y().apply_pin(pin).get().0;
        // A short conversation (or a transcript already at its top) cannot
        // scroll upward. Such input must not disable later live following.
        if content_height > geometry.size.height && content_y < 0. {
            agent.set_native_scroll_up_pending(true);
        }
    }
}

/// Dispatched by a real native tool-approval event, never by a frame timer.
/// Each new pending request is revealed even in manual editing. Refreshing
/// the same request preserves the person's subsequent presentation choice.
pub(crate) fn refresh_tool_permission() {
    Shell::with(|shell, app| {
        let ui = app.global::<AgentUi>();
        let runtime = shell.embedded.borrow().clone();
        let Some(runtime) = runtime else {
            ui.set_tool_connected(false);
            ui.set_tool_pending(false);
            ui.set_tool_request_id("".into());
            ui.set_tool_summary("".into());
            return;
        };
        let Some(ready) = runtime.try_ready() else {
            slint::Timer::single_shot(
                std::time::Duration::from_millis(40),
                refresh_tool_permission,
            );
            return;
        };
        let Ok(permission) = runtime.tool_permission_view() else {
            ui.set_tool_connected(false);
            ui.set_tool_pending(false);
            ui.set_tool_request_id("".into());
            ui.set_tool_summary("".into());
            return;
        };
        let pending = permission.pending_summary.is_some();
        let request_id = permission.pending_id.unwrap_or_default();
        let reveal_pending = pending
            && ready
            && (!ui.get_tool_pending()
                || !ui.get_tool_connected()
                || ui.get_tool_request_id().as_str() != request_id.as_str());
        ui.set_tool_mode(permission.mode);
        ui.set_tool_connected(ready);
        ui.set_tool_request_id(request_id.into());
        ui.set_tool_pending(pending);
        let mut summary = permission.pending_summary.unwrap_or_default();
        if pending && !permission.pending_clip_ids.is_empty() {
            summary.push('\n');
            summary.push_str(&t("permission.currentClips"));
            let studio = shell.studio.borrow();
            for id in &permission.pending_clip_ids {
                if let Some(clip) = studio.clip(id) {
                    let name: String = clip
                        .name
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(48)
                        .collect();
                    summary.push_str(&tf(
                        "permission.currentClip",
                        &[
                            id,
                            &name,
                            &clip.track_id,
                            &format!("{:.3}", clip.start),
                            &format!("{:.3}", clip.start + clip.duration),
                        ],
                    ));
                } else {
                    summary.push_str(&tf("permission.missingClip", &[id]));
                }
            }
        }
        ui.set_tool_summary(summary.into());
        if pending && ready {
            if app.get_phone() {
                if reveal_pending {
                    // A pending native decision reveals the assistant without
                    // replacing the live timeline or changing operation authority.
                    ui.set_phone_script(false);
                    ui.set_phone_sheet_state(ui.get_phone_sheet_state().max(1));
                }
            } else {
                ui.set_view(ui::AgentView::Chat);
                ui.set_sidebar_open(true);
            }
        }
    });
}

/// Temporary attachment import is the only reason to schedule another check.
/// When imports finish, no timer or background UI work remains.
pub(crate) fn schedule_attachment_watch() {
    slint::Timer::single_shot(std::time::Duration::from_millis(250), || {
        let mut pending = false;
        Shell::with(|shell, app| {
            shell
                .agent_pane
                .borrow_mut()
                .collect_attachments(&shell.studio.borrow(), &app);
            pending = app.global::<AgentUi>().get_attachment_pending();
        });
        if pending {
            schedule_attachment_watch();
        }
    });
}

/// Revocation only: never wait on the UI thread for callbacks it must drain.
pub(crate) fn stop() {
    Shell::with(|shell, _| {
        if let Some(agent) = shell.agent.borrow().as_ref() {
            agent.stop();
        }
        if let Some(runtime) = shell.embedded.borrow().as_ref() {
            runtime.stop();
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ops::ControlFlow;
    use std::rc::Rc;

    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
    use slint::private_unstable_api::re_exports::{
        ComplexText, LogicalPoint, SimpleText, TextInput, WindowInner,
    };

    pub(crate) struct NativeTestPlatform(pub(crate) Rc<MinimalSoftwareWindow>);

    impl Platform for NativeTestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    fn rendered_button(app: &App, label: &str) -> slint::LogicalPosition {
        // Render the actual App and hit-test its production approval controls;
        // this is not a duplicate of the gesture state machine in test code.
        app.window().take_snapshot().unwrap();
        let root = WindowInner::from_pub(app.window())
            .window_item_rc()
            .unwrap();
        let mut points = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible() {
                let matches = item.downcast::<SimpleText>().is_some_and(|text| {
                    SimpleText::FIELD_OFFSETS
                        .text()
                        .apply_pin(text.as_pin_ref())
                        .get()
                        == label
                }) || item.downcast::<ComplexText>().is_some_and(|text| {
                    ComplexText::FIELD_OFFSETS
                        .text()
                        .apply_pin(text.as_pin_ref())
                        .get()
                        == label
                });
                if matches {
                    let size = item.geometry().size;
                    points.push(
                        item.map_to_window(LogicalPoint::new(size.width / 2., size.height / 2.)),
                    );
                }
            }
            ControlFlow::Continue(())
        });
        assert_eq!(points.len(), 1, "unique approval label {label}");
        slint::LogicalPosition::new(points[0].x, points[0].y)
    }

    #[test]
    fn approval_pointer_and_keyboard_cannot_cross_prompt_replacement() {
        let _language = crate::i18n::language_lock();
        // A dedicated test thread has its own software-only Slint platform;
        // no OS desktop, model, project or application Shell is opened.
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                slint::platform::set_platform(Box::new(NativeTestPlatform(window))).unwrap();
                let app = App::new().unwrap();
                crate::i18n::install(&app);
                app.set_phone(true);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(600., 800.));
                app.show().unwrap();
                let agent = app.global::<AgentUi>();
                agent.set_phone_sheet_state(2);
                agent.set_tool_connected(true);
                agent.set_tool_summary("审批手势回归".into());
                agent.set_tool_request_id("request-a".into());
                agent.set_tool_pending(true);
                let decisions = Rc::new(RefCell::new(Vec::new()));
                let received = Rc::clone(&decisions);
                agent.on_tool_decision_requested(move |id, approved| {
                    received.borrow_mut().push((id.to_string(), approved));
                });
                let approve = rendered_button(&app, crate::i18n::t("agent.approveOnce").as_str());
                let press = || {
                    app.window()
                        .dispatch_event(WindowEvent::PointerMoved { position: approve });
                    app.window().dispatch_event(WindowEvent::PointerPressed {
                        position: approve,
                        button: PointerEventButton::Left,
                    });
                };
                let release = || {
                    app.window().dispatch_event(WindowEvent::PointerReleased {
                        position: approve,
                        button: PointerEventButton::Left,
                    })
                };

                press();
                agent.set_tool_request_id("request-b".into());
                rendered_button(&app, crate::i18n::t("agent.approveOnce").as_str());
                release();
                assert!(
                    decisions.borrow().is_empty(),
                    "a stale press approved the replacement prompt"
                );
                press();
                release();
                assert_eq!(&*decisions.borrow(), &[("request-b".into(), true)]);
                decisions.borrow_mut().clear();

                // The pointer focused the real approval button. Pressing then
                // replacing a prompt, even with auto-repeat, must remain inert.
                app.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
                agent.set_tool_request_id("request-c".into());
                rendered_button(&app, crate::i18n::t("agent.approveOnce").as_str());
                app.window()
                    .dispatch_event(WindowEvent::KeyPressRepeated { text: " ".into() });
                app.window()
                    .dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
                assert!(
                    decisions.borrow().is_empty(),
                    "a stale key approved the replacement prompt"
                );
                app.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
                app.window()
                    .dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
                assert_eq!(&*decisions.borrow(), &[("request-c".into(), true)]);
                decisions.borrow_mut().clear();
                agent.set_tool_request_id("request-d".into());
                rendered_button(&app, crate::i18n::t("agent.approveOnce").as_str());
                app.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
                app.window()
                    .dispatch_event(WindowEvent::KeyReleased { text: "\n".into() });
                assert_eq!(&*decisions.borrow(), &[("request-d".into(), true)]);
                decisions.borrow_mut().clear();

                let deny = rendered_button(&app, crate::i18n::t("agent.reject").as_str());
                app.window()
                    .dispatch_event(WindowEvent::PointerMoved { position: deny });
                app.window().dispatch_event(WindowEvent::PointerPressed {
                    position: deny,
                    button: PointerEventButton::Left,
                });
                app.window().dispatch_event(WindowEvent::PointerReleased {
                    position: deny,
                    button: PointerEventButton::Left,
                });
                assert_eq!(&*decisions.borrow(), &[("request-d".into(), false)]);
                app.hide().unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn timecode_input(app: &App, replacement: Option<&str>) -> (slint::LogicalPosition, String) {
        app.window().take_snapshot().unwrap();
        let root = WindowInner::from_pub(app.window())
            .window_item_rc()
            .unwrap();
        let mut inputs = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible()
                && let Some(input) = item.downcast::<TextInput>()
            {
                let text_property = TextInput::FIELD_OFFSETS
                    .text()
                    .apply_pin(input.as_pin_ref());
                let text = text_property.get();
                if text.split(':').count() == 4
                    && text.chars().all(|c| c.is_ascii_digit() || c == ':')
                {
                    if let Some(replacement) = replacement {
                        input.as_pin_ref().text.set(replacement.into());
                    }
                    let size = item.geometry().size;
                    let point =
                        item.map_to_window(LogicalPoint::new(size.width / 2., size.height / 2.));
                    inputs.push((
                        slint::LogicalPosition::new(point.x, point.y),
                        text_property.get().to_string(),
                    ));
                }
            }
            ControlFlow::Continue(())
        });
        assert_eq!(
            inputs.len(),
            1,
            "the real preview timecode must be uniquely visible"
        );
        inputs.pop().unwrap()
    }

    #[test]
    fn preview_timecode_commits_integer_frames_and_does_not_seek_back_on_refocus() {
        let _language = crate::i18n::language_lock();
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                slint::platform::set_platform(Box::new(NativeTestPlatform(window))).unwrap();
                let app = App::new().unwrap();
                crate::i18n::install(&app);
                app.set_phone(false);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(1100., 800.));
                app.global::<AgentUi>().set_sidebar_open(false);
                let fmt = app.global::<ui::Fmt>();
                fmt.on_parse_frames(|text, rate| crate::format::parse_frames(text.as_str(), rate));
                let weak = app.as_weak();
                app.global::<ui::Focus>().on_release(move || {
                    weak.upgrade().expect("test window is alive").invoke_blur();
                });
                let editor = app.global::<ui::Editor>();
                editor.set_frame_rate(24.0);
                editor.set_preview_duration(5.0);
                editor.set_playhead_free(true);
                editor.set_output_width(320);
                editor.set_output_height(180);
                editor.set_seats(slint::ModelRc::new(slint::VecModel::from(vec![
                    ui::SeatBox {
                        index: 0,
                        kind: ui::PaneKind::Preview,
                        x: 0.,
                        y: 0.,
                        width: 1000.,
                        height: 650.,
                    },
                ])));
                let requested = Rc::new(RefCell::new(Vec::new()));
                let received = Rc::clone(&requested);
                let weak = app.as_weak();
                editor.on_seek(move |seconds| {
                    received.borrow_mut().push(seconds);
                    // Publish the real control's seek result as the native editor
                    // does. No test copy of its formatter/parser is involved.
                    weak.upgrade()
                        .expect("test window is alive")
                        .global::<ui::Editor>()
                        .set_playhead(seconds);
                });
                app.show().unwrap();

                for (rate, count, expected) in [
                    (24.0, 0, "00:00:00:00"),
                    (24.0, 23, "00:00:00:23"),
                    (24.0, 24, "00:00:01:00"),
                    (24.0, 71, "00:00:02:23"),
                    (24.0, 72, "00:00:03:00"),
                    (24.0, 1440, "00:01:00:00"),
                    (24.0, 86400, "01:00:00:00"),
                    (25.0, 24, "00:00:00:24"),
                    (30.0, 29, "00:00:00:29"),
                    (50.0, 49, "00:00:00:49"),
                    (60.0, 59, "00:00:00:59"),
                    (24000.0 / 1001.0, 24, "00:00:01:00"),
                    (30000.0 / 1001.0, 30, "00:00:01:00"),
                ] {
                    let formatted = fmt.invoke_frame_count_timecode(count, rate);
                    assert_eq!(formatted, expected);
                    assert_eq!(
                        crate::format::parse_frames(formatted.as_str(), rate),
                        count as f32
                    );
                }
                for rate in [24.0, 25.0, 30.0, 50.0, 60.0] {
                    assert_eq!(fmt.invoke_frames_timecode(2.0, rate), "00:00:02:00");
                }

                let focus_input = || {
                    let (position, _) = timecode_input(&app, None);
                    app.window()
                        .dispatch_event(WindowEvent::PointerMoved { position });
                    app.window().dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                    app.window().dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                    assert!(
                        crate::typing(&app),
                        "the production number field did not gain focus"
                    );
                };
                for (rate, count, text) in [
                    (24.0, 23, "00:00:00:23"),
                    (24.0, 24, "00:00:01:00"),
                    (24.0, 71, "00:00:02:23"),
                    (24.0, 72, "00:00:03:00"),
                    (25.0, 24, "00:00:00:24"),
                    (30.0, 29, "00:00:00:29"),
                    (50.0, 49, "00:00:00:49"),
                    (60.0, 59, "00:00:00:59"),
                    (24000.0 / 1001.0, 23, "00:00:00:23"),
                    (30000.0 / 1001.0, 29, "00:00:00:29"),
                ] {
                    editor.set_frame_rate(rate);
                    editor.set_playhead(0.0);
                    focus_input();
                    assert_eq!(timecode_input(&app, Some(text)).1, text);
                    let before = requested.borrow().len();
                    app.window()
                        .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
                    app.window()
                        .dispatch_event(WindowEvent::KeyReleased { text: "\n".into() });
                    assert_eq!(
                        timecode_input(&app, None).1,
                        text,
                        "committed frame readout at {rate}"
                    );
                    assert!(!crate::typing(&app));
                    assert!(
                        requested.borrow().len() > before,
                        "Enter did not submit the production seek callback"
                    );
                    assert_eq!(editor.get_playhead(), count as f32 / rate);

                    for _ in 0..3 {
                        focus_input();
                        assert_eq!(timecode_input(&app, None).1, text);
                        let before = requested.borrow().len();
                        app.invoke_blur();
                        assert_eq!(
                            timecode_input(&app, None).1,
                            text,
                            "refocusing moved the displayed frame"
                        );
                        assert!(!crate::typing(&app));
                        assert!(
                            requested.borrow().len() > before,
                            "blur did not exercise the production commit path"
                        );
                        assert_eq!(
                            editor.get_playhead(),
                            count as f32 / rate,
                            "refocusing changed the seek target"
                        );
                    }
                }
                app.hide().unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    mod chat_transcript {
        use super::*;
        use slint::private_unstable_api::re_exports::ItemRc;
        use slint::private_unstable_api::re_exports::{
            AccessibleRole, AccessibleStringProperty, Flickable,
        };
        use std::cell::Cell;
        use std::time::Duration;

        struct ChatPlatform {
            window: Rc<MinimalSoftwareWindow>,
            clock: Rc<Cell<Duration>>,
        }

        impl Platform for ChatPlatform {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(self.window.clone())
            }

            fn duration_since_start(&self) -> Duration {
                self.clock.get()
            }
        }

        #[derive(Clone, Copy, Debug)]
        struct Bounds {
            x: f32,
            y: f32,
            width: f32,
            height: f32,
        }

        impl Bounds {
            fn center(self) -> slint::LogicalPosition {
                slint::LogicalPosition::new(self.x + self.width / 2., self.y + self.height / 2.)
            }

            fn bottom(self) -> f32 {
                self.y + self.height
            }

            #[cfg(not(target_os = "android"))]
            fn contains(self, position: slint::LogicalPosition) -> bool {
                position.x >= self.x
                    && position.x < self.x + self.width
                    && position.y >= self.y
                    && position.y < self.bottom()
            }
        }

        #[cfg(not(target_os = "android"))]
        mod tight_approval {
            slint::slint! {
                import { AgentChat, AgentUi } from "../../ui/agent.slint";
                import { ResizeBox } from "../../ui/primitives/resize-box.slint";
                import { I18n } from "../../ui/i18n.slint";
                export { AgentUi, I18n }
                export component TightApprovalWindow inherits Window {
                    in-out property <bool> phone;
                    in-out property <bool> fixed-only;
                    in-out property <length> fixed-request: 104px;
                    preferred-width: 320px; preferred-height: 600px;
                    min-width: 0px; min-height: 0px;
                    if !root.fixed-only: AgentChat {
                        width: root.width; height: root.height; phone: root.phone;
                    }
                    if root.fixed-only: Rectangle {
                        width: root.width; height: 60px;
                        VerticalLayout {
                            ResizeBox {
                                box-height: root.fixed-request;
                                accessible-role: groupbox;
                                accessible-label: "固定高度输入区域";
                            }
                        }
                    }
                }
            }
        }

        #[cfg(not(target_os = "android"))]
        mod phone_approval {
            slint::slint! {
                import { PhoneAgentSheet } from "../../ui/phone/agent-sheet.slint";
                import { AgentUi } from "../../ui/agent.slint";
                import { I18n } from "../../ui/i18n.slint";
                export { AgentUi, I18n }
                export component PhoneApprovalWindow inherits Window {
                    in-out property <length> keyboard-inset;
                    preferred-width: 320px; preferred-height: 640px;
                    min-width: 0px; min-height: 0px;
                    PhoneAgentSheet {
                        width: root.width; height: root.height;
                        keyboard-inset: root.keyboard-inset;
                    }
                }
            }
        }

        #[cfg(not(target_os = "android"))]
        #[derive(Clone, Copy, Debug)]
        struct ControlGeometry {
            bounds: Bounds,
            visible: Bounds,
            clip: Bounds,
        }

        #[cfg(not(target_os = "android"))]
        fn intersection(left: Bounds, right: Bounds) -> Bounds {
            let x = left.x.max(right.x);
            let y = left.y.max(right.y);
            Bounds {
                x,
                y,
                width: ((left.x + left.width).min(right.x + right.width) - x).max(0.),
                height: (left.bottom().min(right.bottom()) - y).max(0.),
            }
        }

        #[cfg(not(target_os = "android"))]
        fn control_geometry(
            window: &slint::Window,
            label: &str,
            role: AccessibleRole,
        ) -> ControlGeometry {
            let root = WindowInner::from_pub(window).window_item_rc().unwrap();
            let mut matches = Vec::new();
            root.visit_descendants::<()>(|item| {
                // Slint checks clipping/visibility from the root through every
                // ancestor, including still-mounted hidden panels.
                if item.is_visible()
                    && item.accessible_role() == role
                    && item
                        .accessible_string_property(AccessibleStringProperty::Label)
                        .is_some_and(|value| value == label)
                {
                    matches.push(item.clone());
                }
                ControlFlow::Continue(())
            });
            assert_eq!(matches.len(), 1, "one actual control: {label}");
            let item = &matches[0];
            let rect = item.geometry();
            let origin = item.map_to_window(rect.origin);
            let bounds = Bounds {
                x: origin.x,
                y: origin.y,
                width: rect.size.width,
                height: rect.size.height,
            };
            let size = window.size().to_logical(window.scale_factor());
            let mut clip = Bounds {
                x: 0.,
                y: 0.,
                width: size.width,
                height: size.height,
            };
            // Query the real clipping ancestors by their descendants, without
            // assuming any component nesting or treating partial visibility as full.
            root.visit_descendants::<()>(|ancestor| {
                if ancestor.borrow().as_ref().clips_children()
                    && ancestor
                        .visit_descendants(|child| {
                            if child == item {
                                ControlFlow::Break(())
                            } else {
                                ControlFlow::Continue(())
                            }
                        })
                        .is_some()
                {
                    let rect = ancestor.geometry();
                    let origin = ancestor.map_to_window(rect.origin);
                    clip = intersection(
                        clip,
                        Bounds {
                            x: origin.x,
                            y: origin.y,
                            width: rect.size.width,
                            height: rect.size.height,
                        },
                    );
                }
                ControlFlow::Continue(())
            });
            ControlGeometry {
                bounds,
                visible: intersection(bounds, clip),
                clip,
            }
        }

        #[cfg(not(target_os = "android"))]
        fn fully_visible_control(
            window: &slint::Window,
            label: &str,
            role: AccessibleRole,
        ) -> Bounds {
            let geometry = control_geometry(window, label, role);
            eprintln!("approval control {label}: {geometry:?}");
            assert!(
                geometry.bounds.width > 0.
                    && geometry.bounds.height > 0.
                    && (geometry.visible.width - geometry.bounds.width).abs() < 1.
                    && (geometry.visible.height - geometry.bounds.height).abs() < 1.,
                "the entire control must be inside all real clips and the window: {label}: {geometry:?}"
            );
            geometry.bounds
        }

        #[cfg(not(target_os = "android"))]
        fn render_window(window: &slint::Window) {
            for _ in 0..3 {
                slint::platform::update_timers_and_animations();
                window.take_snapshot().unwrap();
            }
        }

        #[cfg(not(target_os = "android"))]
        fn drag_composer(window: &slint::Window, delta_y: f32) {
            let box_bounds = fully_visible_control(
                window,
                crate::i18n::t("agent.inputRegion").as_str(),
                AccessibleRole::Groupbox,
            );
            // The production ResizeBox has a 16px corner grip. Its absolute
            // position follows the measured allocated box, never its preference.
            let position = slint::LogicalPosition::new(
                box_bounds.x + box_bounds.width - 8.,
                box_bounds.bottom() - 8.,
            );
            window.dispatch_event(WindowEvent::PointerMoved { position });
            window.dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            let position = slint::LogicalPosition::new(position.x, position.y + delta_y);
            window.dispatch_event(WindowEvent::PointerMoved { position });
            window.dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
            render_window(window);
        }

        #[cfg(not(target_os = "android"))]
        fn click_visible_button(window: &slint::Window, label: &str) {
            let position = fully_visible_control(window, label, AccessibleRole::Button).center();
            window.dispatch_event(WindowEvent::PointerMoved { position });
            window.dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            window.dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
            render_window(window);
        }

        #[cfg(not(target_os = "android"))]
        fn approval_controls(window: &slint::Window) -> [Bounds; 6] {
            render_window(window);
            let reject = fully_visible_control(
                window,
                crate::i18n::t("agent.reject").as_str(),
                AccessibleRole::Button,
            );
            let approve = fully_visible_control(
                window,
                crate::i18n::t("agent.approveOnce").as_str(),
                AccessibleRole::Button,
            );
            let stop = fully_visible_control(
                window,
                crate::i18n::t("agent.stop").as_str(),
                AccessibleRole::Button,
            );
            let input = fully_visible_control(
                window,
                crate::i18n::t("agent.inputRegion").as_str(),
                AccessibleRole::Groupbox,
            );
            let details = fully_visible_control(
                window,
                crate::i18n::t("agent.editDetails").as_str(),
                AccessibleRole::List,
            );
            let history = fully_visible_control(
                window,
                crate::i18n::t("agent.transcript").as_str(),
                AccessibleRole::List,
            );
            assert!(
                (reject.y - approve.y).abs() < 1. && reject.x + reject.width <= approve.x + 1.,
                "both decisions must occupy one non-overlapping row"
            );
            assert!(
                details.bottom() <= reject.y + 1.
                    && reject.bottom() <= input.y + 1.
                    && input.bottom() <= stop.y + 1.
                    && history.bottom() <= details.y + 1.,
                "details, decisions, input and stop must keep their separate usable areas"
            );
            assert!(
                input.height >= 55.9 && input.height <= 150.1,
                "allocated input respects the native minimum and user maximum: {input:?}"
            );
            assert!(
                history.height >= 39.9 && details.height > 0.,
                "history and scrollable details must both remain usable"
            );
            [reject, approve, stop, input, details, history]
        }

        #[cfg(not(target_os = "android"))]
        fn phone_approval_controls(window: &slint::Window) -> [Bounds; 5] {
            render_window(window);
            let reject = fully_visible_control(
                window,
                crate::i18n::t("agent.reject").as_str(),
                AccessibleRole::Button,
            );
            let approve = fully_visible_control(
                window,
                crate::i18n::t("agent.approveOnce").as_str(),
                AccessibleRole::Button,
            );
            let stop = fully_visible_control(
                window,
                crate::i18n::t("agent.stop").as_str(),
                AccessibleRole::Button,
            );
            let input = fully_visible_control(
                window,
                crate::i18n::t("agent.inputRegion").as_str(),
                AccessibleRole::Groupbox,
            );
            let content = fully_visible_control(
                window,
                crate::i18n::t("agent.transcript").as_str(),
                AccessibleRole::List,
            );
            assert!(reject.height >= 43.9 && approve.height >= 43.9 && stop.height >= 43.9);
            assert!((reject.y - approve.y).abs() < 1. && reject.x + reject.width <= approve.x + 1.);
            assert!(
                content.bottom() <= reject.y + 1.
                    && reject.bottom() <= input.y + 1.
                    && input.bottom() <= stop.y + 1.
            );
            assert!(input.height >= 55.9 && input.height <= 150.1);
            assert!(
                content.height >= 39.9,
                "the actual phone approval text must retain a readable, draggable viewport: {content:?}"
            );
            [reject, approve, stop, input, content]
        }

        #[derive(Clone, Copy, Debug)]
        struct ScrollView {
            bounds: Bounds,
            content_y: f32,
            content_height: f32,
        }

        struct Chat {
            app: App,
            messages: Vec<ui::AgentMessageData>,
            clock: Rc<Cell<Duration>>,
        }

        fn message(body: &str) -> ui::AgentMessageData {
            ui::AgentMessageData {
                role: ui::AgentMessageRole::Assistant,
                author: crate::i18n::t("agent.model").as_str().into(),
                body: body.into(),
                preview: body.into(),
                collapsible: false,
                pending: false,
            }
        }

        impl Chat {
            fn new() -> Self {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                let clock = Rc::new(Cell::new(Duration::from_secs(1)));
                slint::platform::set_platform(Box::new(ChatPlatform {
                    window,
                    clock: Rc::clone(&clock),
                }))
                .unwrap();
                let app = App::new().unwrap();
                crate::i18n::install(&app);
                let weak = app.as_weak();
                app.global::<ui::Focus>().on_release(move || {
                    if let Some(app) = weak.upgrade() {
                        app.invoke_blur();
                    }
                });
                app.set_phone(true);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(600., 900.));
                let agent = app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.can_send = true;
                status.needs_model_setup = false;
                status.model = "界面回归夹具".into();
                agent.set_status(status);
                agent.set_composer_mode(0);
                agent.set_tool_connected(true);
                let mut chat = Self {
                    app,
                    clock,
                    messages: (0..24)
                        .map(|index| {
                            message(&format!(
                                "历史消息 {index}\n这条已保存的内容用于真实聊天组件的布局回归。"
                            ))
                        })
                        .collect(),
                };
                chat.messages.push(message("初始最新消息"));
                chat.publish();
                chat.app.show().unwrap();
                // Entering a project resets the phone to manual editing in
                // a deferred on-start handler. Drain that transition before
                // explicitly opening the assistant, just as a user does.
                chat.render();
                chat.app.global::<AgentUi>().set_phone_sheet_state(2);
                chat.render();
                chat
            }

            fn publish(&self) {
                // Native AgentPane::publish replaces the message model when
                // a response changes. Exercise that same component boundary.
                self.app
                    .global::<AgentUi>()
                    .set_messages(slint::ModelRc::new(slint::VecModel::from(
                        self.messages.clone(),
                    )));
            }

            fn append(&mut self, body: &str) {
                self.messages.push(message(body));
                self.publish();
            }

            fn render(&self) {
                // Drain delayed initialization and layout notifications using
                // real bounded software frames, without sleeping or polling.
                for _ in 0..3 {
                    slint::platform::update_timers_and_animations();
                    self.app.window().take_snapshot().unwrap();
                }
            }

            fn advance(&self, milliseconds: u64) {
                self.clock
                    .set(self.clock.get() + Duration::from_millis(milliseconds));
                slint::platform::update_timers_and_animations();
            }

            fn scroll_view(&self) -> ScrollView {
                let root = WindowInner::from_pub(self.app.window())
                    .window_item_rc()
                    .unwrap();
                let mut views = Vec::new();
                root.visit_descendants::<()>(|item| {
                    if item.is_visible()
                        && item
                            .accessible_string_property(AccessibleStringProperty::Label)
                            .is_some_and(|label| {
                                label == crate::i18n::t("agent.transcript").as_str()
                            })
                        && let Some(flick) = item.downcast::<Flickable>()
                    {
                        let geometry = item.geometry();
                        let origin = item.map_to_window(geometry.origin);
                        let pin = flick.as_pin_ref();
                        views.push(ScrollView {
                            bounds: Bounds {
                                x: origin.x,
                                y: origin.y,
                                width: geometry.size.width,
                                height: geometry.size.height,
                            },
                            content_y: Flickable::FIELD_OFFSETS.content_y().apply_pin(pin).get().0,
                            content_height: Flickable::FIELD_OFFSETS
                                .content_height()
                                .apply_pin(pin)
                                .get()
                                .0,
                        });
                    }
                    ControlFlow::Continue(())
                });
                assert_eq!(views.len(), 1, "one visible production transcript");
                let view = views[0];
                assert!(
                    view.bounds.height >= 39.9,
                    "real transcript viewport: {view:?}"
                );
                view
            }

            fn text_bounds(&self, body: &str) -> Bounds {
                let root = WindowInner::from_pub(self.app.window())
                    .window_item_rc()
                    .unwrap();
                let mut found = Vec::new();
                root.visit_descendants::<()>(|item| {
                    if item.is_visible()
                        && item.downcast::<Flickable>().is_some()
                        && item
                            .accessible_string_property(AccessibleStringProperty::Label)
                            .is_some_and(|label| {
                                label == crate::i18n::t("agent.transcript").as_str()
                            })
                    {
                        item.visit_descendants::<()>(|entry| {
                            // Keep text clipped by the transcript: its actual
                            // geometry is what the off-screen assertion needs.
                            let matches =
                                entry.downcast::<SimpleText>().is_some_and(|text| {
                                    SimpleText::FIELD_OFFSETS
                                        .text()
                                        .apply_pin(text.as_pin_ref())
                                        .get()
                                        == body
                                }) || entry.downcast::<ComplexText>().is_some_and(|text| {
                                    ComplexText::FIELD_OFFSETS
                                        .text()
                                        .apply_pin(text.as_pin_ref())
                                        .get()
                                        == body
                                });
                            if matches {
                                let geometry = entry.geometry();
                                let origin = entry.map_to_window(geometry.origin);
                                found.push(Bounds {
                                    x: origin.x,
                                    y: origin.y,
                                    width: geometry.size.width,
                                    height: geometry.size.height,
                                });
                            }
                            ControlFlow::Continue(())
                        });
                    }
                    ControlFlow::Continue(())
                });
                assert_eq!(found.len(), 1, "one real message body: {body}");
                found[0]
            }

            #[cfg(not(target_os = "android"))]
            fn prepare_pending_layout(&mut self, request_id: &str) {
                self.desktop_layout();
                self.app
                    .window()
                    .set_size(slint::LogicalSize::new(1250., 850.));
                self.app.global::<AgentUi>().set_sidebar_width(320.);
                self.messages = (0..40)
                    .map(|index| message(&format!("历史单行 {index:02}")))
                    .collect();
                self.messages.push(message("审批之前的最新消息"));
                self.publish();
                let agent = self.app.global::<AgentUi>();
                agent.set_draft("保留最大输入偏好".into());
                let mut status = agent.get_status();
                status.missing = "".into();
                agent.set_status(status.clone());
                self.render();
                drag_composer(self.app.window(), 100.);
                let input = fully_visible_control(
                    self.app.window(),
                    crate::i18n::t("agent.inputRegion").as_str(),
                    AccessibleRole::Groupbox,
                );
                assert!(
                    (input.height - 150.).abs() < 1.,
                    "the real corner drag must establish the maximum preference: {input:?}"
                );
                status.busy = true;
                status.can_send = false;
                status.can_stop = true;
                status.title = "等待你批准当前工程的剪辑操作".into();
                status.detail = "先核对完整的修改说明，当前模型任务仍在等待。"
                    .repeat(8)
                    .into();
                status.has_progress = true;
                status.progress = 0.5;
                agent.set_status(status);
                agent.set_tool_request_id(request_id.into());
                agent.set_tool_summary(
                    "逐项核对字幕、轨道和时间范围，保留原工程素材。\n"
                        .repeat(40)
                        .into(),
                );
                agent.set_tool_pending(true);
                let call = self.app.global::<ui::AgentCallUi>();
                call.set_allowed(true);
                call.set_usage_policy(
                    "当前调用保持既有授权规则，服务商按实际使用计费。"
                        .repeat(8)
                        .into(),
                );
                self.render();
            }

            #[cfg(not(target_os = "android"))]
            fn read_single_line_history(&self) -> (String, Bounds) {
                self.wheel(450.);
                self.render();
                let view = self.scroll_view().bounds;
                // A 40px viewport can stop between whole text lines. Choose
                // the nearest real single-line body, then bring it fully into
                // view with user input before taking the reading measurement.
                let (body, bounds) = self
                    .messages
                    .iter()
                    .take(self.messages.len().saturating_sub(1))
                    .filter(|message| !message.collapsible)
                    .map(|message| {
                        (
                            message.body.to_string(),
                            self.text_bounds(message.body.as_str()),
                        )
                    })
                    .filter(|(_, bounds)| {
                        bounds.height > 0. && bounds.height < (view.height - 12.).min(30.)
                    })
                    .min_by(|(_, left), (_, right)| {
                        (left.y - view.y).abs().total_cmp(&(right.y - view.y).abs())
                    })
                    .expect("a real earlier single-line message must exist");
                self.wheel(view.y + 6. - bounds.y);
                self.render();
                let anchor = self.text_bounds(&body);
                assert!(
                    anchor.height < 30.
                        && (anchor.y - (view.y + 6.)).abs() < 1.
                        && anchor.bottom() <= view.bottom(),
                    "the real wheel must place a complete single-line anchor inside the viewport: anchor={anchor:?}, view={view:?}"
                );
                self.assert_latest_below_view();
                (body, anchor)
            }

            #[cfg(not(target_os = "android"))]
            fn wheel_details(&self, delta_y: f32) {
                use slint::winit_030::winit::dpi::PhysicalPosition;
                use slint::winit_030::winit::event::MouseScrollDelta;
                let position = fully_visible_control(
                    self.app.window(),
                    crate::i18n::t("agent.editDetails").as_str(),
                    AccessibleRole::List,
                )
                .center();
                let scale = f64::from(self.app.window().scale_factor());
                let before = self.app.global::<AgentUi>().get_native_scroll_up_pending();
                pause_transcript_follow_for_native_wheel(
                    &self.app,
                    self.app.window(),
                    &mut TranscriptScrollTarget::default(),
                    PhysicalPosition::new(
                        f64::from(position.x) * scale,
                        f64::from(position.y) * scale,
                    ),
                    &MouseScrollDelta::PixelDelta(PhysicalPosition::new(0., f64::from(delta_y))),
                    false,
                );
                assert_eq!(
                    self.app.global::<AgentUi>().get_native_scroll_up_pending(),
                    before,
                    "the native observer must not interpret detail scrolling as chat intent"
                );
                self.wheel_at(position, delta_y);
                self.render();
            }

            fn desktop_layout(&self) {
                self.app.set_phone(false);
                self.app
                    .window()
                    .set_size(slint::LogicalSize::new(1600., 900.));
                self.app.global::<AgentUi>().set_sidebar_open(true);
                self.render();
            }

            fn visible_history_anchor(&self) -> (String, Bounds) {
                self.render();
                let view = self.scroll_view();
                self.messages
                    .iter()
                    .take(self.messages.len().saturating_sub(1))
                    .filter(|message| !message.collapsible)
                    .find_map(|message| {
                        let bounds = self.text_bounds(message.body.as_str());
                        (bounds.y >= view.bounds.y && bounds.bottom() <= view.bounds.bottom())
                            .then(|| (message.body.to_string(), bounds))
                    })
                    .expect("a complete earlier message must be visible for the reading assertion")
            }

            fn assert_history_anchor(&self, body: &str, before: Bounds, transition: &str) {
                self.render();
                let after = self.text_bounds(body);
                let view = self.scroll_view();
                assert!(
                    (after.y - before.y).abs() < 1.,
                    "history text must retain its absolute screen position during {transition}: before={before:?}, after={after:?}, view={view:?}"
                );
                assert!(
                    after.y >= view.bounds.y - 1. && after.bottom() <= view.bounds.bottom() + 1.,
                    "the chosen history text must remain visible during {transition}: text={after:?}, view={view:?}"
                );
            }

            fn assert_latest_visible(&self) {
                self.render();
                let view = self.scroll_view();
                let bottom = (view.bounds.height - view.content_height).min(0.);
                assert!(
                    (view.content_y - bottom).abs() < 1.,
                    "transcript must reach its actual lower bound: {view:?}"
                );
                let text = self.text_bounds(self.messages.last().unwrap().body.as_str());
                assert!(
                    text.bottom() > view.bounds.y && text.bottom() <= view.bounds.bottom() + 1.,
                    "the latest rendered text must be inside the viewport: text={text:?}, view={view:?}"
                );
            }

            fn assert_latest_below_view(&self) {
                self.render();
                let view = self.scroll_view();
                let text = self.text_bounds(self.messages.last().unwrap().body.as_str());
                assert!(
                    text.bottom() > view.bounds.bottom() + 20.,
                    "reading history must not jump to the latest message: text={text:?}, view={view:?}"
                );
            }

            #[cfg(not(target_os = "android"))]
            fn matching_overlay_text_items(
                &self,
                body: &str,
                visible_only: bool,
            ) -> Vec<(ItemRc, Bounds)> {
                let root = WindowInner::from_pub(self.app.window())
                    .window_item_rc()
                    .unwrap();
                let mut found = Vec::new();
                root.visit_descendants::<()>(|item| {
                    let matches = item.downcast::<SimpleText>().is_some_and(|text| {
                        SimpleText::FIELD_OFFSETS
                            .text()
                            .apply_pin(text.as_pin_ref())
                            .get()
                            == body
                    }) || item.downcast::<ComplexText>().is_some_and(|text| {
                        ComplexText::FIELD_OFFSETS
                            .text()
                            .apply_pin(text.as_pin_ref())
                            .get()
                            == body
                    });
                    if matches && (!visible_only || item.is_visible()) {
                        let geometry = item.geometry();
                        let origin = item.map_to_window(geometry.origin);
                        found.push((
                            item.clone(),
                            Bounds {
                                x: origin.x,
                                y: origin.y,
                                width: geometry.size.width,
                                height: geometry.size.height,
                            },
                        ));
                    }
                    ControlFlow::Continue(())
                });
                found
            }

            #[cfg(not(target_os = "android"))]
            fn matching_overlay_text(&self, body: &str, visible_only: bool) -> Vec<Bounds> {
                self.matching_overlay_text_items(body, visible_only)
                    .into_iter()
                    .map(|(_, bounds)| bounds)
                    .collect()
            }

            #[cfg(not(target_os = "android"))]
            fn overlay_text_bounds(&self, body: &str, visible_only: bool) -> Bounds {
                let found = self.matching_overlay_text(body, visible_only);
                assert_eq!(found.len(), 1, "one actual overlay text item: {body}");
                found[0]
            }

            fn wheel_at(&self, position: slint::LogicalPosition, delta_y: f32) {
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerScrolled {
                        position,
                        delta_x: 0.,
                        delta_y,
                    });
            }

            fn wheel(&self, delta_y: f32) {
                self.wheel_at(self.scroll_view().bounds.center(), delta_y);
            }

            fn drag_transcript(&self, deltas_y: &[f32]) -> ScrollView {
                let mut position = self.scroll_view().bounds.center();
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                for delta_y in deltas_y {
                    self.advance(16);
                    position.y += delta_y;
                    self.app
                        .window()
                        .dispatch_event(WindowEvent::PointerMoved { position });
                    self.render();
                }
                let before_release = self.scroll_view();
                // Non-zero real drag velocity and a 16ms release interval
                // exercise Flickable::animate, including its no-move callback
                // when the content has already reached the lower bound.
                self.advance(16);
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                self.render();
                before_release
            }

            fn click(&self, label: &str) {
                self.click_role(label, AccessibleRole::Button);
            }

            fn click_role(&self, label: &str, role: AccessibleRole) {
                self.click_role_with_anchor(label, role, None);
            }

            fn click_script_picker(&self) {
                self.click_role_with_anchor(
                    crate::i18n::t("script.openWorkspace").as_str(),
                    AccessibleRole::Button,
                    Some(crate::i18n::t("script.closePicker").as_str()),
                );
            }

            fn click_role_with_anchor(
                &self,
                label: &str,
                role: AccessibleRole,
                anchor_label: Option<&str>,
            ) {
                self.render();
                let mut root = WindowInner::from_pub(self.app.window())
                    .window_item_rc()
                    .unwrap();
                if let Some(anchor_label) = anchor_label {
                    let mut anchors = Vec::new();
                    root.visit_descendants::<()>(|item| {
                        if item.is_visible()
                            && item.accessible_role() == AccessibleRole::Button
                            && item
                                .accessible_string_property(AccessibleStringProperty::Label)
                                .is_some_and(|value| value == anchor_label)
                        {
                            anchors.push(item.clone());
                        }
                        ControlFlow::Continue(())
                    });
                    assert_eq!(anchors.len(), 1, "one actual picker anchor: {anchor_label}");
                    let anchor = anchors.pop().unwrap();
                    let contains = |parent: &ItemRc, child: &ItemRc| {
                        parent
                            .visit_descendants(|item| {
                                if item == child {
                                    ControlFlow::Break(())
                                } else {
                                    ControlFlow::Continue(())
                                }
                            })
                            .is_some()
                    };
                    // The phone header also legitimately opens Writing. Use
                    // the nearest real container that owns the picker's close
                    // button and its own open button, never a label ordinal,
                    // guessed coordinate or replacement accessibility name.
                    let mut scope = root.clone();
                    root.visit_descendants::<()>(|candidate| {
                        if contains(candidate, &anchor)
                            && contains(&scope, candidate)
                            && candidate
                                .visit_descendants(|item| {
                                    if item.is_visible()
                                        && item.accessible_role() == role
                                        && item
                                            .accessible_string_property(
                                                AccessibleStringProperty::Label,
                                            )
                                            .is_some_and(|value| value == label)
                                    {
                                        ControlFlow::Break(())
                                    } else {
                                        ControlFlow::Continue(())
                                    }
                                })
                                .is_some()
                        {
                            scope = candidate.clone();
                        }
                        ControlFlow::Continue(())
                    });
                    assert!(
                        scope != root,
                        "the picker must have its own actual container"
                    );
                    root = scope;
                }
                let mut points = Vec::new();
                root.visit_descendants::<()>(|item| {
                    if item.is_visible()
                        && item.accessible_role() == role
                        && item
                            .accessible_string_property(AccessibleStringProperty::Label)
                            .is_some_and(|value| value == label)
                    {
                        let point = item.map_to_window(item.geometry().center());
                        points.push(slint::LogicalPosition::new(point.x, point.y));
                    }
                    ControlFlow::Continue(())
                });
                assert_eq!(points.len(), 1, "one real transcript control: {label}");
                let position = points[0];
                let size = self.app.window().size();
                let scale = self.app.window().scale_factor();
                assert!(
                    position.x >= 0.
                        && position.y >= 0.
                        && position.x < size.width as f32 / scale
                        && position.y < size.height as f32 / scale,
                    "control {label} is not inside the rendered window: {position:?}"
                );
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
            }

            fn enlarge_composer(&self) {
                self.app
                    .global::<AgentUi>()
                    .set_draft("调整输入框高度".into());
                self.render();
                let root = WindowInner::from_pub(self.app.window())
                    .window_item_rc()
                    .unwrap();
                let mut grips = Vec::new();
                root.visit_descendants::<()>(|item| {
                    if item.is_visible()
                        && item.downcast::<TextInput>().is_some()
                        && item
                            .accessible_string_property(AccessibleStringProperty::Label)
                            .is_some_and(|value| {
                                value == crate::i18n::t("agent.chatInput").as_str()
                            })
                    {
                        // ResizeBox's visible grip center is 2px past the
                        // input's right edge, at its padded bottom edge.
                        let geometry = item.geometry();
                        let point = item.map_to_window(LogicalPoint::new(
                            geometry.origin.x + geometry.size.width + 2.,
                            geometry.origin.y + geometry.size.height,
                        ));
                        grips.push(slint::LogicalPosition::new(point.x, point.y));
                    }
                    ControlFlow::Continue(())
                });
                assert_eq!(grips.len(), 1, "one production composer resize grip");
                let position = grips[0];
                let before = self.scroll_view().bounds.height;
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                let position = slint::LogicalPosition::new(position.x, position.y + 60.);
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                self.app
                    .window()
                    .dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                self.render();
                assert!(
                    self.scroll_view().bounds.height < before - 20.,
                    "the real resize gesture must actually shrink the transcript viewport"
                );
            }
        }

        fn with_chat(test: impl FnOnce(&mut Chat) + Send + 'static) {
            std::thread::Builder::new()
                .stack_size(32 << 20)
                .spawn(move || {
                    let mut chat = Chat::new();
                    test(&mut chat);
                    chat.app.hide().unwrap();
                })
                .unwrap()
                .join()
                .unwrap();
        }

        #[cfg(not(target_os = "android"))]
        mod phone_launcher {
            use super::*;

            fn bounds(chat: &Chat) -> Bounds {
                chat.render();
                fully_visible_control(
                    chat.app.window(),
                    crate::i18n::t("agent.phoneOpen").as_str(),
                    AccessibleRole::Button,
                )
            }

            fn assert_position(actual: Bounds, x: f32, y: f32) {
                assert!(
                    (actual.x - x).abs() < 1. && (actual.y - y).abs() < 1.,
                    "the actual launcher must stay at ({x}, {y}): {actual:?}"
                );
            }

            fn press(chat: &Chat) -> slint::LogicalPosition {
                let position = bounds(chat).center();
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                position
            }

            fn release(chat: &Chat, position: slint::LogicalPosition) {
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                chat.render();
            }

            fn drag_to(chat: &Chat, destination: slint::LogicalPosition) -> Bounds {
                let start = press(chat);
                // Consecutive native moves expose feedback errors when an
                // absolute pointer is mistakenly mixed with the moving chip.
                for step in 1..=4 {
                    let progress = step as f32 / 4.;
                    let position = slint::LogicalPosition::new(
                        start.x + (destination.x - start.x) * progress,
                        start.y + (destination.y - start.y) * progress,
                    );
                    chat.app
                        .window()
                        .dispatch_event(WindowEvent::PointerMoved { position });
                    chat.advance(16);
                    chat.render();
                }
                release(chat, destination);
                assert_eq!(
                    chat.app.global::<AgentUi>().get_phone_sheet_state(),
                    0,
                    "releasing a real launcher drag must not open the assistant"
                );
                bounds(chat)
            }

            fn text_color(chat: &Chat) -> slint::Brush {
                let root = WindowInner::from_pub(chat.app.window())
                    .window_item_rc()
                    .unwrap();
                let mut colors = Vec::new();
                root.visit_descendants::<()>(|item| {
                    if item.is_visible() {
                        if let Some(text) = item.downcast::<SimpleText>() {
                            let pin = text.as_pin_ref();
                            if SimpleText::FIELD_OFFSETS.text().apply_pin(pin).get() == "AI" {
                                colors.push(SimpleText::FIELD_OFFSETS.color().apply_pin(pin).get());
                            }
                        } else if let Some(text) = item.downcast::<ComplexText>() {
                            let pin = text.as_pin_ref();
                            if ComplexText::FIELD_OFFSETS.text().apply_pin(pin).get() == "AI" {
                                colors
                                    .push(ComplexText::FIELD_OFFSETS.color().apply_pin(pin).get());
                            }
                        }
                    }
                    ControlFlow::Continue(())
                });
                assert_eq!(colors.len(), 1, "one visible AI label in the real editor");
                colors.pop().unwrap()
            }

            #[test]
            fn defaults_left_follows_theme_and_keeps_a_free_position_after_opening() {
                let _language = crate::i18n::language_lock();
                with_chat(|chat| {
                    let app = chat.app.clone_strong();
                    let agent = app.global::<AgentUi>();
                    agent.set_phone_sheet_state(0);
                    let opened = Rc::new(Cell::new(0));
                    let observed = Rc::clone(&opened);
                    agent.on_assistant_open_requested(move || observed.set(observed.get() + 1));
                    let initial = bounds(chat);
                    assert!(
                        (initial.x - 12.).abs() < 1.,
                        "default is the left edge: {initial:?}"
                    );
                    assert!((initial.width - 52.).abs() < 1. && (initial.height - 52.).abs() < 1.);

                    let theme = app.global::<ui::Theme>();
                    for (dark, choice) in [(true, 0), (true, 2), (false, 2)] {
                        theme.set_dark(dark);
                        theme.set_accent_choice(choice);
                        chat.render();
                        assert_eq!(
                            text_color(chat),
                            slint::Brush::from(theme.get_accent()),
                            "the visible launcher must follow the live theme/accent selection"
                        );
                    }

                    let horizontal =
                        drag_to(chat, slint::LogicalPosition::new(215., initial.center().y));
                    assert_position(horizontal, 189., initial.y);
                    let vertical = drag_to(chat, slint::LogicalPosition::new(215., 245.));
                    assert_position(vertical, 189., 219.);
                    let placed = drag_to(chat, slint::LogicalPosition::new(325., 300.));
                    assert_position(placed, 299., 274.);
                    assert_eq!(opened.get(), 0);

                    // A small real tap jitter must still open exactly once,
                    // without replacing the user's free position.
                    let start = press(chat);
                    let position = slint::LogicalPosition::new(start.x + 3., start.y - 2.);
                    app.window()
                        .dispatch_event(WindowEvent::PointerMoved { position });
                    release(chat, position);
                    assert_eq!(agent.get_phone_sheet_state(), 1);
                    assert_eq!(opened.get(), 1);
                    chat.click(crate::i18n::t("agent.collapseAssistant").as_str());
                    assert_eq!(agent.get_phone_sheet_state(), 0);
                    assert_position(bounds(chat), placed.x, placed.y);
                    chat.click(crate::i18n::t("agent.phoneOpen").as_str());
                    assert_eq!(opened.get(), 2);
                    chat.click(crate::i18n::t("agent.collapseAssistant").as_str());
                    assert_position(bounds(chat), placed.x, placed.y);
                });
            }

            #[test]
            fn clamps_both_axes_and_cancels_pointer_or_resize_without_opening() {
                let _language = crate::i18n::language_lock();
                with_chat(|chat| {
                    let app = chat.app.clone_strong();
                    let agent = app.global::<AgentUi>();
                    agent.set_phone_sheet_state(0);
                    let opened = Rc::new(Cell::new(0));
                    let observed = Rc::clone(&opened);
                    agent.on_assistant_open_requested(move || observed.set(observed.get() + 1));
                    // Keep the finger inside the actual window while the
                    // chip's requested bounds cross its two outer edges.
                    let bottom_right = drag_to(chat, slint::LogicalPosition::new(599., 899.));
                    assert_position(bottom_right, 548., 848.);
                    app.window().set_size(slint::LogicalSize::new(320., 640.));
                    assert_position(bounds(chat), 268., 588.);
                    app.window().set_size(slint::LogicalSize::new(600., 900.));
                    assert_position(bounds(chat), bottom_right.x, bottom_right.y);
                    let top_left = drag_to(chat, slint::LogicalPosition::new(1., 1.));
                    assert_position(top_left, 0., 0.);

                    drag_to(chat, slint::LogicalPosition::new(200., 250.));
                    let start = press(chat);
                    let position = slint::LogicalPosition::new(start.x + 30., start.y + 30.);
                    app.window()
                        .dispatch_event(WindowEvent::PointerMoved { position });
                    chat.render();
                    app.window().dispatch_event(WindowEvent::PointerExited);
                    let cancelled = bounds(chat);
                    let position = slint::LogicalPosition::new(400., 450.);
                    app.window()
                        .dispatch_event(WindowEvent::PointerMoved { position });
                    release(chat, position);
                    assert_position(bounds(chat), cancelled.x, cancelled.y);
                    assert_eq!(opened.get(), 0, "cancel must not become a tap");

                    let start = press(chat);
                    app.window().set_size(slint::LogicalSize::new(600., 800.));
                    chat.render();
                    release(chat, start);
                    assert_eq!(agent.get_phone_sheet_state(), 0);
                    assert_eq!(opened.get(), 0, "resize must cancel the in-flight press");
                    chat.click(crate::i18n::t("agent.phoneOpen").as_str());
                    assert_eq!(agent.get_phone_sheet_state(), 1);
                    assert_eq!(
                        opened.get(),
                        1,
                        "a fresh tap after cancellation still works"
                    );
                });
            }
        }

        #[test]
        fn script_picker_opens_the_correct_native_workspace_and_preserves_the_draft() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                let app = chat.app.clone_strong();
                let agent = app.global::<AgentUi>();
                let opened = Rc::new(Cell::new(0));
                let observed = Rc::clone(&opened);
                app.global::<ui::ScriptUi>()
                    .on_open_requested(move || observed.set(observed.get() + 1));
                agent.set_draft("保留尚未发送的剪辑要求".into());
                agent.set_script_picker(true);
                chat.click_script_picker();
                chat.render();
                assert_eq!(opened.get(), 1);
                assert!(!agent.get_script_picker());
                assert!(agent.get_phone_script());
                assert_eq!(agent.get_phone_sheet_state(), 2);
                assert_eq!(agent.get_view(), ui::AgentView::Editor);
                assert_eq!(agent.get_draft(), "保留尚未发送的剪辑要求");
                chat.click(crate::i18n::t("agent.phoneChat").as_str());
                assert!(!agent.get_phone_script());
                assert_eq!(agent.get_draft(), "保留尚未发送的剪辑要求");

                app.set_phone(false);
                app.window().set_size(slint::LogicalSize::new(1400., 900.));
                agent.set_sidebar_open(true);
                agent.set_view(ui::AgentView::Chat);
                agent.set_script_picker(true);
                chat.click_script_picker();
                assert_eq!(opened.get(), 2);
                assert_eq!(agent.get_view(), ui::AgentView::Script);
                assert!(!agent.get_phone_script());
                assert_eq!(agent.get_draft(), "保留尚未发送的剪辑要求");
            });
        }

        #[test]
        fn phone_back_dismisses_transient_choices_after_the_header_restores_root_focus() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                let app = chat.app.clone_strong();
                let agent = app.global::<AgentUi>();
                let modes = Rc::new(RefCell::new(Vec::new()));
                let observed = Rc::clone(&modes);
                agent.on_tool_mode_requested(move |mode| observed.borrow_mut().push(mode));
                let key = |text: slint::SharedString| {
                    app.window()
                        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                    app.window()
                        .dispatch_event(WindowEvent::KeyReleased { text });
                };
                let back = || key(char::from(slint::platform::Key::Back).to_string().into());
                agent.set_phone_sheet_state(1);
                agent.set_script_picker(true);
                chat.click(crate::i18n::t("agent.phoneFull").as_str());
                assert_eq!(agent.get_phone_sheet_state(), 2);
                assert!(agent.get_script_picker());
                back();
                chat.render();
                assert!(!agent.get_script_picker());
                assert_eq!(agent.get_phone_sheet_state(), 2);

                agent.set_phone_sheet_state(1);
                chat.click_role(
                    crate::i18n::t("agent.permissionAsk").as_str(),
                    AccessibleRole::Combobox,
                );
                key(char::from(slint::platform::Key::End).to_string().into());
                key("\n".into());
                chat.render();
                assert!(agent.get_phone_confirm_full_access());
                assert_eq!(agent.get_tool_mode(), 0);
                assert!(modes.borrow().is_empty());
                chat.click(crate::i18n::t("agent.phoneFull").as_str());
                assert_eq!(agent.get_phone_sheet_state(), 2);
                back();
                chat.render();
                assert!(!agent.get_phone_confirm_full_access());
                assert_eq!(agent.get_phone_sheet_state(), 2);
                assert_eq!(agent.get_tool_mode(), 0);
                assert!(
                    modes.borrow().is_empty(),
                    "dismissing a permission explanation cannot grant authority"
                );
                back();
                assert_eq!(agent.get_phone_sheet_state(), 1);
                back();
                assert_eq!(agent.get_phone_sheet_state(), 0);
            });
        }

        #[test]
        fn chat_follows_streamed_content_and_viewport_reflow() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.assert_latest_visible();
                let app = chat.app.clone_strong();
                let agent = app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.busy = true;
                status.can_stop = true;
                status.title = "正在生成回复".into();
                status.detail = "真实状态栏改变聊天可见高度。".repeat(8).into();
                agent.set_status(status.clone());
                chat.append("状态栏出现后的最新请求");
                chat.assert_latest_visible();
                chat.messages.last_mut().unwrap().body = "流式内容增加一行。\n".repeat(8).into();
                chat.publish();
                chat.assert_latest_visible();
                status.busy = false;
                status.recovery_summary = "恢复核对说明占据聊天上方的空间。".repeat(8).into();
                status.can_review = true;
                agent.set_status(status);
                chat.assert_latest_visible();
                agent.set_tool_request_id("transcript-layout-approval".into());
                agent.set_tool_summary("待批准的原生操作布局说明。".repeat(20).into());
                agent.set_tool_pending(true);
                chat.assert_latest_visible();
                agent.set_tool_pending(false);
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(600., 1000.));
                chat.assert_latest_visible();
                chat.enlarge_composer();
                chat.assert_latest_visible();
            });
        }

        #[test]
        fn chat_keeps_manual_history_position_during_queued_updates() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.assert_latest_visible();
                // Queue reflow and a model replacement, then send real input
                // before rendering either. Old notifications must not re-arm.
                let agent = chat.app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.busy = true;
                status.title = "请求仍在进行".into();
                status.detail = "待处理状态变化。".repeat(8).into();
                agent.set_status(status);
                chat.append("已排队但尚未渲染的新内容");
                chat.wheel(360.);
                chat.render();
                let reading = chat.scroll_view().content_y;
                chat.assert_latest_below_view();
                chat.append("阅读历史期间继续到达的新内容");
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(600., 820.));
                chat.render();
                assert!(
                    (chat.scroll_view().content_y - reading).abs() < 1.,
                    "stream and viewport shrink must preserve the chosen history offset"
                );
                chat.assert_latest_below_view();
            });
        }

        #[cfg(not(target_os = "android"))]
        #[test]
        fn chat_pending_approval_stays_reachable_when_space_shrinks_and_restores() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.prepare_pending_layout("pending-resize-identity");
                let large = approval_controls(chat.app.window());
                let (anchor, before) = chat.read_single_line_history();
                for (width, height) in [(900., 560.), (1250., 850.)] {
                    chat.app
                        .window()
                        .set_size(slint::LogicalSize::new(width, height));
                    let controls = approval_controls(chat.app.window());
                    let actual = chat
                        .app
                        .window()
                        .size()
                        .to_logical(chat.app.window().scale_factor());
                    assert!(
                        (actual.width - width).abs() < 1. && (actual.height - height).abs() < 1.,
                        "layout must fit the requested real window, not enlarge it: {actual:?}"
                    );
                    assert!(
                        (controls[5].width - large[5].width).abs() < 1.,
                        "fixed sidebar width must exclude normal wrapping from the anchor check"
                    );
                    chat.assert_history_anchor(&anchor, before, "pending approval window resize");
                }
                let restored = approval_controls(chat.app.window());
                assert!(
                    (restored[3].height - large[3].height).abs() < 1.,
                    "restoring available space must restore the user's input preference"
                );
                chat.append("缩放后到达的消息不能覆盖历史阅读意图");
                chat.assert_history_anchor(&anchor, before, "new message after resizing");
                chat.assert_latest_below_view();
            });

            // Directly allocate the production AgentChat less space than the
            // desktop shell. This proves the input really yields, even when
            // the 900px desktop can satisfy its whole 150px preference.
            std::thread::Builder::new().stack_size(32 << 20).spawn(|| {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                let clock = Rc::new(Cell::new(Duration::from_secs(1)));
                slint::platform::set_platform(Box::new(ChatPlatform { window, clock })).unwrap();
                let app = tight_approval::TightApprovalWindow::new().unwrap();
                    let words = app.global::<tight_approval::I18n>();
                    words.on_lookup(|_, key| crate::i18n::t(&key).into());
                    words.on_lookup1(|_, key, a| crate::i18n::tf(&key, &[&a]).into());
                    words.on_lookup2(|_, key, a, b| crate::i18n::tf(&key, &[&a, &b]).into());
                    words.set_lang(crate::i18n::current().into());
                app.window().set_size(slint::LogicalSize::new(320., 600.));
                let agent = app.global::<tight_approval::AgentUi>();
                agent.set_draft("收缩后继续拖动".into());
                let mut status = agent.get_status();
                status.can_send = true;
                status.needs_model_setup = false;
                status.missing = "".into();
                agent.set_status(status.clone());
                app.show().unwrap();
                render_window(app.window());
                drag_composer(app.window(), 100.);
                let requested = fully_visible_control(app.window(), crate::i18n::t("agent.inputRegion").as_str(), AccessibleRole::Groupbox);
                assert!((requested.height - 150.).abs() < 1.);
                status.busy = true;
                status.can_send = false;
                status.can_stop = true;
                status.title = "等待批准".into();
                status.detail = "空间不足时完整说明通过详情滚动阅读。".repeat(20).into();
                agent.set_status(status);
                agent.set_tool_request_id("tight-pending-identity".into());
                agent.set_tool_summary("完整剪辑摘要不能挤走必要操作。\n".repeat(40).into());
                agent.set_tool_pending(true);
                app.window().set_size(slint::LogicalSize::new(320., 300.));
                let compressed = approval_controls(app.window())[3];
                assert!(compressed.height < requested.height - 1.,
                    "the real input must shrink below its untouched preference: {compressed:?}");
                let actual = app.window().size().to_logical(app.window().scale_factor());
                assert!((actual.height - 300.).abs() < 1.);
                app.window().set_size(slint::LogicalSize::new(320., 600.));
                assert!((approval_controls(app.window())[3].height - requested.height).abs() < 1.);

                // A fresh drag starts at the allocated height, so shrinking
                // the grip does not first consume invisible preferred space.
                app.window().set_size(slint::LogicalSize::new(320., 300.));
                let allocated = approval_controls(app.window())[3].height;
                drag_composer(app.window(), -10.);
                app.window().set_size(slint::LogicalSize::new(320., 600.));
                let after_drag = approval_controls(app.window())[3].height;
                assert!((after_drag - (allocated - 10.).max(56.)).abs() < 1.,
                    "a new gesture must start at the actual visible height: allocated={allocated}, restored={after_drag}");

                // The same shared production component, without opting into
                // shrink, must retain native fixed-height behavior in a tight parent.
                app.set_fixed_only(true);
                for requested in [104., 220.] {
                    app.set_fixed_request(requested);
                    render_window(app.window());
                    let fixed = fully_visible_control(
                        app.window(), "固定高度输入区域", AccessibleRole::Groupbox,
                    );
                    assert!((fixed.height - requested).abs() < 1.,
                        "default native input remains fixed despite its 60px parent: requested={requested}, actual={fixed:?}");
                }
                app.hide().unwrap();
            }).unwrap().join().unwrap();
        }

        #[cfg(not(target_os = "android"))]
        #[test]
        fn phone_sheet_keyboard_keeps_native_approval_and_input_fully_reachable() {
            let _language = crate::i18n::language_lock();
            std::thread::Builder::new()
                .stack_size(32 << 20)
                .spawn(|| {
                    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                    let clock = Rc::new(Cell::new(Duration::from_secs(1)));
                    slint::platform::set_platform(Box::new(ChatPlatform { window, clock })).unwrap();
                    let app = phone_approval::PhoneApprovalWindow::new().unwrap();
                    let words = app.global::<phone_approval::I18n>();
                    words.on_lookup(|_, key| crate::i18n::t(&key).into());
                    words.on_lookup1(|_, key, a| crate::i18n::tf(&key, &[&a]).into());
                    words.on_lookup2(|_, key, a, b| crate::i18n::tf(&key, &[&a, &b]).into());
                    words.set_lang(crate::i18n::current().into());
                    app.window().set_size(slint::LogicalSize::new(320., 640.));
                    let agent = app.global::<phone_approval::AgentUi>();
                    let reply_languages = crate::i18n::language_choice_labels();
                    let reply_language = reply_languages[0].clone();
                    agent.set_reply_languages(slint::ModelRc::new(slint::VecModel::from(reply_languages)));
                    agent.set_reply_language_index(0);
                    agent.set_phone_sheet_state(2);
                    agent.set_draft("保留输入高度偏好和未发送草稿".into());
                    agent.set_models(slint::ModelRc::new(slint::VecModel::from(vec![
                        slint::SharedString::from("真实模型选择控件"),
                    ])));
                    let mut status = agent.get_status();
                    status.missing = "".into();
                    status.needs_model_setup = false;
                    status.can_send = true;
                    status.title = "就绪".into();
                    status.detail = "".into();
                    agent.set_status(status.clone());
                    app.show().unwrap();

                    // An empty conversation exposes an inherited transcript
                    // height cap that a long approval summary would conceal.
                    // Measure the production surface's clipping edge rather
                    // than reproducing its half/full-height calculation.
                    let assert_sheet_edges = |level, available_bottom: f32, action: &str| {
                        render_window(app.window());
                        let label = crate::i18n::t(if level == 1 { "agent.phoneFull" } else { "agent.phoneHalf" });
                        let header = control_geometry(app.window(), &label, AccessibleRole::Button);
                        fully_visible_control(app.window(), &label, AccessibleRole::Button);
                        assert!((header.bounds.y - header.clip.y).abs() < 1.,
                            "the title bar must start at the actual sheet surface top: {header:?}");
                        let close = fully_visible_control(app.window(), crate::i18n::t("agent.collapseAssistant").as_str(), AccessibleRole::Button);
                        let inside_header = intersection(close, header.bounds);
                        assert!(close.width >= 43.9 && close.height >= 43.9
                            && (inside_header.width - close.width).abs() < 1.
                            && (inside_header.height - close.height).abs() < 1.,
                            "the full collapse target must remain inside the top title bar: {close:?}, {header:?}");
                        let content = fully_visible_control(app.window(), crate::i18n::t("agent.transcript").as_str(), AccessibleRole::List);
                        assert!(header.bounds.bottom() <= content.y + 1.,
                            "the transcript must start below the title bar, never cover it: {header:?}, {content:?}");
                        let input = fully_visible_control(app.window(), crate::i18n::t("agent.inputRegion").as_str(), AccessibleRole::Groupbox);
                        let action = fully_visible_control(app.window(), action, AccessibleRole::Button);
                        let gap = available_bottom - action.bottom();
                        assert!(action.height >= 43.9 && (-1.0..=16.0).contains(&gap),
                            "the action row must stay at the available bottom with only its padding: bottom={available_bottom}, action={action:?}");
                        let language = fully_visible_control(app.window(), reply_language.as_str(), AccessibleRole::Combobox);
                        assert!(input.bottom() <= language.y + 1. && language.y - input.bottom() <= 8.
                            && language.bottom() <= action.y + 1. && action.y - language.bottom() <= 8.,
                            "input, reply language and action must remain adjacent and fully reachable: input={input:?}, language={language:?}, action={action:?}");
                    };
                    for height in [640., 800.] {
                        app.window().set_size(slint::LogicalSize::new(320., height));
                        for level in [1, 2] {
                            for inset in [0., height - 320.] {
                                agent.set_phone_sheet_state(level);
                                app.set_keyboard_inset(inset);
                                assert_sheet_edges(level, height - inset, crate::i18n::t("agent.send").as_str());
                                // Real pointer dispatch also detects a header
                                // whose bounds are correct but another item
                                // intercepts its title or collapse button.
                                let height_label = crate::i18n::t(if level == 1 {
                                    "agent.phoneFull"
                                } else {
                                    "agent.phoneHalf"
                                });
                                click_visible_button(app.window(), &height_label);
                                assert_eq!(agent.get_phone_sheet_state(), 3 - level,
                                    "clicking the actual title bar must change sheet height");
                                click_visible_button(app.window(), crate::i18n::t("agent.collapseAssistant").as_str());
                                assert_eq!(agent.get_phone_sheet_state(), 0,
                                    "the actual header collapse button must close the sheet");
                                assert_eq!(agent.get_draft(), "保留输入高度偏好和未发送草稿");
                            }
                        }
                    }

                    app.window().set_size(slint::LogicalSize::new(320., 640.));
                    app.set_keyboard_inset(0.);
                    agent.set_phone_sheet_state(2);
                    status.busy = true;
                    status.can_send = false;
                    status.can_stop = true;
                    status.title = "等待批准".into();
                    agent.set_status(status);
                    agent.set_tool_connected(true);
                    agent.set_tool_request_id("phone-keyboard-request".into());
                    agent.set_tool_summary("逐项核对当前工程的真实操作摘要。\n".repeat(40).into());
                    agent.set_tool_pending(true);
                    let decisions = Rc::new(RefCell::new(Vec::new()));
                    let observed = Rc::clone(&decisions);
                    agent.on_tool_decision_requested(move |id, approved| {
                        observed.borrow_mut().push((id.to_string(), approved));
                    });
                    render_window(app.window());
                    drag_composer(app.window(), 100.);
                    let preferred = phone_approval_controls(app.window())[3].height;
                    assert!((preferred - 150.).abs() < 1.);
                    for level in [1, 2] {
                        agent.set_phone_sheet_state(level);
                        for inset in [0., 320.] {
                            app.set_keyboard_inset(inset);
                            let controls = phone_approval_controls(app.window());
                            assert_sheet_edges(level, 640. - inset, crate::i18n::t("agent.stop").as_str());
                            assert!(controls[2].bottom() <= 640. - inset + 1.,
                                "the real sheet header, clipping and keyboard inset must keep stop above the keyboard");
                            if inset == 320. {
                                assert!(controls[3].height < preferred - 1.,
                                    "the input preference must yield before the real approval reading viewport disappears");
                            }
                            assert_eq!(agent.get_draft(), "保留输入高度偏好和未发送草稿");
                            assert!(decisions.borrow().is_empty());
                        }
                    }
                    click_visible_button(app.window(), crate::i18n::t("agent.reject").as_str());
                    assert_eq!(&*decisions.borrow(), &[("phone-keyboard-request".to_owned(), false)]);
                    app.set_keyboard_inset(0.);
                    assert!((phone_approval_controls(app.window())[3].height - preferred).abs() < 1.,
                        "closing the keyboard restores the person's actual input height preference");
                    app.hide().unwrap();
                })
                .unwrap()
                .join()
                .unwrap();
        }

        #[cfg(not(target_os = "android"))]
        #[test]
        fn chat_phone_approval_details_drag_without_deciding_or_moving_controls() {
            let _language = crate::i18n::language_lock();
            std::thread::Builder::new()
                .stack_size(32 << 20)
                .spawn(|| {
                    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                    let clock = Rc::new(Cell::new(Duration::from_secs(1)));
                    slint::platform::set_platform(Box::new(ChatPlatform { window, clock }))
                        .unwrap();
                    let app = tight_approval::TightApprovalWindow::new().unwrap();
                    let words = app.global::<tight_approval::I18n>();
                    words.on_lookup(|_, key| crate::i18n::t(&key).into());
                    words.on_lookup1(|_, key, a| crate::i18n::tf(&key, &[&a]).into());
                    words.on_lookup2(|_, key, a, b| crate::i18n::tf(&key, &[&a, &b]).into());
                    words.set_lang(crate::i18n::current().into());
                    app.set_phone(true);
                    app.window().set_size(slint::LogicalSize::new(320., 480.));
                    let agent = app.global::<tight_approval::AgentUi>();
                    let mut status = agent.get_status();
                    status.missing = "".into();
                    status.needs_model_setup = false;
                    status.busy = true;
                    status.can_send = false;
                    status.can_stop = true;
                    status.title = "等待批准".into();
                    agent.set_status(status);
                    agent.set_tool_request_id("phone-drag-approval".into());
                    agent.set_tool_summary(
                        "完整剪辑摘要必须允许在手机上逐项滑动阅读。\n"
                            .repeat(40)
                            .into(),
                    );
                    agent.set_tool_pending(true);
                    let decisions = Rc::new(RefCell::new(Vec::new()));
                    let observed = Rc::clone(&decisions);
                    agent.on_tool_decision_requested(move |id, approved| {
                        observed.borrow_mut().push((id.to_string(), approved));
                    });
                    let stops = Rc::new(Cell::new(0));
                    let observed = Rc::clone(&stops);
                    agent.on_stop_requested(move || observed.set(observed.get() + 1));
                    app.show().unwrap();
                    let controls = phone_approval_controls(app.window());
                    let title_y = || {
                        let root = WindowInner::from_pub(app.window())
                            .window_item_rc()
                            .unwrap();
                        let mut positions = Vec::new();
                        root.visit_descendants::<()>(|item| {
                            // Include clipped text: its real position proves that
                            // the production detail body moved under the viewport.
                            let matches =
                                item.downcast::<SimpleText>().is_some_and(|text| {
                                    SimpleText::FIELD_OFFSETS
                                        .text()
                                        .apply_pin(text.as_pin_ref())
                                        .get()
                                        == crate::i18n::t("agent.approveQuestion").as_str()
                                }) || item.downcast::<ComplexText>().is_some_and(|text| {
                                    ComplexText::FIELD_OFFSETS
                                        .text()
                                        .apply_pin(text.as_pin_ref())
                                        .get()
                                        == crate::i18n::t("agent.approveQuestion").as_str()
                                });
                            if matches {
                                positions.push(item.map_to_window(item.geometry().origin).y);
                            }
                            ControlFlow::Continue(())
                        });
                        assert_eq!(positions.len(), 1, "one real approval title");
                        positions[0]
                    };
                    let details = controls[4];
                    // A new phone request follows the combined content to its
                    // end. Read back to the real detail title before testing a
                    // finger drag on the detail body, not on empty padding.
                    app.window().dispatch_event(WindowEvent::PointerScrolled {
                        position: details.center(),
                        delta_x: 0.,
                        delta_y: 100_000.,
                    });
                    render_window(app.window());
                    let before = title_y();
                    assert!(before >= details.y && before + 40. < details.bottom());
                    let start =
                        slint::LogicalPosition::new(details.x + details.width / 2., before + 40.);
                    app.window()
                        .dispatch_event(WindowEvent::PointerMoved { position: start });
                    app.window().dispatch_event(WindowEvent::PointerPressed {
                        position: start,
                        button: PointerEventButton::Left,
                    });
                    for delta in [25., 50., 75.] {
                        app.window().dispatch_event(WindowEvent::PointerMoved {
                            position: slint::LogicalPosition::new(start.x, start.y - delta),
                        });
                        render_window(app.window());
                    }
                    app.window().dispatch_event(WindowEvent::PointerReleased {
                        position: slint::LogicalPosition::new(start.x, start.y - 75.),
                        button: PointerEventButton::Left,
                    });
                    render_window(app.window());
                    assert!(
                        title_y() < before - 50.,
                        "phone pointer dragging must scroll the actual long approval details"
                    );
                    assert!(
                        decisions.borrow().is_empty() && stops.get() == 0,
                        "reading the details must not approve, reject or stop the task"
                    );
                    let after = phone_approval_controls(app.window());
                    for index in 0..3 {
                        assert!(
                            (after[index].x - controls[index].x).abs() < 1.
                                && (after[index].y - controls[index].y).abs() < 1.,
                            "reading must leave the visible decisions and stop in place"
                        );
                    }
                    click_visible_button(app.window(), crate::i18n::t("agent.reject").as_str());
                    click_visible_button(
                        app.window(),
                        crate::i18n::t("agent.approveOnce").as_str(),
                    );
                    click_visible_button(app.window(), crate::i18n::t("agent.stop").as_str());
                    assert_eq!(
                        *decisions.borrow(),
                        vec![
                            ("phone-drag-approval".to_owned(), false),
                            ("phone-drag-approval".to_owned(), true),
                        ],
                        "visible decision controls must retain the displayed request identity"
                    );
                    assert_eq!(
                        stops.get(),
                        1,
                        "the visible stop must still fire exactly once"
                    );
                    app.hide().unwrap();
                })
                .unwrap()
                .join()
                .unwrap();
        }

        #[cfg(not(target_os = "android"))]
        #[test]
        fn chat_pending_details_scroll_without_moving_history_and_pointer_actions_fire_once() {
            let _language = crate::i18n::language_lock();
            for decision in [Some(true), Some(false), None] {
                with_chat(move |chat| {
                    chat.prepare_pending_layout("pending-details-initial");
                    chat.app
                        .window()
                        .set_size(slint::LogicalSize::new(900., 560.));
                    approval_controls(chat.app.window());
                    // Upward input inside details must not pause an otherwise
                    // following transcript, including the raw native observer.
                    chat.wheel_details(60.);
                    chat.append("详情滚轮不暂停底部跟随");
                    chat.assert_latest_visible();

                    let (anchor, before) = chat.read_single_line_history();
                    let controls = approval_controls(chat.app.window());
                    let title = chat.overlay_text_bounds(
                        crate::i18n::t("agent.approveQuestion").as_str(),
                        false,
                    );
                    let reading_y = chat.scroll_view().content_y;
                    chat.wheel_details(-180.);
                    let scrolled_title = chat.overlay_text_bounds(
                        crate::i18n::t("agent.approveQuestion").as_str(),
                        false,
                    );
                    assert!(
                        scrolled_title.y < title.y - 20.,
                        "the actual detail content must scroll: before={title:?}, after={scrolled_title:?}"
                    );
                    chat.wheel_details(60.);
                    chat.assert_history_anchor(&anchor, before, "scrolling long approval details");
                    assert!((chat.scroll_view().content_y - reading_y).abs() < 1.);
                    let after_scroll = approval_controls(chat.app.window());
                    for index in 0..3 {
                        assert!(
                            (after_scroll[index].x - controls[index].x).abs() < 1.
                                && (after_scroll[index].y - controls[index].y).abs() < 1.,
                            "decision and stop controls must stay fixed outside scrolled details"
                        );
                    }
                    chat.append("详情滚动后继续更新也不能恢复自动跟随");
                    chat.assert_history_anchor(&anchor, before, "streaming after detail scrolling");
                    chat.assert_latest_below_view();

                    let app = chat.app.clone_strong();
                    let agent = app.global::<AgentUi>();
                    let request_id = match decision {
                        Some(true) => "pending-details-approve",
                        Some(false) => "pending-details-reject",
                        None => "pending-details-stop",
                    };
                    agent.set_tool_request_id(request_id.into());
                    chat.render();
                    let reset_title = chat.overlay_text_bounds(
                        crate::i18n::t("agent.approveQuestion").as_str(),
                        false,
                    );
                    assert!(
                        (reset_title.y - title.y).abs() < 1.,
                        "a new request identity must reset only the detail scroller"
                    );
                    chat.assert_history_anchor(&anchor, before, "new approval identity");
                    approval_controls(chat.app.window());

                    let decisions = Rc::new(RefCell::new(Vec::new()));
                    let observed = Rc::clone(&decisions);
                    agent.on_tool_decision_requested(move |id, approved| {
                        observed.borrow_mut().push((id.to_string(), approved));
                    });
                    let stops = Rc::new(Cell::new(0));
                    let observed = Rc::clone(&stops);
                    agent.on_stop_requested(move || observed.set(observed.get() + 1));
                    let label = crate::i18n::t(match decision {
                        Some(true) => "agent.approveOnce",
                        Some(false) => "agent.reject",
                        None => "agent.stop",
                    });
                    click_visible_button(chat.app.window(), &label);
                    chat.advance(32);
                    chat.render();
                    if let Some(approved) = decision {
                        assert_eq!(*decisions.borrow(), vec![(request_id.to_owned(), approved)]);
                        assert_eq!(stops.get(), 0, "a decision must not invoke stop");
                    } else {
                        assert_eq!(
                            stops.get(),
                            1,
                            "the real stop pointer must invoke its callback once"
                        );
                        assert!(
                            decisions.borrow().is_empty(),
                            "stop must not decide an approval"
                        );
                    }
                });
            }
        }

        #[test]
        fn chat_desktop_sidebar_reflows_with_window_and_reopens_within_native_layout() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                // Capture the native workspace's real layout callback rather
                // than deriving editor width from the assistant's preference.
                let workspace = Rc::new(Cell::new((0_f32, 0_f32)));
                let measured = Rc::clone(&workspace);
                chat.app
                    .global::<ui::Editor>()
                    .on_workspace_resized(move |width, height| measured.set((width, height)));
                chat.desktop_layout();
                let app = chat.app.clone_strong();
                let agent = app.global::<AgentUi>();
                let check_layout = |chat: &Chat, maximum_sidebar: f32, minimum_editor: f32| {
                    chat.render();
                    assert!(
                        agent.get_sidebar_open(),
                        "this check requires a visibly open sidebar"
                    );
                    let view = chat.scroll_view();
                    let size = chat
                        .app
                        .window()
                        .size()
                        .to_logical(chat.app.window().scale_factor());
                    let (editor_width, editor_height) = workspace.get();
                    assert!(
                        agent.get_sidebar_width() <= maximum_sidebar + 1.
                            && view.bounds.width <= maximum_sidebar + 1.,
                        "requested and rendered assistant widths must both be bounded: requested={}, view={view:?}",
                        agent.get_sidebar_width()
                    );
                    assert!(
                        editor_width >= minimum_editor && editor_height > 0.,
                        "the actual native editor must retain its usable area: editor=({editor_width}, {editor_height}), view={view:?}"
                    );
                    assert!(
                        view.bounds.x >= editor_width
                            && view.bounds.y >= 0.
                            && view.bounds.x + view.bounds.width <= size.width + 1.
                            && view.bounds.bottom() <= size.height + 1.,
                        "the rendered conversation must not overlap the editor or escape the window: view={view:?}, editor={editor_width}, window={size:?}"
                    );
                };

                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(2560., 900.));
                agent.set_sidebar_width(840.);
                check_layout(chat, 840., 600.);
                assert!(
                    chat.scroll_view().bounds.width > 700.,
                    "the fixture must really start wide"
                );

                // The old code only bounded gutter drags, so its 840px
                // preference survived this resize and consumed the editor.
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(1250., 900.));
                check_layout(chat, 575., 600.);
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(2560., 900.));
                check_layout(chat, 840., 600.);
                let open_editor_width = workspace.get().0;
                chat.click(crate::i18n::t("agent.collapseAssistant").as_str());
                chat.render();
                assert!(!agent.get_sidebar_open());
                assert!(
                    workspace.get().0 > open_editor_width,
                    "hiding the sidebar returns real space to the editor"
                );
                chat.click(crate::i18n::t("agent.expandAssistant").as_str());
                check_layout(chat, 840., 600.);

                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(900., 560.));
                // The minimum desktop supports the compact native workspace;
                // intentionally reopening the assistant must still fit it.
                check_layout(chat, 320., 550.);
                chat.click(crate::i18n::t("agent.collapseAssistant").as_str());
                chat.render();
                chat.click(crate::i18n::t("agent.expandAssistant").as_str());
                check_layout(chat, 320., 550.);
            });
        }

        #[test]
        fn chat_history_screen_anchor_survives_task_status_and_progress_changes() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.desktop_layout();
                let app = chat.app.clone_strong();
                let agent = app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.missing = "".into();
                status.title = crate::i18n::t("agent.idle").as_str().into();
                status.detail = "".into();
                agent.set_status(status.clone());
                chat.assert_latest_visible();
                chat.wheel(360.);
                chat.render();
                chat.assert_latest_below_view();
                let (anchor, before) = chat.visible_history_anchor();

                // A fixed content-y is insufficient: a disappearing status
                // above the viewport can move every visible history line.
                status.busy = true;
                status.can_send = false;
                status.can_stop = true;
                status.title = "正在生成回复".into();
                status.detail = crate::i18n::t("agent.receiving").as_str().into();
                agent.set_status(status.clone());
                chat.assert_history_anchor(&anchor, before, "task start");
                chat.assert_latest_below_view();

                status.has_progress = true;
                status.progress = 0.25;
                agent.set_status(status.clone());
                chat.assert_history_anchor(&anchor, before, "progress appears");
                status.progress = 0.75;
                agent.set_status(status.clone());
                chat.assert_history_anchor(&anchor, before, "progress advances");
                status.has_progress = false;
                agent.set_status(status.clone());
                chat.assert_history_anchor(&anchor, before, "progress disappears");

                status.busy = false;
                status.can_send = true;
                status.can_stop = false;
                status.title = crate::i18n::t("agent.idle").as_str().into();
                status.detail = "".into();
                agent.set_status(status.clone());
                chat.assert_history_anchor(&anchor, before, "task completion");
                chat.append("任务结束后仍不打断历史阅读");
                chat.assert_history_anchor(&anchor, before, "completed message append");
                chat.assert_latest_below_view();

                status.busy = true;
                status.can_send = false;
                status.can_stop = true;
                status.title = "继续处理".into();
                status.detail = "任务状态正常换行也不应移动阅读位置。".repeat(3).into();
                agent.set_status(status);
                chat.assert_history_anchor(&anchor, before, "task restarts with wrapped status");
                chat.append("重新开始后到达的新内容");
                chat.assert_history_anchor(&anchor, before, "restarted task append");
                chat.assert_latest_below_view();
            });
        }

        #[test]
        fn chat_history_screen_anchor_survives_streamed_model_replacements() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.desktop_layout();
                let agent = chat.app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.busy = true;
                status.can_send = false;
                status.can_stop = true;
                status.missing = "".into();
                status.title = "正在生成回复".into();
                status.detail = "正在接收回复。".into();
                agent.set_status(status);
                let mut partial = message("正在输出的第一段。\n");
                partial.pending = true;
                chat.messages.push(partial);
                chat.publish();
                chat.assert_latest_visible();
                chat.wheel(360.);
                chat.render();
                let (anchor, before) = chat.visible_history_anchor();
                chat.assert_latest_below_view();

                // Replace the production message model on each update, as
                // AgentPane::publish does, while the latest body gains lines.
                for lines in 2..=8 {
                    chat.messages.last_mut().unwrap().body =
                        "正在输出的新增段落。\n".repeat(lines).into();
                    chat.publish();
                    chat.assert_history_anchor(&anchor, before, "streamed text growth");
                    chat.assert_latest_below_view();
                }
                chat.messages.last_mut().unwrap().pending = false;
                chat.publish();
                chat.assert_history_anchor(&anchor, before, "streamed message completion");
                chat.append("后续消息到达时仍保持历史阅读位置。");
                chat.assert_history_anchor(&anchor, before, "following message append");
                chat.assert_latest_below_view();
            });
        }

        #[test]
        fn chat_layout_clamp_does_not_resume_but_user_scroll_to_end_does() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.assert_latest_visible();
                chat.wheel(-1200.);
                chat.append("已经贴底时继续向下滚不应暂停跟随");
                chat.assert_latest_visible();
                chat.wheel(360.);
                chat.render();
                chat.assert_latest_below_view();
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(600., 3000.));
                chat.render();
                assert!(
                    chat.scroll_view().content_y.abs() < 1.,
                    "the enlarged viewport must actually clamp the old offset"
                );
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(600., 900.));
                chat.append("布局限位不代表用户选择回到最新消息");
                chat.assert_latest_below_view();
                chat.wheel(-100_000.);
                chat.assert_latest_visible();
                chat.wheel(-1200.);
                chat.append("用户真实滚到底部后继续跟随");
                chat.assert_latest_visible();
            });
        }

        #[test]
        fn chat_send_button_restores_following_after_draft_and_status_changes() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.wheel(360.);
                chat.render();
                chat.assert_latest_below_view();
                chat.enlarge_composer();
                let sent = Rc::new(RefCell::new(Vec::new()));
                let received = Rc::clone(&sent);
                let weak = chat.app.as_weak();
                let agent = chat.app.global::<AgentUi>();
                agent.on_send_requested(move |text| {
                    received.borrow_mut().push(text.to_string());
                    let app = weak.upgrade().unwrap();
                    let agent = app.global::<AgentUi>();
                    agent.set_draft("".into());
                    let mut status = agent.get_status();
                    status.busy = true;
                    status.can_stop = true;
                    status.title = "发送后的真实状态更新".into();
                    status.detail = "状态内容换行。".repeat(8).into();
                    agent.set_status(status);
                });
                agent.set_draft("明确发送并恢复跟随".into());
                chat.click(crate::i18n::t("agent.send").as_str());
                assert_eq!(&*sent.borrow(), &["明确发送并恢复跟随"]);
                assert!(agent.get_draft().is_empty());
                chat.append("发送之后原生发布的最新回复");
                chat.assert_latest_visible();
            });
        }

        #[test]
        fn chat_history_expansion_and_collapse_preserve_manual_reading() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                let mut history = message(&"供逐段阅读的历史明细。\n".repeat(30));
                history.collapsible = true;
                history.preview = "历史明细预览".into();
                chat.messages.push(history);
                chat.publish();
                chat.render();
                let before = chat.scroll_view();
                let (anchor, anchor_bounds) = chat.visible_history_anchor();
                chat.click(crate::i18n::t("agent.expand").as_str());
                chat.render();
                let expanded = chat.scroll_view();
                assert!(
                    expanded.content_height > before.content_height + 150.,
                    "the real history row must expand"
                );
                assert!(
                    (expanded.content_y - before.content_y).abs() < 1.,
                    "expanding history must keep its reading position"
                );
                chat.assert_history_anchor(&anchor, anchor_bounds, "history expansion");
                chat.append("展开历史时到达的最新消息");
                chat.assert_history_anchor(
                    &anchor,
                    anchor_bounds,
                    "append after history expansion",
                );
                chat.assert_latest_below_view();
                chat.wheel(-100_000.);
                chat.assert_latest_visible();
                chat.click(crate::i18n::t("agent.collapse").as_str());
                chat.render();
                assert!(
                    chat.scroll_view().content_height < expanded.content_height,
                    "the real history row must collapse"
                );
                // Collapsing content can legitimately clamp the offset.
                // Once that settles, later arrivals must not move the reader.
                let (anchor, anchor_bounds) = chat.visible_history_anchor();
                chat.append(&"收起历史后仍保留阅读选择。\n".repeat(12));
                chat.assert_history_anchor(&anchor, anchor_bounds, "append after history collapse");
                chat.assert_latest_below_view();
            });
        }

        #[test]
        fn chat_drag_to_end_release_keeps_following_and_upward_drag_pauses() {
            let _language = crate::i18n::language_lock();
            with_chat(|chat| {
                chat.assert_latest_visible();
                chat.wheel(80.);
                chat.render();
                chat.assert_latest_below_view();
                let before_release = chat.drag_transcript(&[-40., -55.]);
                let end = (before_release.bounds.height - before_release.content_height).min(0.);
                assert!(
                    (before_release.content_y - end).abs() < 1.,
                    "the real drag must reach the lower bound before release: {before_release:?}"
                );
                chat.append("拖动到底部并带速度释放后仍显示最新回复");
                chat.assert_latest_visible();
                chat.advance(300);
                chat.render();

                chat.drag_transcript(&[40., 60.]);
                chat.append("从底部拖回历史后不应被新回复拉走");
                chat.assert_latest_below_view();
                chat.advance(200);
                chat.render();
                chat.append("向上阅读的惯性结束后继续保留位置");
                chat.assert_latest_below_view();
            });
        }

        #[cfg(not(target_os = "android"))]
        #[test]
        fn chat_native_wheel_intent_precedes_layout_and_respects_hit_test() {
            let _language = crate::i18n::language_lock();
            use slint::winit_030::winit::dpi::PhysicalPosition;
            use slint::winit_030::winit::event::MouseScrollDelta;

            with_chat(|chat| {
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.5 });
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(600., 900.));
                chat.assert_latest_visible();
                assert_eq!(chat.app.window().scale_factor(), 1.5);
                let assert_pending_wheel_position = |chat: &Chat, before: ScrollView| {
                    chat.render();
                    let after = chat.scroll_view();
                    assert!(
                        (after.content_y - before.content_y).abs() < 1.,
                        "reflow must preserve the offset before the user's wheel animation: before={before:?}, after={after:?}"
                    );
                    assert!(
                        after.content_height > before.content_height + 20.,
                        "the new content must actually extend the scroll range: before={before:?}, after={after:?}"
                    );
                    // Phone activity follows the messages in this same
                    // viewport. A short new message can remain visible even
                    // when its activity tail is below the preserved offset.
                    let end = (after.bounds.height - after.content_height).min(0.);
                    assert!(
                        after.content_y > end + 20.,
                        "pending upward intent must not follow the new actual scroll end: end={end}, after={after:?}"
                    );
                };
                let mut target = TranscriptScrollTarget::default();
                let mut native_wheel =
                    |chat: &Chat, position: slint::LogicalPosition, delta, shift| {
                        let scale = f64::from(chat.app.window().scale_factor());
                        pause_transcript_follow_for_native_wheel(
                            &chat.app,
                            chat.app.window(),
                            &mut target,
                            PhysicalPosition::new(
                                f64::from(position.x) * scale,
                                f64::from(position.y) * scale,
                            ),
                            &delta,
                            shift,
                        );
                    };
                let position = chat.scroll_view().bounds.center();
                native_wheel(
                    chat,
                    slint::LogicalPosition::new(0., 0.),
                    MouseScrollDelta::LineDelta(0., 1.),
                    false,
                );
                assert!(
                    !chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                    "outside input must not pause this transcript"
                );
                native_wheel(chat, position, MouseScrollDelta::LineDelta(0., -1.), false);
                assert!(
                    !chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                    "downward input at the end must keep following"
                );
                let horizontal = if cfg!(target_os = "macos") {
                    MouseScrollDelta::LineDelta(1., 0.)
                } else {
                    MouseScrollDelta::LineDelta(0., 1.)
                };
                native_wheel(chat, position, horizontal, true);
                assert!(
                    !chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                    "effective horizontal input must not pause vertical following"
                );
                chat.append("未命中聊天区及水平滚动均保留跟随");
                chat.assert_latest_visible();

                let position = chat.scroll_view().bounds.center();
                let before = chat.scroll_view();
                // Exercise the exact production pre-dispatch observer at the
                // same clock tick, before any content position has moved.
                // Public PointerScrolled uses the immediate core path, so it
                // cannot by itself prove this native smooth-wheel boundary.
                native_wheel(chat, position, MouseScrollDelta::LineDelta(0., 1.), false);
                assert!(chat.app.global::<AgentUi>().get_native_scroll_up_pending());
                let agent = chat.app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.busy = true;
                status.title = "同一帧发生的模型状态变化".into();
                status.detail = "布局改变不能覆盖刚开始的上翻输入。".repeat(6).into();
                agent.set_status(status);
                chat.append("原生上翻输入之后同批到达的新内容");
                assert_pending_wheel_position(chat, before);
                chat.wheel(80.);
                chat.render();
                chat.wheel(-100_000.);
                chat.assert_latest_visible();
                assert!(!chat.app.global::<AgentUi>().get_native_scroll_up_pending());

                // Finish a real downward drag by inertia, then queue a new
                // upward native input before the older position notification
                // is rendered. It must not consume the new intent as a return
                // to the end of the conversation.
                chat.wheel(80.);
                chat.render();
                let before_release = chat.drag_transcript(&[-10., -10.]);
                assert!(
                    before_release.content_y
                        > (before_release.bounds.height - before_release.content_height).min(0.)
                            + 20.
                );
                chat.advance(200);
                let view = chat.scroll_view();
                assert!(
                    (view.content_y - (view.bounds.height - view.content_height).min(0.)).abs()
                        < 1.,
                    "real downward inertia must reach the lower bound"
                );
                native_wheel(
                    chat,
                    view.bounds.center(),
                    MouseScrollDelta::LineDelta(0., 1.),
                    false,
                );
                chat.append("旧惯性到末尾的通知不能覆盖新上翻意图");
                assert_pending_wheel_position(chat, view);
                chat.wheel(-100_000.);
                chat.assert_latest_visible();

                let before = chat.scroll_view();
                let position = before.bounds.center();
                native_wheel(
                    chat,
                    position,
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(0., 24.)),
                    false,
                );
                assert!(
                    chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                    "touchpad pixel input must use the same guard"
                );
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(600., 820.));
                chat.append("触控板上翻输入与窗口缩小时也保留历史");
                assert_pending_wheel_position(chat, before);

                let app = chat.app.clone_strong();
                let agent = app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.busy = false;
                agent.set_status(status);
                agent.set_draft("发送后清除原生上翻状态".into());
                let sent = Rc::new(Cell::new(false));
                let received = Rc::clone(&sent);
                agent.on_send_requested(move |_| received.set(true));
                chat.click(crate::i18n::t("agent.send").as_str());
                assert!(
                    sent.get(),
                    "the real send button must reach its native callback"
                );
                assert!(!agent.get_native_scroll_up_pending());
                chat.append("明确发送之后重新显示最新回复");
                chat.assert_latest_visible();
            });
        }

        #[cfg(not(target_os = "android"))]
        #[test]
        fn chat_nonoverflowing_wheel_keeps_following_when_content_grows() {
            let _language = crate::i18n::language_lock();
            use slint::winit_030::winit::dpi::PhysicalPosition;
            use slint::winit_030::winit::event::MouseScrollDelta;

            with_chat(|chat| {
                chat.messages = vec![message("只有一条简短对话")];
                chat.publish();
                chat.assert_latest_visible();
                let view = chat.scroll_view();
                assert!((view.content_height - view.bounds.height).abs() < 1.);
                assert!(view.content_y.abs() < 1.);
                let position = view.bounds.center();
                let scale = f64::from(chat.app.window().scale_factor());
                pause_transcript_follow_for_native_wheel(
                    &chat.app,
                    chat.app.window(),
                    &mut TranscriptScrollTarget::default(),
                    PhysicalPosition::new(
                        f64::from(position.x) * scale,
                        f64::from(position.y) * scale,
                    ),
                    &MouseScrollDelta::LineDelta(0., 1.),
                    false,
                );
                chat.wheel(120.);
                chat.render();
                assert!(
                    !chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                    "input with no upward room must not pause following"
                );
                assert!(
                    chat.scroll_view().content_y.abs() < 1.,
                    "the real wheel must leave a short conversation unchanged"
                );
                chat.append(&"随后到达的长回复需要让最新内容保持可见。\n".repeat(60));
                chat.assert_latest_visible();
                let view = chat.scroll_view();
                assert!(
                    view.content_height > view.bounds.height + 200.,
                    "the new reply must actually overflow"
                );
            });
        }

        #[cfg(not(target_os = "android"))]
        #[test]
        fn chat_wheel_on_covering_dialog_does_not_pause_background_following() {
            let _language = crate::i18n::language_lock();
            use slint::winit_030::winit::dpi::PhysicalPosition;
            use slint::winit_030::winit::event::MouseScrollDelta;

            with_chat(|chat| {
                chat.assert_latest_visible();
                let mut target = TranscriptScrollTarget::default();
                let mut native_wheel =
                    |chat: &Chat, position: slint::LogicalPosition, delta_y: f32| {
                        let scale = f64::from(chat.app.window().scale_factor());
                        pause_transcript_follow_for_native_wheel(
                            &chat.app,
                            chat.app.window(),
                            &mut target,
                            PhysicalPosition::new(
                                f64::from(position.x) * scale,
                                f64::from(position.y) * scale,
                            ),
                            &MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                                0.,
                                f64::from(delta_y),
                            )),
                            false,
                        );
                    };
                let mut wheel_on_overlay =
                    |chat: &Chat, position: slint::LogicalPosition, delta_y: f32, label: &str| {
                        chat.render();
                        assert!(
                            chat.app.get_content_overlay_open(),
                            "the actual covering component must be open"
                        );
                        assert!(
                            chat.app
                                .invoke_content_overlay_covers(position.x, position.y),
                            "the observed foreground must really cover this wheel point: {label}"
                        );
                        let before = chat.scroll_view();
                        native_wheel(chat, position, delta_y);
                        assert!(
                            !chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                            "the raw observer must ignore a still-visible foreground: {label}"
                        );
                        chat.wheel_at(position, delta_y);
                        chat.render();
                        let after = chat.scroll_view();
                        assert!(
                            (after.content_y - before.content_y).abs() < 1.,
                            "foreground wheel must not reach the real background transcript: {label}, before={before:?}, after={after:?}"
                        );
                    };

                let set_sheet_open = |chat: &Chat, kind: i32, open: bool| match kind {
                    0 => chat.app.global::<ui::AgentSettingsUi>().set_open(open),
                    1 => {
                        let mut settings = chat.app.get_settings();
                        settings.open = open;
                        chat.app.set_settings(settings);
                    }
                    2 => {
                        let mut export = chat.app.get_export();
                        export.open = open;
                        chat.app.set_export(export);
                    }
                    _ => unreachable!(),
                };
                chat.app
                    .global::<ui::AgentSettingsUi>()
                    .set_can_modify(true);
                // A configured channel renders the complete existing form.
                // No credential value is assigned to any test property.
                chat.app
                    .global::<ui::AgentSettingsUi>()
                    .set_has_saved_channel(true);
                for (kind, title) in [
                    (0, crate::i18n::t("modelSettings.title")),
                    (1, crate::i18n::t("common.settings")),
                    (2, crate::i18n::t("common.export")),
                ] {
                    let title = title.as_str();
                    chat.render();
                    // The original editor stays mounted behind the sheet.
                    // Exclude its already-visible labels by item identity,
                    // never by duplicate text order or guessed coordinates.
                    let background = chat.matching_overlay_text_items(title, true);
                    let overlay_title = |chat: &Chat| {
                        let mut found: Vec<_> = chat
                            .matching_overlay_text_items(title, true)
                            .into_iter()
                            .filter(|(item, _)| !background.iter().any(|(old, _)| old == item))
                            .collect();
                        assert_eq!(found.len(), 1, "one foreground title item: {title}");
                        found.pop().unwrap()
                    };
                    set_sheet_open(chat, kind, true);
                    chat.render();
                    chat.advance(400);
                    chat.render();
                    let (title_item, header) = overlay_title(chat);
                    assert!(
                        header.y >= 0. && header.bottom() <= 52.,
                        "the real 380ms opening animation must finish: {title}, header={header:?}"
                    );
                    wheel_on_overlay(chat, chat.scroll_view().bounds.center(), 120., title);
                    if kind == 0 {
                        // A lower fallback layer must not take the model
                        // form's own scroll input. First establish actual
                        // overflow and a visible hit inside that form; a
                        // short new-channel form fits this window and has
                        // no legitimate scrolling distance.
                        let before = chat.overlay_text_bounds(
                            crate::i18n::t("modelSettings.connection").as_str(),
                            true,
                        );
                        let last_control = chat.overlay_text_bounds(
                            crate::i18n::t("modelSettings.deleteCredential").as_str(),
                            false,
                        );
                        let window_bottom = chat.app.window().size().height as f32
                            / chat.app.window().scale_factor();
                        assert!(
                            last_control.bottom() > window_bottom + 20.,
                            "the complete real form must overflow its page: last={last_control:?}, bottom={window_bottom}"
                        );
                        let position = before.center();
                        assert!(
                            position.y > header.bottom() && position.y < window_bottom,
                            "the real visible label must be inside the foreground form: {before:?}"
                        );
                        wheel_on_overlay(chat, position, -160., title);
                        let after = chat.overlay_text_bounds(
                            crate::i18n::t("modelSettings.connection").as_str(),
                            false,
                        );
                        assert!(
                            after.y < before.y - 20.,
                            "the real foreground form must still scroll: before={before:?}, after={after:?}"
                        );
                    }
                    chat.append(&format!("{title}覆盖聊天时，新内容仍保持跟随"));
                    chat.assert_latest_visible();

                    set_sheet_open(chat, kind, false);
                    chat.render();
                    chat.advance(60);
                    chat.render();
                    // The moving header now covers the transcript. A wheel
                    // here exercises the page's fallback, not PaneScroll.
                    let (closing_item, header) = overlay_title(chat);
                    assert!(
                        closing_item == title_item,
                        "the same title must animate closed"
                    );
                    let position = header.center();
                    assert!(
                        header.y > 0. && chat.scroll_view().bounds.contains(position),
                        "the real closing header must still cover the wheel point: {title}, header={header:?}"
                    );
                    wheel_on_overlay(chat, position, 120., title);
                    chat.append(&format!("{title}关闭动画期间也不能暂停后台跟随"));
                    chat.assert_latest_visible();

                    chat.advance(400);
                    chat.render();
                    assert!(
                        !chat.app.get_content_overlay_open(),
                        "finished closing must release background input: {title}"
                    );
                    // Use a fresh raw observer invocation after the same
                    // surface is gone, followed by real core wheel input.
                    let position = chat.scroll_view().bounds.center();
                    let scale = f64::from(chat.app.window().scale_factor());
                    pause_transcript_follow_for_native_wheel(
                        &chat.app,
                        chat.app.window(),
                        &mut TranscriptScrollTarget::default(),
                        PhysicalPosition::new(
                            f64::from(position.x) * scale,
                            f64::from(position.y) * scale,
                        ),
                        &MouseScrollDelta::LineDelta(0., 1.),
                        false,
                    );
                    assert!(
                        chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                        "closed foreground must allow real upward intent: {title}"
                    );
                    chat.wheel_at(position, 120.);
                    chat.append(&format!("{title}完全关闭后，手动阅读历史不被新消息拉走"));
                    chat.assert_latest_below_view();
                    chat.wheel(-100_000.);
                    chat.append(&format!("{title}关闭后明确滚回底部恢复跟随"));
                    chat.assert_latest_visible();
                }

                let mut confirm = chat.app.get_confirm();
                confirm.open = true;
                confirm.title = "真实确认组件回归".into();
                confirm.message = "此夹具不触发任何工程操作。".into();
                chat.app.set_confirm(confirm.clone());
                chat.render();
                let header = chat.overlay_text_bounds("真实确认组件回归", true);
                assert!(
                    chat.scroll_view().bounds.contains(header.center()),
                    "the actual card must cover the transcript"
                );
                wheel_on_overlay(chat, header.center(), 120., "确认卡片");
                let view = chat.scroll_view();
                // ConfirmDialog has a 400px centered card in this 600px
                // window. This real point is on its outside scrim.
                let scrim = slint::LogicalPosition::new(view.bounds.x + 1., view.bounds.center().y);
                assert!(scrim.x < 100. && view.bounds.contains(scrim));
                wheel_on_overlay(chat, scrim, 120., "确认遮罩");
                confirm.open = false;
                chat.app.set_confirm(confirm);
                chat.append("确认窗口上的滚轮不改变后台跟随");
                chat.assert_latest_visible();

                let mut relink = chat.app.get_relink();
                relink.open = true;
                relink.items = slint::ModelRc::new(slint::VecModel::from(
                    (0..10)
                        .map(|index| ui::MissingMediaItem {
                            id: format!("overlay-test-{index}").into(),
                            name: format!("隔离校验素材{index}").into(),
                            folder: "界面测试夹具".into(),
                            kind: 0,
                            clips: 1,
                        })
                        .collect::<Vec<_>>(),
                ));
                chat.app.set_relink(relink.clone());
                chat.render();
                chat.overlay_text_bounds(crate::i18n::t("relink.missingMedia").as_str(), true);
                wheel_on_overlay(
                    chat,
                    chat.scroll_view().bounds.center(),
                    120.,
                    "重连卡片空白",
                );
                let first = chat.overlay_text_bounds("隔离校验素材0", true);
                wheel_on_overlay(chat, first.center(), -100., "重连列表子控件");
                let moved = chat.overlay_text_bounds("隔离校验素材0", false);
                assert!(
                    moved.y < first.y - 20.,
                    "the real modal child list must keep scrolling: before={first:?}, after={moved:?}"
                );
                relink.open = false;
                chat.app.set_relink(relink);
                chat.append("素材重连窗口上的滚轮不改变后台跟随");
                chat.assert_latest_visible();

                // A desktop tool card is not a full-window modal. Move the
                // real floating card by its real header, leaving the chat
                // uncovered, and verify that its presence cannot swallow
                // upward reading intent outside the card.
                chat.app.set_phone(false);
                chat.app
                    .window()
                    .set_size(slint::LogicalSize::new(1400., 1000.));
                chat.app.global::<AgentUi>().set_sidebar_open(true);
                chat.render();
                chat.wheel(-100_000.);
                chat.assert_latest_visible();
                let mut captions = chat.app.get_captions();
                captions.open = true;
                chat.app.set_captions(captions.clone());
                chat.render();
                let header = || {
                    let matches: Vec<_> = chat
                        .matching_overlay_text(crate::i18n::t("captions.captions").as_str(), true)
                        .into_iter()
                        .filter(|bounds| bounds.y >= 40. && bounds.bottom() <= 95.)
                        .collect();
                    assert_eq!(matches.len(), 1, "one actual floating card header");
                    matches[0]
                };
                let before = header();
                let position = before.center();
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
                chat.advance(16);
                let position = slint::LogicalPosition::new(position.x - 600., position.y);
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::PointerMoved { position });
                chat.advance(16);
                chat.app
                    .window()
                    .dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                chat.render();
                let moved = header();
                assert!(
                    moved.x < before.x - 500.,
                    "the real floating card must move: before={before:?}, after={moved:?}"
                );
                let view = chat.scroll_view();
                let position = view.bounds.center();
                assert!(
                    !chat
                        .app
                        .invoke_content_overlay_covers(position.x, position.y),
                    "the card must leave this real chat position uncovered"
                );
                let scale = f64::from(chat.app.window().scale_factor());
                pause_transcript_follow_for_native_wheel(
                    &chat.app,
                    chat.app.window(),
                    &mut TranscriptScrollTarget::default(),
                    PhysicalPosition::new(
                        f64::from(position.x) * scale,
                        f64::from(position.y) * scale,
                    ),
                    &MouseScrollDelta::LineDelta(0., 1.),
                    false,
                );
                assert!(
                    chat.app.global::<AgentUi>().get_native_scroll_up_pending(),
                    "a card elsewhere must not suppress upward reading intent"
                );
                chat.wheel_at(position, 120.);
                chat.render();
                assert!(
                    chat.scroll_view().content_y > view.content_y + 20.,
                    "the uncovered transcript must actually receive its real wheel"
                );
                chat.append("浮动工具卡片外仍能正常阅读聊天历史");
                chat.assert_latest_below_view();
                captions.open = false;
                chat.app.set_captions(captions);
                chat.wheel(-100_000.);
                chat.append("浮动卡片关闭后明确回到底部仍恢复跟随");
                chat.assert_latest_visible();
            });
        }
    }
}
