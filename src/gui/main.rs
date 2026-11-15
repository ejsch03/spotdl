//! spotdl's desktop app: search Spotify and download what you find.
//!
//! The window and all UI state live on the main thread. Network work runs on a tokio
//! runtime; the UI thread awaits its results with `slint::spawn_local`, so state never
//! needs locking. Downloads go through one [`Downloader`] on a single worker task, in
//! order, so the account sees exactly the same pacing as with the command line.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod instance;
mod prefs;
mod saved_queue;
mod theme;
mod tray;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use futures_util::{StreamExt, stream};
use slint::{
    ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel,
};
use tokio::sync::{mpsc, watch};

use spotdl::auth::{self, Auth, Connection};
use spotdl::cli::{DownloadArgs, Format, GlobalArgs};
use spotdl::downloaded::Checker;
use spotdl::engine::{self, Downloader, Event, Reporter, Settings, Stats, TrackState, count};
use spotdl::input::{self, Input, Target};
use spotdl::login::{self, BrowserLogin};
use spotdl::meta::{Meta, TrackInfo};
use spotdl::output;
use spotdl::search::{self, Hit, Kind};
use spotdl::ui::human_duration;

use prefs::Prefs;
use saved_queue::{Finished, SavedJob, SavedQueue};

slint::include_modules!();

/// The formats in the order of the window's format menu.
const FORMATS: [Format; 4] = [Format::Ogg, Format::Flac, Format::Mp3, Format::Opus];

/// The longest a search may take before the app gives up on it.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Thumbnails are shown at 44 px; decoding them at 96 keeps them sharp on 2x screens.
const THUMBNAIL_PX: usize = 96;
/// How many thumbnails to download at once.
const THUMBNAIL_FETCHES: usize = 6;
/// Thumbnails kept in memory (about 36 KB each) before unused ones are dropped.
const THUMBNAIL_CACHE: usize = 300;

/// How long typing in the folder box has to pause before the folder counts as changed.
const FOLDER_SETTLE: Duration = Duration::from_millis(500);
/// Matches the installed `spotdl.desktop`, which is how Wayland desktops find the app's
/// name and icon for its window.
const APP_ID: &str = "spotdl";

/// How often the tray icon's status is brought up to date.
const TRAY_REFRESH: Duration = Duration::from_secs(2);

/// The `spotdl` download options and environment variables work here too and set the
/// app's defaults; any inputs given are queued once logged in.
#[derive(Parser)]
#[command(
    name = "spotdl-gui",
    version,
    about = "Search Spotify and download songs, albums, playlists and artists"
)]
struct Args {
    #[command(flatten)]
    download: DownloadArgs,
    #[command(flatten)]
    global: GlobalArgs,
}

