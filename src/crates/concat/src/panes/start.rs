// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! The launch screen's sheet: a new project's name, place, shape, size
//! and rate, the verb that opens one that already exists, and the project
//! grid's own two verbs.

use std::path::{Path, PathBuf};

use concat_host::projects;

use crate::i18n::{t, tf};
use crate::panes::import_progress::ImportJob;
use crate::platform;
use crate::studio::{
    ASPECTS, RATES, SIZES, Studio, custom_frame, custom_rate, fps_of, frame_size, home_folder,
};
use crate::ui::StartData;

/// Everything that can happen to the launch screen's sheet.
#[derive(Clone, Debug)]
pub enum StartMsg {
    /// Put up the sheet a new project is described on.
    Compose,
    /// New from files, including standalone audio and items outside Photos.
    ComposeFiles,
    CancelImport,
    Prepared {
        request: u64,
        epoch: u64,
        results: Vec<Result<concat_host::media::MediaSummary, String>>,
    },
    DraftReady {
        request: u64,
        epoch: u64,
        result: Result<projects::ProjectInfo, String>,
        imports: Vec<Result<concat_host::media::MediaSummary, String>>,
    },
    /// Take it down, keeping what was typed for next time.
    Dismiss,
    NameEdited(String),
    LocationEdited(String),
    /// The frame's shape: 16:9, 9:16, 1:1, 4:3.
    AspectChanged(i32),
    /// The frame's size, as the short edge: 720p, 1080p, 4K.
    SizeChanged(i32),
    RateChanged(i32),
    /// The custom frame's sides and rate, as typed.
    CustomWidth(f32),
    CustomHeight(f32),
    CustomFps(f32),
    /// What shows where no picture covers the frame, `[r, g, b, a]`.
    CanvasChanged([u8; 4]),
    DismissError,
    /// Pick where the project folder goes.
    Browse,
    /// The folder the picker answered with.
    FolderChosen(String),
    Create,
    /// Open a project that already exists, picked from disk.
    Open,
    /// The folder the Open picker answered with.
    OpenChosen(PathBuf),
    /// Make a project of a CapCut draft, picked from CapCut's drafts.
    ImportCapcut,
    OpenRecent(String),
    ForgetRecent(String),
    /// Show a project's folder in the file manager.
    RevealRecent(String),
    /// What the projects' search field holds now.
    SearchEdited(String),
    /// How the projects are ordered: [`SORT_RECENT`] or [`SORT_NAME`].
    Sort(i32),
    /// One of the learn-and-community cards, or the version line, by the
    /// name [`link`] knows it by.
    Link(String),
    /// Files or folders dropped on the launch screen: a project folder
    /// opens, and anything else starts a new project to go into.
    Dropped(Vec<PathBuf>),
}

/// The projects in the order they were last opened, newest first.
pub const SORT_RECENT: i32 = 0;
/// The projects by name, A to Z.
pub const SORT_NAME: i32 = 1;

/// The recents the grid shows: those whose name or folder holds every
/// word of the query, ignoring case, newest first or by name. A missing
/// folder is found and sorted like any other.
pub fn shown<'a>(
    recents: &'a [projects::Recent],
    query: &str,
    sort: i32,
) -> Vec<&'a projects::Recent> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut shown: Vec<&projects::Recent> = recents
        .iter()
        .filter(|recent| {
            let name = recent.info.name.to_lowercase();
            let path = recent.info.path.to_lowercase();
            words
                .iter()
                .all(|word| name.contains(word.as_str()) || path.contains(word.as_str()))
        })
        .collect();
    if sort == SORT_NAME {
        shown.sort_by_cached_key(|recent| recent.info.name.to_lowercase());
    }
    shown
}

/// The official site, which the guides and the developer docs are pages of.
const SITE: &str = "https://concat.standalones.org";
/// The repository: the code, the issues, the releases.
pub const REPOSITORY: &str = "https://github.com/beyondchenlin/Concat";

/// Where each of the launch screen's links goes, by the name the screen
/// sends. One table, so a moved page is one line here. Every address is a
/// page of the website's own content - src/content/guides in its
/// repository - or the repository's, and None for a name nobody sends.
/// "shortcuts" is not here: it opens Settings, not a page.
pub fn link(name: &str) -> Option<String> {
    Some(match name {
        "guides" => format!("{SITE}/guides/"),
        "first-edit" => format!("{SITE}/guides/first-edit/"),
        "report" => format!("{SITE}/guides/report-a-problem/"),
        "github" => REPOSITORY.to_owned(),
        // This development build must not claim an upstream release as
        // this product's change history or assume a published product tag.
        "news" => format!("{REPOSITORY}/releases"),
        // The invite the README carries.
        "discord" => "https://discord.gg/DVuPfpXfqP".to_owned(),
        // Product releases, including development builds with no published
        // matching tag. Upstream learning and licence links remain separate.
        "release-notes" => format!("{REPOSITORY}/releases"),
        _ => return None,
    })
}

