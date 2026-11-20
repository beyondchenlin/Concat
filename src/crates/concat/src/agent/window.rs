// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pi callbacks attached to the native editor window.

use crate::{
    embedded,
    host::Shell,
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
        let mut result = Err("聊天运行组件尚未连接。".to_owned());
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.inspect();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_continue_requested(|| {
        let mut result = Err("聊天运行组件尚未连接。".to_owned());
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.confirm();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_discard_requested(|| {
        let mut result = Err("聊天运行组件尚未连接。".to_owned());
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.discard_interrupted();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_connect_requested(|| embedded::show_ui_error(embedded::connect_from_ui()));
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
        || agent.get_view() == ui::AgentView::Script
        || (app.get_phone() && agent.get_view() != ui::AgentView::Chat)
        || (!app.get_phone() && !agent.get_sidebar_open())
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
    let mut item = target.item.as_ref().and_then(|item| item.upgrade());
    if item.is_none() {
        let Some(root) = window.window_item_rc() else {
            return;
        };
        item = root.visit_descendants(|item| {
            if item.is_visible()
                && item.downcast::<Flickable>().is_some()
                && item
                    .accessible_string_property(AccessibleStringProperty::Label)
                    .is_some_and(|label| label == "对话记录")
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
/// A pending request is always made visible even in manual editing.
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
        ui.set_tool_mode(permission.mode);
        ui.set_tool_connected(ready);
        ui.set_tool_request_id(permission.pending_id.unwrap_or_default().into());
        ui.set_tool_pending(pending);
        let mut summary = permission.pending_summary.unwrap_or_default();
        if pending && !permission.pending_clip_ids.is_empty() {
            summary.push_str("\n当前工程中已有片段（批准前核对）：");
            let studio = shell.studio.borrow();
            for id in &permission.pending_clip_ids {
                if let Some(clip) = studio.clip(id) {
                    let name: String = clip
                        .name
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(48)
                        .collect();
                    summary.push_str(&format!(
                        "\n· 片段 {}「{}」：当前轨道 {}，当前时间 {:.3}～{:.3} 秒",
                        id,
                        name,
                        clip.track_id,
                        clip.start,
                        clip.start + clip.duration
                    ));
                } else {
                    summary.push_str(&format!(
                        "\n· 片段 {}：当前工程中找不到，编辑器将拒绝无效目标",
                        id
                    ));
                }
            }
        }
        ui.set_tool_summary(summary.into());
        if pending && ready {
            ui.set_view(ui::AgentView::Chat);
            ui.set_sidebar_open(true);
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
        // A dedicated test thread has its own software-only Slint platform;
        // no OS desktop, model, project or application Shell is opened.
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                slint::platform::set_platform(Box::new(NativeTestPlatform(window))).unwrap();
                let app = App::new().unwrap();
                app.set_phone(true);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(600., 800.));
                app.show().unwrap();
                let agent = app.global::<AgentUi>();
                agent.set_tool_connected(true);
                agent.set_tool_summary("审批手势回归".into());
                agent.set_tool_request_id("request-a".into());
                agent.set_tool_pending(true);
                let decisions = Rc::new(RefCell::new(Vec::new()));
                let received = Rc::clone(&decisions);
                agent.on_tool_decision_requested(move |id, approved| {
                    received.borrow_mut().push((id.to_string(), approved));
                });
                let approve = rendered_button(&app, "批准本次");
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
                rendered_button(&app, "批准本次");
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
                rendered_button(&app, "批准本次");
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
                rendered_button(&app, "批准本次");
                app.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: "\n".into() });
                app.window()
                    .dispatch_event(WindowEvent::KeyReleased { text: "\n".into() });
                assert_eq!(&*decisions.borrow(), &[("request-d".into(), true)]);
                decisions.borrow_mut().clear();

                let deny = rendered_button(&app, "拒绝");
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
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                slint::platform::set_platform(Box::new(NativeTestPlatform(window))).unwrap();
                let app = App::new().unwrap();
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
                author: "模型".into(),
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
                            .is_some_and(|label| label == "对话记录")
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
                    view.bounds.height > 40.,
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
                            .is_some_and(|label| label == "对话记录")
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
            fn matching_overlay_text(&self, body: &str, visible_only: bool) -> Vec<Bounds> {
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
                        found.push(Bounds {
                            x: origin.x,
                            y: origin.y,
                            width: geometry.size.width,
                            height: geometry.size.height,
                        });
                    }
                    ControlFlow::Continue(())
                });
                found
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
                self.render();
                let root = WindowInner::from_pub(self.app.window())
                    .window_item_rc()
                    .unwrap();
                let mut points = Vec::new();
                root.visit_descendants::<()>(|item| {
                    if item.is_visible()
                        && item.accessible_role() == AccessibleRole::Button
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
                            .is_some_and(|value| value == "对话输入")
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

        #[test]
        fn chat_follows_streamed_content_and_viewport_reflow() {
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

        #[test]
        fn chat_layout_clamp_does_not_resume_but_user_scroll_to_end_does() {
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
                chat.click("发送");
                assert_eq!(&*sent.borrow(), &["明确发送并恢复跟随"]);
                assert!(agent.get_draft().is_empty());
                chat.append("发送之后原生发布的最新回复");
                chat.assert_latest_visible();
            });
        }

        #[test]
        fn chat_history_expansion_and_collapse_preserve_manual_reading() {
            with_chat(|chat| {
                let mut history = message(&"供逐段阅读的历史明细。\n".repeat(30));
                history.collapsible = true;
                history.preview = "历史明细预览".into();
                chat.messages.push(history);
                chat.publish();
                chat.render();
                let before = chat.scroll_view();
                chat.click("展开完整内容");
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
                chat.append("展开历史时到达的最新消息");
                chat.assert_latest_below_view();
                chat.wheel(-100_000.);
                chat.assert_latest_visible();
                chat.click("收起详情");
                chat.render();
                assert!(
                    chat.scroll_view().content_height < expanded.content_height,
                    "the real history row must collapse"
                );
                chat.append(&"收起历史后仍保留阅读选择。\n".repeat(12));
                chat.assert_latest_below_view();
            });
        }

        #[test]
        fn chat_drag_to_end_release_keeps_following_and_upward_drag_pauses() {
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
                let before = chat.scroll_view().content_y;
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
                chat.render();
                assert!(
                    (chat.scroll_view().content_y - before).abs() < 1.,
                    "reflow must not move content before the user's animation"
                );
                chat.assert_latest_below_view();
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
                chat.assert_latest_below_view();
                chat.wheel(-100_000.);
                chat.assert_latest_visible();

                let position = chat.scroll_view().bounds.center();
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
                chat.assert_latest_below_view();

                let app = chat.app.clone_strong();
                let agent = app.global::<AgentUi>();
                let mut status = agent.get_status();
                status.busy = false;
                agent.set_status(status);
                agent.set_draft("发送后清除原生上翻状态".into());
                let sent = Rc::new(Cell::new(false));
                let received = Rc::clone(&sent);
                agent.on_send_requested(move |_| received.set(true));
                chat.click("发送");
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
                    (0, "模型渠道与凭据"),
                    (1, "common.settings"),
                    (2, "common.export"),
                ] {
                    set_sheet_open(chat, kind, true);
                    chat.render();
                    chat.advance(400);
                    chat.render();
                    let header = chat.overlay_text_bounds(title, true);
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
                        let before = chat.overlay_text_bounds("编辑渠道", true);
                        let last_control = chat.overlay_text_bounds("删除对应系统凭据", false);
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
                        let after = chat.overlay_text_bounds("编辑渠道", false);
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
                    let header = chat.overlay_text_bounds(title, true);
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
                chat.overlay_text_bounds("relink.missingMedia", true);
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
                        .matching_overlay_text("captions.captions", true)
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