fn main() -> std::process::ExitCode {
    // Slint draws its own menus (the Queue menu, text boxes' Cut/Copy/Paste) instead of
    // Windows' native ones. Native menus go through one event handler for the whole
    // program, which can be set only once, and the notification area icon's menu needs
    // it (see `tray`).
    #[cfg(windows)]
    // SAFETY: nothing else runs yet (no other thread could be reading the environment),
    // and Windows synchronizes access to it anyway.
    unsafe {
        std::env::set_var("SLINT_NO_MUDA", "1");
    }
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            let message = format!("spotdl ran into a problem: {e:#}");
            eprintln!("{message}");
            // started from a menu, the Dock or the Start menu, there's no terminal to see
            // it in (and on Windows, no terminal at all)
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("spotdl")
                .set_description(message)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    spotdl::tune_allocator();
    let matches = Args::command().get_matches();
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    spotdl::ui::init_logging(args.global.verbose);
    let auth = Auth::new(args.global.config_dir.as_deref())?;
    let prefs = Prefs::load(auth.dir());
    let mut settings = Settings::from_args(&args.download)?;
    // what was chosen in the window last time, unless told otherwise for this run
    if !given(&matches, "output") {
        // (a desktop app's working directory is rarely where music should go)
        settings.output = prefs.output().map_or_else(default_music_dir, PathBuf::from);
    }
    if !given(&matches, "format")
        && let Some(format) = prefs.format()
    {
        settings.format = format;
    }
    // after a restart (see `restart_with_software_renderer`), what was given is in the
    // saved Downloads list already
    let startup = if std::env::var_os(RESTARTED).is_some() {
        Vec::new()
    } else {
        input::collect(&args.download.inputs, args.download.input_file.as_deref())?
    };
    // already running? then that one shows its window and queues these, and that's all
    let texts: Vec<String> = startup.iter().map(|input| input.text.clone()).collect();
    let launches = match instance::claim(auth.dir(), &texts)? {
        instance::Claim::Forwarded => return Ok(()),
        instance::Claim::First(listener) => listener,
    };
    // the saved theme (GitHub's to begin with), in the light or dark half of its pair that
    // matches the system as it's set now
    let theme = theme::Theme::at_launch(prefs.theme(), theme::system_prefers_dark());
    // a broken developer app setup shouldn't stop the app; search falls back to songs
    let (web, web_problem) = match search::WebApi::load(auth.dir()) {
        Ok(web) => (web.map(Arc::new), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };

    let rt = spotdl::runtime().context("starting the async runtime")?;
    let window = AppWindow::new()?;
    if let Err(e) = slint::set_xdg_app_id(APP_ID) {
        log::debug!("couldn't set the app id: {e}");
    }
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
    let (pause, paused) = watch::channel(false);
    let conn = Arc::new(OnceLock::new());
    rt.spawn(worker(
        conn.clone(),
        settings.clone(),
        jobs_rx,
        paused,
        window.as_weak(),
    ));

    let app = Rc::new(App {
        window: window.as_weak(),
        rt: rt.handle().clone(),
        auth,
        conn,
        web: RefCell::new(web),
        web_problem: RefCell::new(web_problem),
        images: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()?,
        jobs_tx,
        pause,
        tray: tray::Tray::start(window.as_weak()),
        tray_timer: Timer::default(),
        settings: settings.clone(),
        prefs: RefCell::new(prefs),
        checker: Default::default(),
        folder_timer: Timer::default(),
        results: Rc::new(VecModel::default()),
        jobs: Rc::new(VecModel::default()),
        state: RefCell::default(),
    });
    app.init(&window, &settings, theme);
    app.restore_queue(SavedQueue::load(app.auth.dir()), startup);
    let mut running = None;
    if let Some(launches) = launches {
        let (tx, mut rx) = mpsc::unbounded_channel();
        running = Some(launches.serve(move |inputs| _ = tx.send(inputs)));
        let reopen = app.clone();
        app.on_ui(async move {
            while let Some(inputs) = rx.recv().await {
                reopen.reopen(inputs);
            }
        });
    }

    // closing the window hides it in the tray, if there is one; downloads carry on
    let tray_app = app.clone();
    window.window().on_close_requested(move || {
        if !tray_app.tray.is_showing() {
            _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
    let ran = window
        .show()
        .and_then(|()| slint::run_event_loop_until_quit());
    _ = window.hide();
    app.tray.remove();
    // the window closed while a typed folder was still settling
    if app.folder_timer.running() {
        app.remember_folder();
    }
    // with the latest titles; what was downloading carries on next time
    app.save_queue();

    // dropping the running download removes its partial file; give blocking work
    // (tagging, flushing to disk) a moment to finish
    rt.shutdown_timeout(Duration::from_secs(2));

    match ran {
        Ok(()) => Ok(()),
        // Most likely the window couldn't be drawn: OpenGL isn't there or isn't up to it,
        // as in virtual machines, remote sessions and on some Windows on ARM computers.
        // Slint picks its renderer once per process, so start again with the software one.
        // (Downloads are stopped and the list saved above, and letting go of being the
        // running app comes first, so the two never overlap.)
        Err(e) if std::env::var_os("SLINT_BACKEND").is_none() => {
            log::warn!("couldn't show the window ({e}); starting again with the software renderer");
            drop(running);
            restart_with_software_renderer()
        }
        Err(e) => Err(e.into()),
    }
}

/// Set for a restart with the software renderer.
const RESTARTED: &str = "SPOTDL_RESTARTED";

/// Starts the app again, with the same options, drawing its window with Slint's software
/// renderer, which needs no GPU.
fn restart_with_software_renderer() -> Result<()> {
    std::process::Command::new(std::env::current_exe().context("finding spotdl-gui")?)
        .args(std::env::args_os().skip(1))
        .env("SLINT_BACKEND", "winit-software")
        .env(RESTARTED, "1")
        .spawn()
        .context("starting spotdl again with the software renderer")?;
    Ok(())
}

/// Whether an option was given, on the command line or through its SPOTDL_* variable,
/// rather than left at its default.
fn given(matches: &clap::ArgMatches, id: &str) -> bool {
    matches
        .value_source(id)
        .is_some_and(|source| source != ValueSource::DefaultValue)
}

fn default_music_dir() -> PathBuf {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Music")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `~/Music` → `/home/me/Music`
fn expand_home(path: &str) -> PathBuf {
    output::expand_home(Path::new(path))
}

// ---------------------------------------------------------------------------------------
// The UI thread

struct App {
    window: slint::Weak<AppWindow>,
    rt: tokio::runtime::Handle,
    auth: Auth,
    /// Set once logged in; shared with the download worker.
    conn: Arc<OnceLock<Arc<Connection>>>,
    /// The user's developer app, for searching everything through the Web API. It can
    /// be set up (or changed) in Settings.
    web: RefCell<Option<Arc<search::WebApi>>>,
    /// Why the saved developer app couldn't be loaded, if it couldn't.
    web_problem: RefCell<Option<String>>,
    /// For thumbnails, which come from Spotify's public image CDN (no login involved).
    images: reqwest::Client,
    jobs_tx: mpsc::UnboundedSender<Job>,
    /// Pauses (and resumes) the download worker.
    pause: watch::Sender<bool>,
    tray: tray::Tray,
    tray_timer: Timer,
    /// The download options the app started with; the window picks folder and format.
    settings: Settings,
    /// The folder, format and theme chosen in the window, kept for next time.
    prefs: RefCell<Prefs>,
    /// Tells which results are already downloaded. Only one check runs at a time.
    checker: Arc<tokio::sync::Mutex<Option<Checker>>>,
    /// Waits for typing in the folder box to pause.
    folder_timer: Timer,
    results: Rc<VecModel<ResultRow>>,
    jobs: Rc<VecModel<JobRow>>,
    state: RefCell<State>,
}

/// What the results list is showing.
#[derive(Default, PartialEq)]
enum Listing {
    #[default]
    Nothing,
    Search(String),
    /// The playlists in the user's library.
    Playlists,
}

#[derive(Default)]
struct State {
    /// Everything the last search found, in Spotify's order.
    hits: Vec<Hit>,
    listing: Listing,
    /// Why only songs were searched, if the Web API failed.
    note: Option<String>,
    /// Which hit each visible row shows.
    shown: Vec<usize>,
    /// Which hits are already in the download folder, by hit.
    downloaded: Vec<bool>,
    /// Bumped for every check of the hits against the download folder.
    check_generation: u64,
    check_task: Option<tokio::task::AbortHandle>,
    /// Links of hits waiting in or running through the queue, with their job, so their
    /// button says so until the job is over.
    queued: HashMap<String, i32>,
    /// How to cancel each job that may still be running.
    cancels: HashMap<i32, watch::Sender<bool>>,
    next_job: i32,
    /// Bumped for every search, so a slow older one can't overwrite a newer one.
    search_generation: u64,
    search: Option<tokio::task::AbortHandle>,
    /// Decoded thumbnails by URL.
    thumbnails: HashMap<String, slint::Image>,
    /// Downloads the current results' thumbnails.
    thumbnail_task: Option<tokio::task::AbortHandle>,
    /// What each listed job downloads, and where, for saving the list.
    specs: HashMap<i32, JobSpec>,
    /// Jobs waiting for the login before they can go to the worker.
    held: Vec<Job>,
    /// The browser login under way, if one is.
    login: Option<LoginInProgress>,
}

struct LoginInProgress {
    /// Stops waiting for the browser (and frees the port).
    task: tokio::task::AbortHandle,
    /// The address the login ended on, when pasted into the window.
    paste: mpsc::UnboundedSender<String>,
}

struct JobSpec {
    /// The link (or other input) as typed or as the result gave it.
    input: String,
    output: PathBuf,
    format: Format,
}

impl App {
    fn init(self: &Rc<Self>, window: &AppWindow, settings: &Settings, theme: &theme::Theme) {
        let names: Vec<slint::SharedString> = theme::THEMES
            .iter()
            .map(|theme| theme.name.into())
            .collect();
        window.set_theme_names(ModelRc::new(VecModel::from(names)));
        show_theme(window, theme);
        window.set_results(ModelRc::from(self.results.clone()));
        window.set_jobs(ModelRc::from(self.jobs.clone()));
        window.set_output_dir(settings.output.display().to_string().into());
        let format = FORMATS.iter().position(|f| *f == settings.format);
        window.set_format_index(format.unwrap_or(0) as i32);
        window.set_search_status(self.idle_status().into());
        let saved_login = self.auth.is_logged_in().unwrap_or(false);
        window.set_has_saved_login(saved_login);

        let app = self.clone();
        window.on_log_in(move || app.log_in());
        let app = self.clone();
        window.on_reconnect(move || app.connect());
        let app = self.clone();
        window.on_cancel_login(move || app.cancel_login());
        let app = self.clone();
        window.on_paste_login_address(move |text| app.paste_login_address(&text));
        let app = self.clone();
        window.on_open_login_page(move || {
            if let Some(window) = app.window.upgrade() {
                login::open_in_browser(&window.get_login_url());
            }
        });
        let app = self.clone();
        window.on_open_settings(move || app.open_settings());
        let app = self.clone();
        window.on_save_developer_app(move || app.save_developer_app());
        let app = self.clone();
        window.on_remove_developer_app(move || app.remove_developer_app());
        window
            .on_open_developer_dashboard(|| _ = login::open_in_browser(login::DEVELOPER_DASHBOARD));
        let app = self.clone();
        window.on_search(move |query| app.search(&query));
        let app = self.clone();
        window.on_filter_changed(move |_| app.show_hits());
        let app = self.clone();
        window.on_download(move |row| app.download_hit(row as usize));
        let app = self.clone();
        window.on_show_playlists(move || app.show_playlists());
        let app = self.clone();
        window.on_cancel_job(move |id| app.cancel(id));
        let app = self.clone();
        window.on_clear_finished(move || app.clear_finished());
        let app = self.clone();
        window.on_pause_all(move || app.set_paused(true));
        let app = self.clone();
        window.on_resume_all(move || app.set_paused(false));
        let app = self.clone();
        window.on_cancel_all(move || app.cancel_all());
        let app = Rc::downgrade(self);
        self.tray_timer
            .start(TimerMode::Repeated, TRAY_REFRESH, move || {
                if let Some(app) = app.upgrade() {
                    app.tray.set_status(app.tray_status());
                }
            });
        let app = self.clone();
        window.on_job_finished(move |id| app.job_finished(id));
        let app = self.clone();
        window.on_choose_folder(move || app.choose_folder());
        let app = Rc::downgrade(self);
        window.on_output_dir_edited(move || {
            let Some(app) = app.upgrade() else {
                return;
            };
            let settled = Rc::downgrade(&app);
            app.folder_timer
                .start(TimerMode::SingleShot, FOLDER_SETTLE, move || {
                    if let Some(app) = settled.upgrade() {
                        app.folder_changed();
                    }
                });
        });
        let app = self.clone();
        window.on_format_changed(move || app.format_changed());
        let app = self.clone();
        window.on_theme_changed(move |index| app.theme_changed(index));
        let app = self.clone();
        window.on_open_folder(move || app.open_folder());

        if saved_login {
            self.connect();
        }
    }

    /// Connects with the saved login.
    fn connect(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        window.set_busy_connecting(true);
        window.set_login_error("".into());
        let auth = self.auth.clone();
        let task = self.rt.spawn(async move { auth.connect().await });
        let app = self.clone();
        self.on_ui(async move {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            app.connected(result, false);
        });
    }

    /// Logs in through the browser. The window shows the login page's link (for another
    /// browser), takes the address the login ended on if it's pasted, and can cancel.
    fn log_in(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        window.set_busy_connecting(true);
        window.set_login_error("".into());
        let start = self.rt.spawn(BrowserLogin::start());
        let app = self.clone();
        self.on_ui(async move {
            let login = match start.await.map_err(anyhow::Error::from).and_then(|r| r) {
                Ok(login) => login,
                Err(e) => return app.connected(Err(e), true),
            };
            let url = login.url().to_string();
            if let Some(window) = app.window.upgrade() {
                window.set_login_url(url.as_str().into());
                window.set_login_paste("".into());
            }
            login::open_in_browser(&url);

            let (paste, pasted) = mpsc::unbounded_channel();
            let auth = app.auth.clone();
            let task = app
                .rt
                .spawn(async move { auth.finish_login(login, pasted).await });
            app.state.borrow_mut().login = Some(LoginInProgress {
                task: task.abort_handle(),
                paste,
            });
            let result = task.await;
            app.state.borrow_mut().login = None;
            if let Some(window) = app.window.upgrade() {
                window.set_login_url("".into());
            }
            match result {
                // cancelled from the window, which is already back as it was
                Err(e) if e.is_cancelled() => {}
                result => app.connected(result.map_err(anyhow::Error::from).and_then(|r| r), true),
            }
        });
    }

    fn cancel_login(&self) {
        if let Some(login) = self.state.borrow_mut().login.take() {
            login.task.abort();
        }
        if let Some(window) = self.window.upgrade() {
            window.set_busy_connecting(false);
            window.set_login_url("".into());
            window.set_login_error("".into());
        }
    }

    /// The address the browser ended on, pasted in (when the browser runs elsewhere).
    fn paste_login_address(&self, text: &str) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        if !login::is_reply(text) {
            window.set_login_error(
                "That's not it: paste the whole address of the page the login ended on (it \
                 has “code=” in it)."
                    .into(),
            );
            return;
        }
        window.set_login_error("".into());
        if let Some(login) = &self.state.borrow().login {
            _ = login.paste.send(text.to_string());
        }
    }

    /// Shows the outcome of connecting (or logging in, when `login` is set).
    fn connected(&self, result: Result<librespot_core::Session>, login: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        window.set_busy_connecting(false);
        match result {
            Ok(session) => {
                let account = format!("Logged in as {}", auth::describe(&session));
                window.set_account(account.into());
                _ = self
                    .conn
                    .set(Arc::new(Connection::new(self.auth.clone(), session)));
                window.set_logged_in(true);
                // jobs restored or queued before logging in can start now
                let held = std::mem::take(&mut self.state.borrow_mut().held);
                for job in held {
                    _ = self.jobs_tx.send(job);
                }
            }
            Err(e) => {
                window.set_has_saved_login(self.auth.is_logged_in().unwrap_or(false));
                // the messages from `Auth` point at the command line; say it plainly here
                let message = if login {
                    format!("Logging in didn't work: {}.", e.root_cause())
                } else {
                    format!(
                        "Couldn't connect with the saved login ({}). Log in again, or try once more.",
                        e.root_cause()
                    )
                };
                window.set_login_error(message.into());
            }
        }
    }

    /// Another launch asked for the window, maybe with links to queue.
    fn reopen(&self, inputs: Vec<String>) {
        if let Some(window) = self.window.upgrade() {
            // Wayland desktops only bring a new window to the front, so show it afresh
            // (it may be in the tray, minimized, or behind other windows)
            _ = window.hide();
            if let Err(e) = window.show() {
                log::warn!("couldn't show the window: {e}");
            }
        }
        for text in inputs {
            match input::parse(&text) {
                Ok(target) => _ = self.enqueue(text.clone(), Input { text, target }),
                Err(e) => log::warn!("ignoring {text:?} from another launch: {e}"),
            }
        }
    }

    /// Runs a future on the UI thread, where it can touch the window and the state.
    fn on_ui(&self, future: impl Future<Output = ()> + 'static) {
        if let Err(e) = slint::spawn_local(future) {
            log::error!("couldn't schedule work on the UI thread: {e}");
        }
    }

    fn search(self: &Rc<Self>, query: &str) {
        let query = query.trim().to_string();
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if query.is_empty() {
            return;
        }

        // a pasted link (or URI, or ID) goes straight to the downloads
        match input::parse(&query) {
            Ok(Target::Search(_)) => {}
            Ok(target) => {
                _ = self.enqueue(
                    query.clone(),
                    Input {
                        text: query.clone(),
                        target,
                    },
                );
                window.set_query("".into());
                return;
            }
            Err(e) => {
                self.set_hits(Listing::Search(query), Vec::new(), None, Vec::new());
                window.set_search_status(format!("That link didn't work: {e}").into());
                return;
            }
        }
        let Some(conn) = self.conn.get().cloned() else {
            return;
        };

        let q = query.clone();
        let web = self.web.borrow().clone();
        let task = self.rt.spawn(async move {
            let search = async {
                let meta = Meta::new(conn.session().await?);
                search::search(&meta, web.as_deref(), &q).await
            };
            // a search never spins forever, and giving up also stops any retrying
            tokio::time::timeout(SEARCH_TIMEOUT, search)
                .await
                .unwrap_or_else(|_| Err(anyhow!("Spotify didn't answer in time")))
        });
        let status = format!("Searching for “{query}”…");
        self.load_results(Listing::Search(query), status, task);
    }

    /// Lists the playlists in the user's library.
    fn show_playlists(self: &Rc<Self>) {
        let (Some(window), Some(conn)) = (self.window.upgrade(), self.conn.get().cloned()) else {
            return;
        };
        window.set_filter(0); // a Songs filter would hide them all
        let task = self.rt.spawn(async move {
            let load = async {
                let meta = Meta::new(conn.session().await?);
                anyhow::Ok(search::Results {
                    hits: search::library_playlists(&meta).await?,
                    note: None,
                    tracks: Vec::new(),
                })
            };
            tokio::time::timeout(SEARCH_TIMEOUT, load)
                .await
                .unwrap_or_else(|_| Err(anyhow!("Spotify didn't answer in time")))
        });
        self.load_results(Listing::Playlists, "Loading your playlists…".into(), task);
    }

    /// Empties the results list and fills it with what `task` finds, unless a newer
    /// search (or list) replaces it first.
    fn load_results(
        self: &Rc<Self>,
        listing: Listing,
        status: String,
        task: tokio::task::JoinHandle<Result<search::Results>>,
    ) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let generation = {
            let mut state = self.state.borrow_mut();
            if let Some(previous) = state.search.replace(task.abort_handle()) {
                previous.abort();
            }
            state.search_generation += 1;
            state.shown.clear(); // the list is emptied below
            state.search_generation
        };
        window.set_searching(true);
        self.results.set_vec(Vec::new());
        window.set_search_status(status.into());

        let app = self.clone();
        self.on_ui(async move {
            let result = task.await;
            // a newer search replaced this one (and aborted it); leave the UI to that one
            if app.state.borrow().search_generation != generation {
                return;
            }
            if let Some(window) = app.window.upgrade() {
                window.set_searching(false);
            }
            let error = match result {
                Ok(Ok(results)) => {
                    return app.set_hits(listing, results.hits, results.note, results.tracks);
                }
                Ok(Err(e)) => format!("{e:#}"),
                Err(e) => e.to_string(), // the task itself failed
            };
            let failed = match listing {
                Listing::Playlists => "Couldn't load your playlists",
                _ => "Search failed",
            };
            app.set_hits(listing, Vec::new(), None, Vec::new());
            if let Some(window) = app.window.upgrade() {
                window.set_search_status(format!("{failed}: {error}").into());
            }
        });
    }

    /// Shows a search's results. `tracks` are song details the search already loaded.
    fn set_hits(
        self: &Rc<Self>,
        listing: Listing,
        hits: Vec<Hit>,
        note: Option<String>,
        tracks: Vec<TrackInfo>,
    ) {
        {
            let mut state = self.state.borrow_mut();
            state.downloaded = vec![false; hits.len()];
            state.hits = hits;
            state.listing = listing;
            state.note = note;
        }
        self.show_hits();
        self.load_thumbnails();
        self.check_downloads(tracks);
    }

    /// Works out which results are already in the download folder (in the chosen
    /// format) and greys out their Download buttons. Starts over if a check is running.
    /// `tracks` are song details that are already loaded.
    fn check_downloads(self: &Rc<Self>, tracks: Vec<TrackInfo>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let (generation, targets) = {
            let mut state = self.state.borrow_mut();
            if let Some(task) = state.check_task.take() {
                task.abort();
            }
            state.check_generation += 1;
            let targets: Vec<Target> = state.hits.iter().map(Hit::target).collect();
            (state.check_generation, targets)
        };
        let Some(conn) = self.conn.get().cloned() else {
            return;
        };
        if targets.is_empty() {
            return;
        }
        let mut settings = self.settings.clone();
        settings.output = chosen_output(&window);
        settings.format = chosen_format(&window);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let checker = self.checker.clone();
        let task = self.rt.spawn(async move {
            let session = match conn.session().await {
                Ok(session) => session,
                Err(e) => return log::debug!("not checking for downloads: {e:#}"),
            };
            let mut checker = checker.lock().await;
            let checker = checker.get_or_insert_with(|| Checker::new(session.clone()));
            checker.set_session(session);
            checker.add_tracks(tracks);
            checker
                .check(&targets, &settings, |i, downloaded| {
                    _ = tx.send((i, downloaded));
                })
                .await;
        });
        self.state.borrow_mut().check_task = Some(task.abort_handle());

        let app = self.clone();
        self.on_ui(async move {
            // ends once the check is done or replaced
            while let Some((hit, downloaded)) = rx.recv().await {
                app.set_downloaded(generation, hit, downloaded);
            }
        });
    }

    fn set_downloaded(&self, generation: u64, hit: usize, downloaded: bool) {
        let mut state = self.state.borrow_mut();
        if state.check_generation != generation
            || state.downloaded.get(hit).is_none_or(|d| *d == downloaded)
        {
            return;
        }
        state.downloaded[hit] = downloaded;
        if let Some(row) = state.shown.iter().position(|&shown| shown == hit)
            && let Some(mut data) = self.results.row_data(row)
        {
            data.downloaded = downloaded;
            self.results.set_row_data(row, data);
        }
    }

    /// Downloads the thumbnails the current results need, a few at a time, and puts
    /// each in place as it arrives. Replaces any download still going for older results.
    fn load_thumbnails(self: &Rc<Self>) {
        let urls: Vec<String> = {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            if let Some(task) = state.thumbnail_task.take() {
                task.abort();
            }
            let wanted: HashSet<&str> = state
                .hits
                .iter()
                .filter_map(|h| h.image.as_deref())
                .collect();
            if state.thumbnails.len() > THUMBNAIL_CACHE {
                state
                    .thumbnails
                    .retain(|url, _| wanted.contains(url.as_str()));
            }
            wanted
                .into_iter()
                .filter(|url| !state.thumbnails.contains_key(*url))
                .map(String::from)
                .collect()
        };
        if urls.is_empty() {
            return;
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let http = self.images.clone();
        let task = self.rt.spawn(async move {
            let mut fetches = stream::iter(urls)
                .map(|url| {
                    let http = http.clone();
                    async move {
                        let pixels = fetch_thumbnail(&http, &url).await;
                        (url, pixels)
                    }
                })
                .buffer_unordered(THUMBNAIL_FETCHES);
            while let Some((url, pixels)) = fetches.next().await {
                match pixels {
                    Ok(pixels) => {
                        if tx.send((url, pixels)).is_err() {
                            break;
                        }
                    }
                    Err(e) => log::debug!("no thumbnail from {url}: {e:#}"),
                }
            }
        });
        self.state.borrow_mut().thumbnail_task = Some(task.abort_handle());

        let app = self.clone();
        self.on_ui(async move {
            // ends once the download task is done or replaced
            while let Some((url, pixels)) = rx.recv().await {
                app.add_thumbnail(url, pixels);
            }
        });
    }

    fn add_thumbnail(&self, url: String, pixels: SharedPixelBuffer<Rgba8Pixel>) {
        let image = slint::Image::from_rgba8(pixels);
        let mut state = self.state.borrow_mut();
        for (row, &index) in state.shown.iter().enumerate() {
            if state.hits[index].image.as_deref() == Some(url.as_str())
                && let Some(mut data) = self.results.row_data(row)
            {
                data.image = image.clone();
                self.results.set_row_data(row, data);
            }
        }
        state.thumbnails.insert(url, image);
    }

    fn idle_status(&self) -> String {
        match (&*self.web.borrow(), &*self.web_problem.borrow()) {
            (Some(_), _) => {
                "Search for a song, album, artist or playlist, paste a Spotify link, or open \
                 Your playlists."
                    .into()
            }
            (None, Some(problem)) => format!(
                "Couldn't use your developer app ({problem}), so search finds songs, and \
                 their albums and artists. Fix it in Settings. Pasting a Spotify link works \
                 for anything."
            ),
            (None, None) => "Search for a song, album or artist, paste a Spotify link, or \
                             open Your playlists. To search albums, artists and playlists \
                             directly, set up a developer app in Settings."
                .into(),
        }
    }

    /// Fills the results list with the hits that match the selected filter.
    fn show_hits(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let kind = match window.get_filter() {
            1 => Some(Kind::Track),
            2 => Some(Kind::Album),
            3 => Some(Kind::Artist),
            4 => Some(Kind::Playlist),
            _ => None,
        };
        let mut state = self.state.borrow_mut();
        let shown: Vec<usize> = (0..state.hits.len())
            .filter(|&i| kind.is_none_or(|k| state.hits[i].kind == k))
            .collect();
        let rows: Vec<ResultRow> = shown
            .iter()
            .map(|&i| {
                let mut row = result_row(&state.hits[i], &state.queued, &state.thumbnails);
                row.downloaded = state.downloaded.get(i).copied().unwrap_or(false);
                row
            })
            .collect();

        let what = match kind {
            None => "results",
            Some(Kind::Track) => "songs",
            Some(Kind::Album) => "albums",
            Some(Kind::Artist) => "artists",
            Some(Kind::Playlist) => "playlists",
        };
        let mut status = match &state.listing {
            Listing::Nothing => self.idle_status(),
            Listing::Playlists if rows.is_empty() && state.hits.is_empty() => {
                "There are no playlists in your library.".into()
            }
            Listing::Playlists if rows.is_empty() => {
                "These are your playlists: choose All or Playlists to see them.".into()
            }
            Listing::Playlists => format!("Your playlists · {}", rows.len()),
            Listing::Search(_)
                if rows.is_empty()
                    && kind == Some(Kind::Playlist)
                    && self.web.borrow().is_none() =>
            {
                "Searching playlists needs a Spotify developer app: set one up in Settings. \
                 Your playlists and pasted playlist links work without one."
                    .into()
            }
            Listing::Search(query) if rows.is_empty() => {
                format!("No {what} found for “{query}”.")
            }
            Listing::Search(_) => format!("{} {what}", rows.len()),
        };
        if let Some(note) = &state.note {
            status = format!("{status} · {note}");
        }
        state.shown = shown;
        drop(state);

        self.results.set_vec(rows);
        window.set_search_status(status.into());
    }

    fn download_hit(&self, row: usize) {
        let hit = {
            let state = self.state.borrow();
            let Some(&index) = state.shown.get(row) else {
                return;
            };
            state.hits[index].clone()
        };
        let link = hit.link();
        let title = format!("{} · {}", hit.label, hit.title);
        let input = Input {
            text: link.clone(),
            target: hit.target(),
        };
        let Some(id) = self.enqueue(title, input) else {
            return;
        };
        self.state.borrow_mut().queued.insert(link, id);
        self.refresh_queued();
    }

    /// A job ended (done, failed or cancelled): its result can be downloaded again,
    /// unless it's all there now.
    fn job_finished(self: &Rc<Self>, id: i32) {
        self.state.borrow_mut().queued.retain(|_, job| *job != id);
        self.refresh_queued();
        self.check_downloads(Vec::new());
        self.save_queue();
    }

    /// Brings the visible rows' Download/Queued buttons up to date.
    fn refresh_queued(&self) {
        let state = self.state.borrow();
        for (row, &index) in state.shown.iter().enumerate() {
            let queued = state.queued.contains_key(&state.hits[index].link());
            if let Some(mut data) = self.results.row_data(row)
                && data.queued != queued
            {
                data.queued = queued;
                self.results.set_row_data(row, data);
            }
        }
    }

    /// Adds a job to the queue and returns its id.
    fn enqueue(&self, title: String, input: Input) -> Option<i32> {
        let window = self.window.upgrade()?;
        let id = self.add_job(title, input, chosen_output(&window), chosen_format(&window));
        self.save_queue();
        Some(id)
    }

    /// Lists a job and hands it to the worker, or holds on to it until logged in.
    fn add_job(&self, title: String, input: Input, output: PathBuf, format: Format) -> i32 {
        let (cancel_tx, cancel) = watch::channel(false);
        let id = self.new_job_id(JobSpec {
            input: input.text.clone(),
            output: output.clone(),
            format,
        });
        self.state.borrow_mut().cancels.insert(id, cancel_tx);
        self.jobs.push(JobRow {
            id,
            title: title.into(),
            status: "Queued".into(),
            detail: "".into(),
            progress: 0.0,
            state: JobState::Queued,
        });
        let job = Job {
            id,
            input,
            output,
            format,
            cancel,
        };
        if self.conn.get().is_some() {
            _ = self.jobs_tx.send(job);
        } else {
            self.state.borrow_mut().held.push(job);
        }
        id
    }

    fn new_job_id(&self, spec: JobSpec) -> i32 {
        let mut state = self.state.borrow_mut();
        state.next_job += 1;
        let id = state.next_job;
        state.specs.insert(id, spec);
        id
    }

    /// Puts back the Downloads list from last time, then queues the command line's
    /// inputs. Unfinished jobs start over (skipping tracks already there) once logged in.
    fn restore_queue(&self, saved: SavedQueue, startup: Vec<Input>) {
        for job in saved.jobs {
            let format = job.format().unwrap_or(self.settings.format);
            match job.finished {
                Some(finished) => {
                    let id = self.new_job_id(JobSpec {
                        input: job.input,
                        output: job.output,
                        format,
                    });
                    self.jobs.push(JobRow {
                        id,
                        title: job.title.into(),
                        status: finished.status.into(),
                        detail: finished.detail.into(),
                        progress: 1.0,
                        state: if finished.failed {
                            JobState::Failed
                        } else {
                            JobState::Done
                        },
                    });
                }
                None => match input::parse(&job.input) {
                    Ok(target) => {
                        let input = Input {
                            text: job.input.clone(),
                            target,
                        };
                        let id = self.add_job(job.title, input, job.output, format);
                        self.state.borrow_mut().queued.insert(job.input, id);
                    }
                    Err(e) => log::warn!("dropping the saved download {:?}: {e}", job.input),
                },
            }
        }
        if saved.paused {
            self.set_paused(true);
        }
        for input in startup {
            _ = self.enqueue(input.text.clone(), input);
        }
    }

    /// Saves the Downloads list, and whether it's paused, for next time.
    fn save_queue(&self) {
        let state = self.state.borrow();
        let jobs = self
            .jobs
            .iter()
            .filter_map(|row| {
                let spec = state.specs.get(&row.id)?;
                let finished =
                    matches!(row.state, JobState::Done | JobState::Failed).then(|| Finished {
                        failed: row.state == JobState::Failed,
                        status: row.status.to_string(),
                        detail: row.detail.to_string(),
                    });
                Some(SavedJob {
                    title: row.title.to_string(),
                    input: spec.input.clone(),
                    output: spec.output.clone(),
                    format: saved_queue::format_name(spec.format),
                    finished,
                })
            })
            .collect();
        drop(state);
        let queue = SavedQueue {
            paused: *self.pause.borrow(),
            jobs,
        };
        if let Err(e) = queue.save(self.auth.dir()) {
            log::warn!("couldn't save the Downloads list: {e:#}");
        }
    }

    fn cancel(self: &Rc<Self>, id: i32) {
        self.cancel_jobs(&[id]);
    }

    fn cancel_all(self: &Rc<Self>) {
        let ids: Vec<i32> = self
            .jobs
            .iter()
            .filter(|row| matches!(row.state, JobState::Queued | JobState::Active))
            .map(|row| row.id)
            .collect();
        self.cancel_jobs(&ids);
    }

    /// Stops the jobs and takes them off the list. A running download is dropped (its
    /// partial file goes with it); the worker skips queued ones and reports nothing back
    /// for either.
    fn cancel_jobs(self: &Rc<Self>, ids: &[i32]) {
        if ids.is_empty() {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            for id in ids {
                if let Some(cancel) = state.cancels.remove(id) {
                    cancel.send_replace(true);
                }
                state.specs.remove(id);
            }
            state.queued.retain(|_, job| !ids.contains(job));
            state.held.retain(|job| !ids.contains(&job.id));
        }
        let keep: Vec<JobRow> = self
            .jobs
            .iter()
            .filter(|row| !ids.contains(&row.id))
            .collect();
        self.jobs.set_vec(keep);
        self.refresh_queued();
        self.check_downloads(Vec::new());
        self.save_queue();
    }

    fn set_paused(&self, paused: bool) {
        self.pause.send_replace(paused);
        if let Some(window) = self.window.upgrade() {
            window.set_paused(paused);
        }
        self.tray.set_status(self.tray_status());
        self.save_queue();
    }

    /// One line on what's downloading, for the tray icon.
    fn tray_status(&self) -> String {
        let active = self.jobs.iter().find(|row| row.state == JobState::Active);
        let queued = self
            .jobs
            .iter()
            .filter(|row| row.state == JobState::Queued)
            .count();
        let mut parts = Vec::new();
        if *self.pause.borrow() {
            parts.push("Paused".to_string());
        }
        match active {
            Some(row) => parts.push(format!("{}: {}", row.title, row.status)),
            None if queued == 0 => parts.push("Nothing to download".into()),
            None => {}
        }
        if queued > 0 {
            parts.push(format!("{queued} queued"));
        }
        parts.join(" · ")
    }

    fn clear_finished(&self) {
        let keep: Vec<JobRow> = self
            .jobs
            .iter()
            .filter(|row| matches!(row.state, JobState::Queued | JobState::Active))
            .collect();
        let ids: HashSet<i32> = keep.iter().map(|row| row.id).collect();
        {
            let mut state = self.state.borrow_mut();
            state.cancels.retain(|id, _| ids.contains(id));
            state.specs.retain(|id, _| ids.contains(id));
        }
        self.jobs.set_vec(keep);
        self.save_queue();
    }

    /// Shows the system's folder picker, starting at the current download folder.
    fn choose_folder(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let current = expand_home(window.get_output_dir().trim());
        // start in the nearest folder that exists
        let start = current
            .ancestors()
            .find(|dir| dir.is_dir())
            .map(PathBuf::from);
        window.set_choosing_folder(true);

        let app = self.clone();
        let use_choice = move |chosen: Option<PathBuf>| {
            let Some(window) = app.window.upgrade() else {
                return;
            };
            window.set_choosing_folder(false);
            // None: cancelled
            if let Some(dir) = chosen {
                window.set_output_dir(dir.display().to_string().into());
                app.folder_changed();
            }
        };

        // macOS shows the panel as a sheet on the window, opened from the main thread
        // and awaited there, so the window keeps drawing (and downloads keep showing
        // progress) meanwhile. Elsewhere the dialog waits on a background thread.
        if cfg!(target_os = "macos") {
            let dialog = rfd::AsyncFileDialog::new().set_title("Choose where to save downloads");
            let dialog = match start {
                Some(dir) => dialog.set_directory(dir),
                None => dialog,
            };
            let chosen = dialog.pick_folder();
            self.on_ui(async move { use_choice(chosen.await.map(|f| f.path().to_path_buf())) });
        } else {
            let task = self.rt.spawn_blocking(move || {
                let dialog = rfd::FileDialog::new().set_title("Choose where to save downloads");
                match start {
                    Some(dir) => dialog.set_directory(dir),
                    None => dialog,
                }
                .pick_folder()
            });
            self.on_ui(async move { use_choice(task.await.ok().flatten()) });
        }
    }

    /// The download folder changed: remember it, and check the results against it.
    fn folder_changed(self: &Rc<Self>) {
        self.remember_folder();
        self.check_downloads(Vec::new());
    }

    fn remember_folder(&self) {
        self.folder_timer.stop();
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let mut prefs = self.prefs.borrow_mut();
        prefs.set_output(&window.get_output_dir());
        save_prefs(&prefs);
    }

    fn format_changed(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        {
            let mut prefs = self.prefs.borrow_mut();
            prefs.set_format(chosen_format(&window));
            save_prefs(&prefs);
        }
        self.check_downloads(Vec::new());
    }

    /// A theme was chosen in Settings: show it, and keep it for next time.
    fn theme_changed(&self, index: i32) {
        let theme = usize::try_from(index)
            .ok()
            .and_then(|index| theme::THEMES.get(index));
        let (Some(window), Some(theme)) = (self.window.upgrade(), theme) else {
            return;
        };
        show_theme(&window, theme);
        let mut prefs = self.prefs.borrow_mut();
        prefs.set_theme(theme.id);
        save_prefs(&prefs);
    }

    /// Shows Settings, where the theme is chosen and the developer app set up.
    fn open_settings(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let web = self.web.borrow();
        let client_id = web.as_ref().map(|w| w.client_id().to_string());
        window.set_app_client_id(client_id.unwrap_or_default().into());
        window.set_app_client_secret("".into());
        window.set_app_saved(search::WebApi::is_saved(self.auth.dir()));
        window.set_app_from_environment(search::WebApi::in_environment());
        match (&*web, &*self.web_problem.borrow()) {
            (Some(_), _) => show_app_status(
                &window,
                "Set up: search covers albums, artists and playlists.",
                false,
            ),
            (None, Some(problem)) => show_app_status(
                &window,
                &format!("The saved developer app couldn't be used: {problem}."),
                true,
            ),
            (None, None) => show_app_status(&window, "", false),
        }
        window.set_settings_open(true);
    }

    /// Checks the developer app typed into Settings with Spotify, then saves and uses it.
    /// Nothing is saved unless Spotify accepts it.
    fn save_developer_app(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let typed =
            search::WebApi::new(&window.get_app_client_id(), &window.get_app_client_secret());
        let web = match typed {
            Ok(web) => web,
            Err(e) => return show_app_status(&window, &sentence(&e.to_string()), true),
        };
        window.set_app_checking(true);
        show_app_status(&window, "Checking with Spotify…", false);
        let dir = self.auth.dir().to_path_buf();
        let task = self.rt.spawn(async move {
            web.check().await?;
            web.save(&dir)?;
            anyhow::Ok(web)
        });
        let app = self.clone();
        self.on_ui(async move {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let Some(window) = app.window.upgrade() else {
                return;
            };
            window.set_app_checking(false);
            match result {
                Ok(web) => {
                    window.set_app_client_id(web.client_id().into());
                    window.set_app_client_secret("".into());
                    window.set_app_saved(true);
                    *app.web.borrow_mut() = Some(Arc::new(web));
                    *app.web_problem.borrow_mut() = None;
                    show_app_status(
                        &window,
                        "Saved. Search now covers albums, artists and playlists.",
                        false,
                    );
                    app.show_hits(); // updates the hints in the results list
                }
                Err(e) => show_app_status(&window, &sentence(&format!("{e:#}")), true),
            }
        });
    }

    fn remove_developer_app(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if let Err(e) = search::WebApi::remove(self.auth.dir()) {
            return show_app_status(&window, &sentence(&format!("{e:#}")), true);
        }
        // one given through the environment still counts
        let web = search::WebApi::load(self.auth.dir())
            .ok()
            .flatten()
            .map(Arc::new);
        let client_id = web.as_ref().map(|w| w.client_id().to_string());
        let status = if web.is_some() {
            "Removed. The developer app from SPOTDL_CLIENT_ID and SPOTDL_CLIENT_SECRET is still \
             used."
        } else {
            "Removed. Search finds songs, and the albums and artists they're on."
        };
        *self.web.borrow_mut() = web;
        *self.web_problem.borrow_mut() = None;
        window.set_app_client_id(client_id.unwrap_or_default().into());
        window.set_app_client_secret("".into());
        window.set_app_saved(false);
        show_app_status(&window, status, false);
        self.show_hits();
    }

    fn open_folder(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dir = expand_home(window.get_output_dir().trim());
        _ = std::fs::create_dir_all(&dir);
        // the Finder, Explorer or the desktop's file manager
        if let Err(e) = open::that_detached(&dir) {
            log::warn!("couldn't open {}: {e}", dir.display());
        }
    }
}

/// The download folder in the window, or the default one if it's left empty.
fn chosen_output(window: &AppWindow) -> PathBuf {
    match window.get_output_dir().trim() {
        "" => default_music_dir(),
        dir => expand_home(dir),
    }
}

fn chosen_format(window: &AppWindow) -> Format {
    FORMATS[(window.get_format_index().max(0) as usize).min(FORMATS.len() - 1)]
}

/// Colors the window in `theme`.
fn show_theme(window: &AppWindow, theme: &theme::Theme) {
    let colors = window.global::<Theme>();
    colors.set_colors(theme.colors());
    colors.set_dark(theme.dark);
    window.set_theme_index(theme.index() as i32);
}

fn show_app_status(window: &AppWindow, text: &str, bad: bool) {
    window.set_app_status(text.into());
    window.set_app_status_bad(bad);
}

/// An error message as a sentence: capitalized, with a full stop.
fn sentence(text: &str) -> String {
    let mut chars = text.trim().chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().chain(chars).collect();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

fn save_prefs(prefs: &Prefs) {
    if let Err(e) = prefs.save() {
        log::warn!("couldn't save the window's settings: {e:#}");
    }
}

fn result_row(
    hit: &Hit,
    queued: &HashMap<String, i32>,
    thumbnails: &HashMap<String, slint::Image>,
) -> ResultRow {
    let subtitle = if hit.subtitle.is_empty() {
        hit.label.clone()
    } else {
        format!("{} · {}", hit.label, hit.subtitle)
    };
    let initial: String = hit
        .title
        .chars()
        .take(1)
        .flat_map(char::to_uppercase)
        .collect();
    ResultRow {
        title: hit.title.as_str().into(),
        subtitle: subtitle.into(),
        detail: hit.detail.as_str().into(),
        queued: queued.contains_key(&hit.link()),
        downloaded: false,
        image: hit
            .image
            .as_ref()
            .and_then(|url| thumbnails.get(url).cloned())
            .unwrap_or_default(),
        initial: initial.into(),
        round: hit.kind == Kind::Artist,
    }
}

// ---------------------------------------------------------------------------------------
// Thumbnails

async fn fetch_thumbnail(
    http: &reqwest::Client,
    url: &str,
) -> Result<SharedPixelBuffer<Rgba8Pixel>> {
    let response = http.get(url).send().await?.error_for_status()?;
    // thumbnails are tens of kilobytes; don't let anything odd use up memory
    if response.content_length().is_some_and(|len| len > 4 << 20) {
        bail!("image too large");
    }
    let bytes = response.bytes().await?;
    tokio::task::spawn_blocking(move || decode_thumbnail(&bytes, THUMBNAIL_PX)).await?
}

/// Decodes a JPEG and shrinks it so its shorter side is at most `size` pixels.
fn decode_thumbnail(jpeg: &[u8], size: usize) -> Result<SharedPixelBuffer<Rgba8Pixel>> {
    use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(8192)
        .set_max_height(8192);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
    let pixels = decoder
        .decode()
        .map_err(|e| anyhow!("couldn't decode the image: {e:?}"))?;
    let (width, height) = decoder.dimensions().context("the image has no size")?;
    ensure!(
        width > 0 && height > 0 && pixels.len() == width * height * 4,
        "unexpected image data"
    );
    let (to_width, to_height) = cover_size(width, height, size);
    Ok(shrink(&pixels, width, height, to_width, to_height))
}

/// The size that makes the shorter side `size`, keeping the aspect ratio; never larger
/// than the original.
fn cover_size(width: usize, height: usize, size: usize) -> (usize, usize) {
    let shorter = width.min(height);
    if shorter <= size {
        return (width, height);
    }
    (
        (width * size).div_ceil(shorter),
        (height * size).div_ceil(shorter),
    )
}

/// Shrinks RGBA pixels by averaging the source pixels each target pixel covers.
fn shrink(
    src: &[u8],
    width: usize,
    height: usize,
    to_width: usize,
    to_height: usize,
) -> SharedPixelBuffer<Rgba8Pixel> {
    // which source pixels (start..end) the target pixel `i` of `to` covers
    let span = |i: usize, from: usize, to: usize| {
        let start = i * from / to;
        (start, ((i + 1) * from / to).clamp(start + 1, from))
    };
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(to_width as u32, to_height as u32);
    let dst = out.make_mut_slice();
    for ty in 0..to_height {
        let (y0, y1) = span(ty, height, to_height);
        for tx in 0..to_width {
            let (x0, x1) = span(tx, width, to_width);
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * width + x) * 4;
                    for (total, &value) in sum.iter_mut().zip(&src[i..i + 4]) {
                        *total += u32::from(value);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let [r, g, b, a] = sum.map(|total| (total / n) as u8);
            dst[ty * to_width + tx] = Rgba8Pixel { r, g, b, a };
        }
    }
    out
}

/// Changes the job row with this id, if it's still listed.
fn update_row(jobs: &impl Model<Data = JobRow>, id: i32, change: impl FnOnce(&mut JobRow)) {
    let found = (0..jobs.row_count()).find_map(|i| {
        jobs.row_data(i)
            .filter(|row| row.id == id)
            .map(|row| (i, row))
    });
    if let Some((i, mut row)) = found {
        change(&mut row);
        jobs.set_row_data(i, row);
    }
}

// ---------------------------------------------------------------------------------------
// The download worker

struct Job {
    id: i32,
    input: Input,
    output: PathBuf,
    format: Format,
    cancel: watch::Receiver<bool>,
}

/// Runs queued jobs one at a time through a single downloader, so the gap between
/// audio key requests, and stopping when Spotify refuses, hold across all of them.
async fn worker(
    conn: Arc<OnceLock<Arc<Connection>>>,
    settings: Settings,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    paused: watch::Receiver<bool>,
    ui: slint::Weak<AppWindow>,
) {
    let mut downloader: Option<Downloader> = None;
    while let Some(mut job) = jobs.recv().await {
        if *job.cancel.borrow() {
            continue;
        }
        let Some(conn) = conn.get().cloned() else {
            finish(&ui, job.id, JobState::Failed, "Not logged in", "");
            continue;
        };
        if downloader.is_none() {
            match Downloader::new(conn, settings.clone(), Box::new(Silent)) {
                Ok(mut d) => {
                    d.pause = Some(paused.clone());
                    downloader = Some(d);
                }
                Err(e) => {
                    finish(
                        &ui,
                        job.id,
                        JobState::Failed,
                        "Couldn't start",
                        &format!("{e:#}"),
                    );
                    continue;
                }
            }
        }
        let d = downloader.as_mut().expect("created above");
        if d.halted() {
            finish(
                &ui,
                job.id,
                JobState::Failed,
                "Not started",
                engine::HALT_MESSAGE,
            );
            continue;
        }
        if job.format.needs_ffmpeg() && !output::ffmpeg_available().await {
            let detail = format!(
                "Saving as {} needs ffmpeg, which wasn't found. {}, or choose Ogg (original).",
                job.format.extension(),
                output::FFMPEG_HINT
            );
            finish(&ui, job.id, JobState::Failed, "Needs ffmpeg", &detail);
            continue;
        }

        d.settings.output = job.output.clone();
        d.settings.format = job.format;
        let cancelled = loop {
            d.stats = Stats::default();
            d.reporter = Box::new(JobReporter::new(job.id, ui.clone()));
            let mut pause = paused.clone();
            let run = async {
                // paused: don't even start looking things up
                if *pause.borrow_and_update() {
                    send(&ui, job.id, |row| row.status = "Paused".into());
                    _ = pause.wait_for(|paused| !paused).await;
                }
                send(&ui, job.id, |row| {
                    row.state = JobState::Active;
                    row.status = "Starting…".into();
                });
                d.process(&job.input).await;
            };

            // dropping the download (on cancel) removes its partial file
            let cancelled = tokio::select! {
                () = run => false,
                () = cancelled(&mut job.cancel) => true,
            };
            if cancelled || d.halted() {
                break cancelled;
            }
            let Some(problem) = d.stats.unwritable.take() else {
                break false;
            };
            // The folder can't take files (say, its drive was disconnected): every job
            // for it would fail the same way. Hold the queue instead, and pick this job
            // up again once it's resumed.
            if !wait_for_folder(&ui, &mut job, &paused, problem).await {
                break true;
            }
        };
        // a cancelled job is already off the list
        if !cancelled {
            let (state, status, detail) = outcome(&d.stats, d.halted());
            finish(&ui, job.id, state, &status, &detail);
        }
        if d.halted() {
            let banner = format!(
                "{} Downloads stay paused until you restart spotdl.",
                engine::HALT_MESSAGE
            );
            _ = ui.upgrade_in_event_loop(move |w| w.set_banner(banner.into()));
        }
    }
}

/// Pauses the queue because `job`'s folder can't take files, says so, and returns once
/// the queue is resumed: true to try the job again, false if it was cancelled meanwhile.
async fn wait_for_folder(
    ui: &slint::Weak<AppWindow>,
    job: &mut Job,
    paused: &watch::Receiver<bool>,
    problem: String,
) -> bool {
    let banner =
        format!("{problem}. Downloads are paused: choose Queue ▾ → Resume all once that's fixed.");
    send(ui, job.id, move |row| {
        row.state = JobState::Queued;
        row.status = "Waiting for the download folder".into();
        row.detail = problem.into();
    });
    let mut pause = paused.clone();
    pause.borrow_and_update();
    _ = ui.upgrade_in_event_loop(move |window| {
        window.set_banner(banner.into());
        // the window's own Pause all, so the pause is shown, and remembered on restart
        window.invoke_pause_all();
    });
    let resumed = async {
        // paused (by the above), then resumed
        while pause.changed().await.is_ok() {
            if !*pause.borrow_and_update() {
                return;
            }
        }
        std::future::pending::<()>().await;
    };
    let retry = tokio::select! {
        () = resumed => true,
        () = cancelled(&mut job.cancel) => false,
    };
    _ = ui.upgrade_in_event_loop(|window| window.set_banner("".into()));
    retry
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    // cancelling says so before dropping the sender, so a dropped sender alone means the
    // job is no longer on the list; carry on with it then
    if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The finished job's row: its state, a summary, and what went wrong if anything did.
fn outcome(s: &Stats, halted: bool) -> (JobState, String, String) {
    let mut parts = Vec::new();
    if s.downloaded > 0 {
        parts.push(format!("{} downloaded", count(s.downloaded, "track")));
    }
    if s.existing > 0 {
        parts.push(format!("{} already there", s.existing));
    }
    if s.planned > 0 {
        parts.push(format!("{} to download", count(s.planned, "track")));
    }
    if !s.failures.is_empty() {
        parts.push(format!("{} failed", s.failures.len()));
    }
    if s.not_attempted > 0 {
        parts.push(format!("{} not attempted", s.not_attempted));
    }
    let summary = if parts.is_empty() {
        "Nothing to download".to_string()
    } else {
        parts.join(" · ")
    };

    let detail = if halted {
        engine::HALT_MESSAGE.to_string()
    } else {
        match s.failures.as_slice() {
            [] => String::new(),
            [only] => only.clone(),
            [first, rest @ ..] => format!("{first} (and {} more)", rest.len()),
        }
    };
    let state = if halted || (!s.failures.is_empty() && s.downloaded + s.existing == 0) {
        JobState::Failed
    } else {
        JobState::Done
    };
    (state, summary, detail)
}

fn send(ui: &slint::Weak<AppWindow>, id: i32, change: impl FnOnce(&mut JobRow) + Send + 'static) {
    _ = ui.upgrade_in_event_loop(move |window| update_row(&window.get_jobs(), id, change));
}

fn finish(ui: &slint::Weak<AppWindow>, id: i32, state: JobState, status: &str, detail: &str) {
    let (status, detail) = (status.to_string(), detail.to_string());
    _ = ui.upgrade_in_event_loop(move |window| {
        update_row(&window.get_jobs(), id, |row| {
            row.state = state;
            row.status = status.into();
            row.detail = detail.into();
            row.progress = 1.0;
        });
        window.invoke_job_finished(id);
    });
}

struct Silent;

impl Reporter for Silent {
    fn event(&mut self, _: Event<'_>) {}
}

/// Turns the engine's events for one job into updates of its row, at most ten a
/// second for byte-by-byte progress.
struct JobReporter {
    id: i32,
    ui: slint::Weak<AppWindow>,
    total: usize,
    /// Tracks finished one way or another.
    finished: usize,
    last_sent: Option<Instant>,
}

impl JobReporter {
    const MIN_INTERVAL: Duration = Duration::from_millis(100);

    fn new(id: i32, ui: slint::Weak<AppWindow>) -> Self {
        Self {
            id,
            ui,
            total: 0,
            finished: 0,
            last_sent: None,
        }
    }

    fn send(&mut self, urgent: bool, change: impl FnOnce(&mut JobRow) + Send + 'static) {
        let now = Instant::now();
        if !urgent && self.last_sent.is_some_and(|t| now - t < Self::MIN_INTERVAL) {
            return;
        }
        self.last_sent = Some(now);
        send(&self.ui, self.id, change);
    }

    fn progress(&self, current: f32) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            ((self.finished as f32 + current) / self.total as f32).min(1.0)
        }
    }
}

impl Reporter for JobReporter {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Status(Some(text)) => {
                let text = text.to_string();
                self.send(false, move |row| row.status = text.into());
            }
            Event::Status(None) => {}
            Event::Resolved { header, total } => {
                self.total = total;
                let header = header.map(str::to_string);
                self.send(true, move |row| {
                    if let Some(header) = header {
                        row.title = header.into();
                    }
                    row.status = "Starting…".into();
                });
            }
            Event::Warning(text) | Event::Failed(text) => {
                let text = text.to_string();
                self.send(true, move |row| row.detail = text.into());
            }
            Event::WrotePlaylist(path) => {
                let text = format!("Wrote playlist {}", path.display());
                self.send(true, move |row| row.status = text.into());
            }
            Event::Track { pos, label, state } => {
                let n = format!("{}/{}", pos.index, pos.total);
                let mut current = 0.0;
                let (status, urgent) = match state {
                    TrackState::Waiting { left, retry: None } => (
                        format!("{n} · next download in {}: {label}", human_duration(left)),
                        true,
                    ),
                    TrackState::Waiting {
                        left,
                        retry: Some(reason),
                    } => (
                        format!(
                            "{n} · retrying in {} ({reason}): {label}",
                            human_duration(left)
                        ),
                        true,
                    ),
                    TrackState::Downloading { done, total } => {
                        current = if total > 0 {
                            done as f32 / total as f32
                        } else {
                            0.0
                        };
                        (format!("{n} · {label} · {:.0}%", current * 100.0), false)
                    }
                    TrackState::Saving => (format!("{n} · saving {label}…"), true),
                    TrackState::Paused => (format!("{n} · paused before {label}"), true),
                    TrackState::Retrying { reason, wait, .. } => {
                        let text =
                            format!("{label}: {reason}; retrying in {}", human_duration(wait));
                        self.send(true, move |row| row.detail = text.into());
                        return;
                    }
                    TrackState::Failed { reason } => {
                        self.finished += 1;
                        let text = format!("{label}: {reason}");
                        let progress = self.progress(0.0);
                        self.send(true, move |row| {
                            row.detail = text.into();
                            row.progress = progress;
                        });
                        return;
                    }
                    TrackState::Downloaded { .. }
                    | TrackState::Existing { .. }
                    | TrackState::Planned { .. } => {
                        self.finished += 1;
                        (format!("{n} · {label}"), true)
                    }
                };
                let progress = self.progress(current);
                self.send(urgent, move |row| {
                    row.status = status.into();
                    row.progress = progress;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/Music"), home.join("Music"));
        assert_eq!(expand_home("/srv/music"), PathBuf::from("/srv/music"));
        assert_eq!(expand_home("~other/x"), PathBuf::from("~other/x"));
    }

    #[test]
    fn tells_given_options_from_defaults() {
        let matches = Args::command().get_matches_from(["spotdl-gui", "-o", "/srv/music"]);
        assert!(given(&matches, "output"));
        // "format" is a real option: given only if SPOTDL_FORMAT is set where tests run
        assert!(matches.value_source("format").is_some());
        assert_eq!(
            given(&matches, "format"),
            std::env::var_os("SPOTDL_FORMAT").is_some()
        );
    }

    #[test]
    fn shrinks_thumbnails() {
        assert_eq!(cover_size(640, 640, 96), (96, 96));
        assert_eq!(cover_size(320, 200, 96), (154, 96));
        assert_eq!(cover_size(64, 64, 96), (64, 64), "never enlarged");

        // a 4x2 image, black on the left half and white on the right, shrunk to 2x1
        // keeps one black and one white pixel
        let mut src = Vec::new();
        for _ in 0..2 {
            src.extend([
                0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            ]);
        }
        let out = shrink(&src, 4, 2, 2, 1);
        let px = out.as_slice();
        assert_eq!((px[0].r, px[1].r, px[1].a), (0, 255, 255));
        // and to 1x1, the average
        assert_eq!(shrink(&src, 4, 2, 1, 1).as_slice()[0].g, 127);
    }

    /// Decodes a real JPEG (made with ffmpeg; skipped without it).
    #[test]
    fn decodes_jpeg_thumbnails() {
        let Ok(out) = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=320x200"])
            .args(["-frames:v", "1", "-f", "mjpeg", "-"])
            .output()
        else {
            eprintln!("ffmpeg not found; skipping");
            return;
        };
        let image = decode_thumbnail(&out.stdout, THUMBNAIL_PX).unwrap();
        assert_eq!((image.width(), image.height()), (154, 96));
        assert!(decode_thumbnail(b"not a jpeg", THUMBNAIL_PX).is_err());
    }

    #[test]
    fn summarises_jobs() {
        let stats = Stats {
            downloaded: 3,
            existing: 1,
            failures: vec!["a: nope".into(), "b: nope".into()],
            ..Stats::default()
        };
        let (state, summary, detail) = outcome(&stats, false);
        assert_eq!(state, JobState::Done);
        assert_eq!(summary, "3 tracks downloaded · 1 already there · 2 failed");
        assert_eq!(detail, "a: nope (and 1 more)");

        let failed = Stats {
            failures: vec!["x: gone".into()],
            ..Stats::default()
        };
        assert_eq!(outcome(&failed, false).0, JobState::Failed);
        assert_eq!(outcome(&stats, true).0, JobState::Failed);
    }
}