/// The sheet on the launch screen.
pub struct StartPane {
    /// The sheet is up. Down again once the project it describes opens,
    /// and not before: a create that fails keeps the form, and its notice,
    /// on screen.
    pub composing: bool,
    pub name: String,
    pub location: String,
    /// Index into [`ASPECTS`].
    pub aspect: usize,
    /// Index into [`SIZES`], or one past its end for a custom frame.
    pub size: usize,
    /// Index into [`RATES`], or one past its end for a custom rate.
    pub rate: usize,
    /// The custom frame, even on both sides; seeded from the preset that
    /// was picked when Custom was, so the fields start from a real frame.
    pub custom_size: (u32, u32),
    /// The custom rate, as an exact fraction.
    pub custom_rate: (i64, i64),
    /// What shows where no picture covers the frame (#456): black, as
    /// every project before it had.
    pub canvas: [u8; 4],
    pub busy: bool,
    request: u64,
    import_job: Option<ImportJob>,
    pub error: String,
    /// The projects' search, as typed; empty shows them all.
    pub query: String,
    /// [`SORT_RECENT`] or [`SORT_NAME`].
    pub sort: i32,
    /// Media dropped on the launch screen, waiting for the project the
    /// sheet is about to make: imported into it once it opens, and let go
    /// if the sheet is put down instead.
    pub dropped: Vec<PathBuf>,
}

impl Default for StartPane {
    fn default() -> Self {
        Self {
            composing: false,
            name: untitled(),
            location: default_location(),
            aspect: 0,
            // 1080p, not the first of the three. The size everything else
            // in the app assumes, and the one a phone and a desk agree on.
            size: 1,
            rate: 3,
            custom_size: (1920, 1080),
            custom_rate: (30, 1),
            canvas: [0, 0, 0, 255],
            busy: false,
            request: 0,
            import_job: None,
            error: String::new(),
            query: String::new(),
            sort: SORT_RECENT,
            dropped: Vec::new(),
        }
    }
}

/// Where a new project goes unless told otherwise; see [`location_on`].
/// On an iPhone the folder is made here, so that the Files app shows it
/// from the first launch and not only from the first project.
fn default_location() -> String {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let desktop = dirs::document_dir();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let desktop = None;
    let location = location_on(std::env::consts::OS, desktop, home_folder);
    if cfg!(target_os = "ios")
        && !location.is_empty()
        && let Err(error) = std::fs::create_dir_all(&location)
    {
        log::warn!(
            "project: wanted the new-project folder {location} and could not make it: {error}; the first project makes it"
        );
    }
    location
}

