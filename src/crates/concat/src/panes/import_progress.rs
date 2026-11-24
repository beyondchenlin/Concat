// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 BK CUT contributors
//! Native cancellation and progress for ordered media preparation. No project
//! writes happen here; each caller still checks its project and request epoch.

use slint::ComponentHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
static VISIBLE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(crate) struct ImportJob {
    id: u64,
    cancelled: Arc<AtomicBool>,
}

impl ImportJob {
    pub(crate) fn reserved() -> Self {
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            cancelled: Arc::default(),
        }
    }

    pub(crate) fn new(total: usize) -> Self {
        let job = Self::reserved();
        VISIBLE.store(job.id, Ordering::Release);
        job.progress(0, total);
        job
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.finish();
    }

    pub(crate) fn progress(&self, done: usize, total: usize) {
        let job = self.clone();
        crate::host::on_ui(move |_, app, _| {
            if job.cancelled() || VISIBLE.load(Ordering::Acquire) != job.id {
                return;
            }
            let ui = app.global::<crate::ui::ImportUi>();
            ui.set_active(true);
            ui.set_progress(if total == 0 {
                0.0
            } else {
                done as f32 / total as f32
            });
            ui.set_message(crate::i18n::tf("import.preparing", &[&done, &total]).into());
        });
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub(crate) fn copying(&self, progress: crate::platform::PickProgress) {
        if self.cancelled() {
            return;
        }
        let job = self.clone();
        crate::host::on_ui(move |_, app, _| {
            if job.cancelled() {
                return;
            }
            VISIBLE.store(job.id, Ordering::Release);
            let ui = app.global::<crate::ui::ImportUi>();
            ui.set_active(true);
            ui.set_progress(progress.fraction());
            ui.set_message(
                crate::i18n::tf(
                    "import.copying",
                    &[&progress.completed, &progress.total, &progress.copied_bytes],
                )
                .into(),
            );
        });
    }

    pub(crate) fn finish(&self) {
        if VISIBLE
            .compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            crate::host::on_ui(|_, app, _| {
                if VISIBLE.load(Ordering::Acquire) == 0 {
                    app.global::<crate::ui::ImportUi>().set_active(false);
                }
            });
        }
    }
}

pub(crate) fn install(app: &crate::ui::App) {
    app.global::<crate::ui::ImportUi>().on_cancel_requested(|| {
        crate::host::Shell::with(|shell, app| {
            let mut studio = shell.studio.borrow_mut();
            let copying = crate::platform::cancel_file_pick();
            studio.handle(crate::panes::Msg::Media(
                crate::panes::media_bin::MediaMsg::CancelImport,
            ));
            studio.handle(crate::panes::Msg::Start(
                crate::panes::start::StartMsg::CancelImport,
            ));
            if copying {
                studio.notify(&crate::i18n::t("import.cancelled"), false);
            }
            app.global::<crate::ui::ImportUi>().set_active(false);
        });
    });
}