/// Where a new project goes on `os`: a Concat folder on the desktop. The
/// desktop the system names, not `~/Desktop`: on Windows OneDrive often
/// moves it, and on Linux it is called what the desktop's language calls
/// it. A phone has no desk. An Android phone's projects live at the top of
/// the folder the file manager shows for the app. An iPhone's live in the
/// app's Documents, the one folder of its sandbox the Files app can show:
/// `~/Desktop` there is a folder the app is not allowed to make, and every
/// new project failed with EPERM. https://github.com/jub0t/Concat/issues/299
fn location_on(os: &str, desktop: Option<PathBuf>, home: impl Fn(&str) -> String) -> String {
    match os {
        "android" => home("Concat"),
        "ios" => home("Documents/Concat"),
        _ => desktop
            .map(|desktop| {
                desktop
                    .join("BK CUT")
                    .join("Projects")
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| home("Documents/BK CUT/Projects")),
    }
}

/// What an unnamed project is called, in the interface's language: the name
/// this sheet suggests, the folder it makes, and what the title bar shows
/// until the project is named.
fn untitled() -> String {
    t("common.untitledProject")
}

/// Whether `name` is one this sheet made up rather than one somebody
/// typed: "Untitled project", or that with a number after it.
#[cfg(test)]
fn is_untitled(name: &str) -> bool {
    let untitled = untitled();
    let name = name.trim();
    name.is_empty()
        || name == untitled.as_str()
        || name
            .strip_prefix(untitled.as_str())
            .and_then(|rest| rest.strip_prefix(' '))
            .is_some_and(|number| number.parse::<u32>().is_ok_and(|n| n >= 2))
}

/// The first of "Untitled project", "Untitled project 2", ... whose folder
/// is not `taken`. A second new project in the same place used to be
/// refused as "already exists" until it was renamed by hand.
/// https://github.com/jub0t/Concat/issues/151
#[cfg(test)]
fn first_free_untitled(taken: impl Fn(&str) -> bool) -> String {
    let untitled = untitled();
    std::iter::once(untitled.clone())
        .chain((2..).map(|n| format!("{untitled} {n}")))
        .find(|name| !taken(&projects::folder_name(name)))
        .expect("the numbers do not run out")
}

/// A late successful creation is an unused candidate, never a reason to
/// replace the project explicitly chosen while its directory was being made.
fn remember_unopened(info: projects::ProjectInfo) {
    crate::host::spawn(
        move || projects::drafts::remember_unopened(&info),
        |studio, _, _, result| {
            if let Err(error) = result {
                studio.notify(&error, true);
            }
        },
    );
}

impl StartPane {
    /// Applies one message. The studio is the rest of the window; while
    /// this runs the studio's copy of the pane is a blank it must not read.
    pub fn update(&mut self, msg: StartMsg, studio: &mut Studio) {
        match msg {
            StartMsg::Compose => self.compose(studio, false),
            StartMsg::ComposeFiles => self.compose(studio, true),
            StartMsg::CancelImport => {
                if self.import_job.is_some() {
                    self.retire_request();
                    self.dropped.clear();
                    studio.notify(&t("import.cancelled"), false);
                }
            }
            StartMsg::Prepared {
                request,
                epoch,
                results,
            } => {
                if request != self.request {
                    return;
                }
                self.busy = false;
                if epoch != crate::host::project_epoch() {
                    return;
                }
                if results.iter().any(Result::is_ok) {
                    if let Some(job) = self.import_job.take() {
                        job.finish();
                    }
                    self.begin_draft(studio, results);
                } else if !results.is_empty() {
                    if let Some(job) = self.import_job.take() {
                        job.finish();
                    }
                    let errors = results
                        .into_iter()
                        .filter_map(Result::err)
                        .map(|error| crate::host::probe_error(&error))
                        .collect::<Vec<_>>();
                    studio.notify(&errors.join("\n"), true);
                } else if let Some(job) = self.import_job.take() {
                    job.finish();
                }
            }
            StartMsg::DraftReady {
                request,
                epoch,
                result,
                imports,
            } => {
                if request != self.request {
                    if let Ok(info) = result {
                        remember_unopened(info);
                    }
                    return;
                }
                self.busy = false;
                if epoch != crate::host::project_epoch() {
                    if let Ok(info) = result {
                        remember_unopened(info);
                    }
                    return;
                }
                if let Some(job) = self.import_job.take() {
                    job.finish();
                }
                match result.and_then(|info| {
                    let path = info.path.clone();
                    match studio.open_project(info.clone()) {
                        Ok(()) => Ok(path),
                        Err(error) => {
                            remember_unopened(info);
                            Err(error)
                        }
                    }
                }) {
                    Ok(path) => {
                        self.composing = false;
                        self.error.clear();
                        studio.prefs.last_project_location = Some(self.location.clone());
                        studio.prefs.empty_draft_path = Some(path);
                        studio.prefs.save(&studio.host.dirs);
                        if !imports.is_empty() {
                            studio.handle(crate::panes::Msg::Media(
                                crate::panes::media_bin::MediaMsg::InitialImported(imports),
                            ));
                        }
                        let dropped = std::mem::take(&mut self.dropped);
                        if !dropped.is_empty() {
                            studio.handle(crate::panes::Msg::Media(
                                crate::panes::media_bin::MediaMsg::Import(dropped),
                            ));
                        }
                    }
                    Err(error) => {
                        self.error = error.clone();
                        // The new-project defaults remain editable in Settings.
                        // A failed create must not expose the retired setup form.
                        self.composing = false;
                        studio.notify(&error, true);
                    }
                }
            }
            StartMsg::Dismiss => {
                self.retire_request();
                self.composing = false;
                // The notice belongs to the attempt it reported on, not to
                // the next time the sheet comes up; and so do the files
                // dropped to start it.
                self.error.clear();
                self.dropped.clear();
            }
            StartMsg::NameEdited(name) => self.name = name,
            StartMsg::LocationEdited(path) => self.location = path,
            StartMsg::AspectChanged(index) => {
                self.aspect = (index.max(0) as usize).min(ASPECTS.len() - 1);
            }
            StartMsg::SizeChanged(index) => {
                let index = (index.max(0) as usize).min(SIZES.len());
                if index == SIZES.len() && !self.custom_frame() {
                    self.custom_size = self.frame();
                }
                self.size = index;
            }
            StartMsg::RateChanged(index) => {
                let index = (index.max(0) as usize).min(RATES.len());
                if index == RATES.len() && !self.custom_rate_on() {
                    self.custom_rate = self.rate();
                }
                self.rate = index;
            }
            StartMsg::CustomWidth(width) => {
                self.custom_size = custom_frame(width, self.custom_size.1 as f32);
            }
            StartMsg::CustomHeight(height) => {
                self.custom_size = custom_frame(self.custom_size.0 as f32, height);
            }
            StartMsg::CustomFps(fps) => self.custom_rate = custom_rate(f64::from(fps)),
            StartMsg::CanvasChanged(canvas) => self.canvas = canvas,
            StartMsg::DismissError => self.error.clear(),
            // The picker runs with the studio unborrowed and answers as a
            // message of its own; see `host::after_dialog`.
            StartMsg::Browse => {
                let title = t("start.whereShouldProjectFolder");
                let current = self.location.clone();
                crate::host::after_dialog(
                    move || platform::pick_folder(&title, &current),
                    |studio, _, _, folder| {
                        studio.handle(crate::panes::Msg::Start(StartMsg::FolderChosen(
                            folder.to_string_lossy().into_owned(),
                        )));
                    },
                );
            }
            StartMsg::FolderChosen(folder) => self.location = folder,
            StartMsg::Create => self.begin_draft(studio, Vec::new()),
            StartMsg::Open => self.open(),
            StartMsg::OpenChosen(folder) => {
                self.retire_request();
                self.dropped.clear();
                Self::open_chosen(&folder, &self.location, studio);
            }
            StartMsg::ImportCapcut => {
                let title = t("start.importCapcut");
                let drafts = concat_host::capcut::drafts_folder()
                    .map(|folder| folder.to_string_lossy().into_owned())
                    .unwrap_or_default();
                crate::host::after_dialog(
                    move || platform::pick_folder(&title, &drafts),
                    |studio, _, _, folder| {
                        studio.handle(crate::panes::Msg::Start(StartMsg::OpenChosen(folder)));
                    },
                );
            }
            // Both of the grid's verbs report through the toast, as Open
            // does: the sheet is down while a card is pressed, and a notice
            // inside it would go unseen.
            StartMsg::OpenRecent(path) => {
                self.retire_request();
                self.dropped.clear();
                // A folder that has gone says so in words, not as the
                // manifest file it could not read.
                if !projects::is_project(Path::new(&path)) {
                    studio.notify(&tf("start.folderMissing", &[&path]), true);
                    studio.recents = projects::recents(&studio.host.dirs.config);
                    return;
                }
                if let Err(error) = studio.open_project_at(&path) {
                    studio.notify(&error, true);
                }
            }
            StartMsg::ForgetRecent(path) => {
                if let Err(error) = projects::forget(&studio.host.dirs.config, &path) {
                    studio.notify(&error, true);
                }
                studio.recents = projects::recents(&studio.host.dirs.config);
            }
            StartMsg::RevealRecent(path) => {
                if let Err(error) = platform::reveal(&path) {
                    studio.notify(&error, true);
                }
            }
            StartMsg::SearchEdited(query) => self.query = query,
            StartMsg::Sort(sort) => {
                self.sort = if sort == SORT_NAME {
                    SORT_NAME
                } else {
                    SORT_RECENT
                };
            }
            StartMsg::Link(name) => {
                if name == "shortcuts" {
                    use crate::panes::settings::{SHORTCUTS_PAGE, SettingsMsg};
                    studio.handle(crate::panes::Msg::Settings(SettingsMsg::Open));
                    studio.handle(crate::panes::Msg::Settings(SettingsMsg::PageChanged(
                        SHORTCUTS_PAGE,
                    )));
                    return;
                }
                let Some(url) = link(&name) else {
                    log::warn!(
                        "start: the launch screen asked for a link called {name:?}, which there is none of; nothing opened"
                    );
                    return;
                };
                if let Err(error) = platform::open_link(&url) {
                    studio.notify(&tf("start.couldNotOpenLink", &[&url, &error]), true);
                }
            }
            StartMsg::Dropped(paths) => self.dropped_on(paths, studio),
        }
    }

    /// An explicit project folder opens. Media starts a persistent desktop
    /// draft, or goes through the same validation as an accepted phone
    /// selection before a phone draft can be created.
    fn dropped_on(&mut self, paths: Vec<PathBuf>, studio: &mut Studio) {
        // An explicit project always wins over a pending automatic draft.
        if let Some(folder) = paths
            .iter()
            .find(|path| projects::is_project(path) || concat_host::capcut::is_draft(path))
        {
            self.retire_request();
            self.dropped.clear();
            self.composing = false;
            Self::open_chosen(folder, &self.location, studio);
            return;
        }
        let media: Vec<PathBuf> = paths.into_iter().filter(|path| path.is_file()).collect();
        if media.is_empty() {
            studio.notify(&t("start.dropNothingToOpen"), true);
            return;
        }
        // Paths already supplied by the platform must not open another
        // picker. Phone drops use the same request and validation path as
        // an accepted selection, including cancellation and epoch checks.
        if platform::phone() {
            if self.busy || self.composing {
                // Keep a phone selection bound to its own request. A
                // second drop cannot silently survive cancellation and
                // enter a later project without the initial probe.
                studio.notify(&t("import.title"), false);
                return;
            }
            self.dropped.clear();
            self.error.clear();
            self.busy = true;
            self.request = self.request.wrapping_add(1);
            self.prepare_initial_paths(self.request, crate::host::project_epoch(), media);
            return;
        }
        // A second desktop drop during a pending draft joins the queued import.
        for path in media {
            if !self.dropped.contains(&path) {
                self.dropped.push(path);
            }
        }
        if !self.busy && !self.composing {
            self.update(StartMsg::Compose, studio);
        }
    }

    pub(crate) fn retire_imports(&mut self) {
        platform::cancel_file_pick();
        self.retire_request();
        self.dropped.clear();
        self.composing = false;
    }

    fn retire_request(&mut self) {
        if let Some(job) = self.import_job.take() {
            job.cancel();
        }
        self.request = self.request.wrapping_add(1);
        self.busy = false;
    }

    /// The normal entry: a desk enters a persistent draft; a phone first
    /// selects and validates sources without creating a project.
    fn compose(&mut self, studio: &mut Studio, files: bool) {
        if self.busy {
            return;
        }
        self.error.clear();
        if platform::phone() {
            self.busy = true;
            self.request = self.request.wrapping_add(1);
            let request = self.request;
            let epoch = crate::host::project_epoch();
            let title = t("lib.importMedia");
            let visual: &[&str] = &["mp4", "jpg", "png"];
            platform::pick_files_async(
                &title,
                if files { None } else { Some(("Media", visual)) },
                move |picked| {
                    crate::host::on_ui(move |studio, _, _| {
                        if studio.start.request != request || crate::host::project_epoch() != epoch
                        {
                            return;
                        }
                        let paths = match picked.accept() {
                            Ok(Some(paths)) => paths,
                            Ok(None) => {
                                studio.start.busy = false;
                                return;
                            }
                            Err(error) => {
                                studio.start.busy = false;
                                studio.notify(&crate::platform::pick_error_message(&error), true);
                                return;
                            }
                        };
                        studio.start.prepare_initial_paths(request, epoch, paths);
                    });
                },
            );
            return;
        }
        if studio
            .session
            .as_ref()
            .is_some_and(|session| session.is_unused_draft())
            && !studio.script.has_user_work()
            && !agent_has_user_work()
        {
            studio.on_start = false;
            let dropped = std::mem::take(&mut self.dropped);
            if !dropped.is_empty() {
                studio.handle(crate::panes::Msg::Media(
                    crate::panes::media_bin::MediaMsg::Import(dropped),
                ));
            }
            return;
        }
        if studio.session.is_none()
            && let Some(path) = studio.prefs.empty_draft_path.clone()
            && projects::drafts::reusable(&path)
        {
            match studio.open_project_at(&path) {
                Ok(()) => {
                    self.composing = false;
                    let dropped = std::mem::take(&mut self.dropped);
                    if !dropped.is_empty() {
                        studio.handle(crate::panes::Msg::Media(
                            crate::panes::media_bin::MediaMsg::Import(dropped),
                        ));
                    }
                    return;
                }
                Err(error) => {
                    studio.notify(&error, true);
                    return;
                }
            }
        }
        self.location = effective_location(studio);
        self.begin_draft(studio, Vec::new());
    }

    /// Both an accepted picker result and an explicit platform media drop
    /// must finish the same asynchronous probe before a phone makes a draft.
    fn prepare_initial_paths(&mut self, request: u64, epoch: u64, paths: Vec<PathBuf>) {
        if request != self.request || epoch != crate::host::project_epoch() {
            return;
        }
        let failures = crate::panes::media_bin::take_platform_import_failures(&paths);
        if paths.is_empty() && failures.is_empty() {
            self.busy = false;
            return;
        }
        let job = ImportJob::new(paths.len());
        self.import_job = Some(job.clone());
        crate::host::spawn_long(
            move || {
                let mut results = Vec::with_capacity(paths.len() + failures.len());
                for (index, path) in paths.iter().enumerate() {
                    if job.cancelled() {
                        return Vec::new();
                    }
                    results.push(concat_host::media::probe(&path.to_string_lossy()));
                    if job.cancelled() {
                        return Vec::new();
                    }
                    job.progress(index + 1, paths.len());
                }
                results.extend(failures.into_iter().map(Err));
                results
            },
            move |studio, _, _, results| {
                studio.handle(crate::panes::Msg::Start(StartMsg::Prepared {
                    request,
                    epoch,
                    results,
                }));
            },
        );
    }

    fn begin_draft(
        &mut self,
        studio: &mut Studio,
        imports: Vec<Result<concat_host::media::MediaSummary, String>>,
    ) {
        if self.busy {
            return;
        }
        if platform::phone() && imports.is_empty() {
            self.compose(studio, true);
            return;
        }
        if platform::phone() || self.location.trim().is_empty() {
            self.location = effective_location(studio);
        }
        self.busy = true;
        self.composing = false;
        self.request = self.request.wrapping_add(1);
        let request = self.request;
        let epoch = crate::host::project_epoch();
        let location = self.location.clone();
        let defaults = studio.prefs.new_project_defaults.clone();
        let base = format!("{} {}", untitled(), local_stamp());
        crate::host::spawn(
            move || {
                if let Some(info) = projects::drafts::take_unopened(
                    &location,
                    defaults.video(),
                    defaults.match_media,
                )? {
                    return Ok(info);
                }
                let mut number = 1_u64;
                let name = loop {
                    let candidate = if number == 1 {
                        base.clone()
                    } else {
                        format!("{base} {number}")
                    };
                    if !Path::new(&location)
                        .join(projects::folder_name(&candidate))
                        .exists()
                    {
                        break candidate;
                    }
                    number += 1;
                };
                projects::create_draft(&location, &name, defaults.video(), defaults.match_media)
            },
            move |studio, _, _, result| {
                studio.handle(crate::panes::Msg::Start(StartMsg::DraftReady {
                    request,
                    epoch,
                    result,
                    imports,
                }));
            },
        );
    }

    /// Opens a project folder that already exists.
    ///
    /// The folder is checked before it is read, so picking the wrong one
    /// says which folder and what was wrong with it rather than reporting a
    /// missing file by its path — which is what `projects::open` has to say
    /// about a folder that was never a project in the first place.
    ///
    /// This is also what File › Open project does, from the menu bar of a
    /// window that already has a project in it. That is why it reports
    /// through the toast rather than through the sheet's notice: the sheet
    /// is down when this is pressed, and half the presses of this never see
    /// the launch screen at all.
    ///
    /// The picker runs with the studio unborrowed, as Browse's does, and
    /// the folder comes back as [`StartMsg::OpenChosen`]; see
    /// `host::after_dialog`.
    fn open(&self) {
        let title = t("start.openAProject");
        let current = self.location.clone();
        crate::host::after_dialog(
            move || platform::pick_folder(&title, &current),
            |studio, _, _, folder| {
                studio.handle(crate::panes::Msg::Start(StartMsg::OpenChosen(folder)));
            },
        );
    }

    /// The folder Open was answered with: checked, then opened. A CapCut
    /// draft is made a project of first, under `location` - where a new
    /// project goes - and the draft itself is left as it was (#477).
    fn open_chosen(folder: &Path, location: &str, studio: &mut Studio) {
        let path = folder.to_string_lossy().into_owned();
        if !projects::is_project(folder) && concat_host::capcut::is_draft(folder) {
            Self::import_capcut(folder, location, studio);
            return;
        }
        if !projects::is_project(folder) {
            studio.notify(&tf("start.notConcatProjectFolder", &[&path]), true);
            return;
        }
        if let Err(error) = studio.open_project_at(&path) {
            studio.notify(&error, true);
        }
    }

    /// Makes a project of the CapCut draft in `folder` and opens it, saying
    /// how much of it came across: what did not is named in the log.
    fn import_capcut(folder: &Path, location: &str, studio: &mut Studio) {
        let imported = concat_host::capcut::import(folder, location);
        match imported {
            Ok(import) => {
                let left = import.notes.len();
                if let Err(error) = studio.open_project(import.project) {
                    studio.notify(&error, true);
                    return;
                }
                let said = if left == 0 {
                    t("start.capcutImported")
                } else {
                    tf("start.capcutImportedSome", &[&left])
                };
                studio.notify(&said, false);
            }
            Err(error) => {
                log::error!(
                    "project: making a project of the draft {} failed: {error}; nothing was made",
                    folder.display()
                );
                studio.notify(&tf("start.capcutImportFailed", &[&error]), true);
            }
        }
    }

    /// The first untitled name with no folder of that name where the sheet
    /// says the project goes. Any folder counts, not only a project: a new
    /// project is not made inside something that is already there.
    #[cfg(test)]
    fn free_untitled(&self) -> String {
        let location = self.location.trim();
        if location.is_empty() {
            return untitled();
        }
        first_free_untitled(|folder| Path::new(location).join(folder).exists())
    }

    /// Whether the frame is typed rather than picked.
    fn custom_frame(&self) -> bool {
        self.size >= SIZES.len()
    }

    /// Whether the rate is typed rather than picked.
    fn custom_rate_on(&self) -> bool {
        self.rate >= RATES.len()
    }

    /// The frame the sheet describes: the typed one, or the shape at the
    /// size.
    fn frame(&self) -> (u32, u32) {
        if self.custom_frame() {
            self.custom_size
        } else {
            frame_size(self.aspect, self.size)
        }
    }

    /// The rate the sheet describes, as an exact fraction.
    fn rate(&self) -> (i64, i64) {
        if self.custom_rate_on() {
            self.custom_rate
        } else {
            let (_, num, den) = RATES[self.rate];
            (num, den)
        }
    }

    /// The sheet as Slint shows it.
    pub fn data(&self) -> StartData {
        let (width, height) = self.frame();
        let (num, den) = self.rate();
        StartData {
            composing: self.composing,
            name: self.name.as_str().into(),
            location: self.location.as_str().into(),
            aspect: self.aspect as i32,
            size: self.size as i32,
            rate: self.rate as i32,
            size_readout: format!("{width} x {height}").into(),
            canvas: slint::Color::from_argb_u8(
                self.canvas[3],
                self.canvas[0],
                self.canvas[1],
                self.canvas[2],
            ),
            custom_width: width as f32,
            custom_height: height as f32,
            custom_fps: fps_of(num, den) as f32,
            frame_aspect: width as f32 / height.max(1) as f32,
            rate_readout: format!("{num}/{den} fps").into(),
            rate_options: slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(
                RATES
                    .iter()
                    .map(|(label, _, _)| slint::SharedString::from(format!("{label} fps")))
                    .chain(std::iter::once(t("common.custom").into()))
                    .collect::<Vec<_>>(),
            ))),
            busy: self.busy,
            error: self.error.as_str().into(),
            dropped: self.dropped.len() as i32,
            query: self.query.as_str().into(),
            sort: self.sort,
        }
    }
}

/// Mobile roots remain the installed application's existing persistent mapping.
pub(crate) fn effective_location(studio: &Studio) -> String {
    if platform::phone() {
        return default_location();
    }
    studio
        .prefs
        .new_project_defaults
        .location
        .clone()
        .or_else(|| studio.prefs.project_location().map(str::to_owned))
        .unwrap_or_else(default_location)
}

fn agent_has_user_work() -> bool {
    let mut has_work = false;
    crate::host::Shell::with(|shell, _| {
        has_work = shell
            .agent_pane
            .try_borrow()
            .map_or(true, |pane| pane.has_user_work());
    });
    has_work
}

fn local_stamp() -> String {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetLocalTime(value: *mut [u16; 8]);
        }
        let mut value = [0_u16; 8];
        // SYSTEMTIME is eight consecutive WORD values owned by this call.
        unsafe {
            GetLocalTime(&mut value);
        }
        return format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            value[0], value[1], value[3], value[4], value[5], value[6]
        );
    }
    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    {
        let seconds = unsafe { libc::time(std::ptr::null_mut()) };
        let mut value = std::mem::MaybeUninit::<libc::tm>::uninit();
        if !unsafe { libc::localtime_r(&seconds, value.as_mut_ptr()) }.is_null() {
            let value = unsafe { value.assume_init() };
            return format!(
                "{:04}{:02}{:02}-{:02}{:02}{:02}",
                value.tm_year + 1900,
                value.tm_mon + 1,
                value.tm_mday,
                value.tm_hour,
                value.tm_min,
                value.tm_sec
            );
        }
    }
    #[allow(unreachable_code)]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An iPhone's projects go in its Documents, whatever a desktop would
    /// have said; a desktop's go on the desktop the system names.
    #[test]
    fn each_platform_has_a_place_for_new_projects() {
        let home = |name: &str| format!("/home/{name}");
        let desktop = || Some(PathBuf::from("/desk"));
        assert_eq!(
            location_on("ios", desktop(), home),
            "/home/Documents/Concat"
        );
        assert_eq!(location_on("ios", None, home), "/home/Documents/Concat");
        assert_eq!(location_on("android", None, home), "/home/Concat");
        assert_eq!(
            PathBuf::from(location_on("macos", desktop(), home)),
            Path::new("/desk").join("BK CUT").join("Projects")
        );
        assert_eq!(
            location_on("linux", None, home),
            "/home/Documents/BK CUT/Projects"
        );
    }

    /// The next untitled name skips every folder already there, and starts
    /// over at the plain name once that is free again.
    #[test]
    fn a_second_untitled_project_gets_a_name_of_its_own() {
        // The name is the interface's, so the test holds English while it
        // reads it: another test may choose a language at any moment.
        let _english = crate::i18n::language_lock();
        assert_eq!(first_free_untitled(|_| false), "Untitled project");
        let taken = ["Untitled project", "Untitled project 2"];
        assert_eq!(
            first_free_untitled(|folder| taken.contains(&folder)),
            "Untitled project 3"
        );
        let taken = ["Untitled project 2"];
        assert_eq!(
            first_free_untitled(|folder| taken.contains(&folder)),
            "Untitled project"
        );
    }

    /// The search finds by every word, in the name or the folder, and the
    /// sort by name ignores case; the list's own order is the recent one.
    #[test]
    fn the_projects_search_and_sort() {
        let recent = |name: &str, path: &str| projects::Recent {
            info: projects::ProjectInfo {
                canvas: [0, 0, 0, 255],
                path: path.into(),
                name: name.into(),
                width: 1920,
                height: 1080,
                rate_num: 30,
                rate_den: 1,
                opened_at: 0,
            },
            present: true,
            edited_at: None,
        };
        let list = [
            recent("beach vlog", "/v/summer/beach"),
            recent("Podcast 12", "/v/pods/12"),
            recent("Apple ad", "/v/clients/apple"),
        ];
        let names = |shown: Vec<&projects::Recent>| -> Vec<String> {
            shown
                .iter()
                .map(|recent| recent.info.name.clone())
                .collect()
        };
        assert_eq!(
            names(shown(&list, "", SORT_RECENT)),
            ["beach vlog", "Podcast 12", "Apple ad"]
        );
        assert_eq!(
            names(shown(&list, "", SORT_NAME)),
            ["Apple ad", "beach vlog", "Podcast 12"]
        );
        assert_eq!(names(shown(&list, "SUMMER", SORT_RECENT)), ["beach vlog"]);
        assert_eq!(names(shown(&list, "pod 12", SORT_RECENT)), ["Podcast 12"]);
        assert!(shown(&list, "nothing", SORT_RECENT).is_empty());
    }

    /// Every link the launch screen sends goes somewhere, and the release
    /// notes point to the product history, including unpublished dev builds.
    #[test]
    fn every_link_has_an_address() {
        for name in [
            "guides",
            "first-edit",
            "report",
            "github",
            "discord",
            "news",
            "release-notes",
        ] {
            assert!(
                link(name).is_some_and(|url| url.starts_with("https://")),
                "{name}"
            );
        }
        assert_eq!(
            link("release-notes").unwrap(),
            format!("{REPOSITORY}/releases")
        );
        assert_eq!(link("nowhere"), None);
    }

    /// Only a name the sheet made up is replaced; a typed one is kept.
    #[test]
    fn only_a_made_up_name_moves_on() {
        let _english = crate::i18n::language_lock();
        for name in [
            "",
            "  ",
            "Untitled project",
            "Untitled project 2",
            "Untitled project 41",
        ] {
            assert!(is_untitled(name), "{name:?}");
        }
        for name in [
            "My film",
            "Untitled project 1",
            "Untitled project two",
            "Untitled projects",
            "Untitled project 2 final",
        ] {
            assert!(!is_untitled(name), "{name:?}");
        }
    }

    /// Composing again after a project was made in the same place offers
    /// the next free name, not the one that would be refused.
    #[test]
    fn compose_offers_a_name_that_is_free_in_the_location() {
        let _english = crate::i18n::language_lock();
        let root = std::env::temp_dir().join(format!("concat-untitled-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Untitled project")).unwrap();
        let pane = StartPane {
            location: root.to_string_lossy().into_owned(),
            ..StartPane::default()
        };
        assert_eq!(pane.free_untitled(), "Untitled project 2");
        let _ = std::fs::remove_dir_all(&root);
    }
}
