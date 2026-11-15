//! The command line: download, login, logout and search.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use indicatif::{HumanBytes, ProgressBar};
use librespot_core::Session;

use crate::auth::{self, Auth, Connection};
use crate::cli::{Cli, Command, DownloadArgs};
use crate::engine::{self, Downloader, Event, Pos, Reporter, Settings, Stats, TrackState, count};
use crate::input;
use crate::login::{self, BrowserLogin};
use crate::meta::Meta;
use crate::output;
use crate::search::duration_text;
use crate::ui::{self, ui};

pub async fn run(cli: Cli) -> Result<ExitCode> {
    let auth = Auth::new(cli.global.config_dir.as_deref())?;
    match cli.command {
        Some(Command::Login) => {
            let session = log_in(&auth).await?;
            ui().println(ui::ok(&format!(
                "Logged in as {}",
                auth::describe(&session)
            )));
            ui().println(ui::dim(&format!(
                "Saved to {}",
                auth.credentials_path().display()
            )));
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Logout) => {
            if auth.logout()? {
                ui().println(ui::ok(&format!(
                    "Logged out (removed {})",
                    auth.credentials_path().display()
                )));
            } else {
                ui().println("You weren't logged in.");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Search { query, limit }) => search(&auth, &query.join(" "), limit).await,
        None => download(&auth, cli.download).await,
    }
}

/// Connects with the saved login, or walks through logging in if there isn't one.
async fn connect(auth: &Auth) -> Result<Session> {
    if !auth.is_logged_in()? {
        if !std::io::stdin().is_terminal() {
            bail!("not logged in; run `spotdl login` first");
        }
        ui().println("You're not logged in yet, so let's do that first.");
        let session = log_in(auth).await?;
        ui().println(ui::ok(&format!(
            "Logged in as {}",
            auth::describe(&session)
        )));
        return Ok(session);
    }
    let spinner = ui().spinner("Connecting to Spotify...");
    let session = auth.connect().await;
    spinner.finish_and_clear();
    session
}

/// Logs in through the browser. Where the browser can't reach this computer (say, over
/// SSH), pasting the address the login ended on works too.
async fn log_in(auth: &Auth) -> Result<Session> {
    let login = BrowserLogin::start().await?;
    if login::open_in_browser(login.url()) {
        ui().println("Log in to Spotify in your browser. If it didn't open, open this link:");
    } else {
        ui().println("Open this link in a browser to log in to Spotify:");
    }
    // on a line of its own and unstyled, so it copies cleanly
    ui().println(format!("\n{}\n", login.url()));
    let (paste, pasted) = tokio::sync::mpsc::unbounded_channel();
    if std::io::stdin().is_terminal() {
        ui().println(ui::dim(
            "Browser on another computer (say, you're using SSH)? Log in there. When it ends on \
             a page that doesn't load, copy that page's address and paste it here.",
        ));
        read_pasted_addresses(paste);
    }
    ui().println(ui::dim("Waiting for the login... (Ctrl-C cancels)"));
    auth.finish_login(login, pasted).await
}

/// Passes on what's pasted into the terminal, once it looks like the address a login
/// ended on. A thread of its own rather than the runtime's, since reading the terminal
/// blocks until Enter and would keep the program from exiting.
fn read_pasted_addresses(paste: tokio::sync::mpsc::UnboundedSender<String>) {
    let spawned = std::thread::Builder::new()
        .name("paste".into())
        .spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if !login::is_reply(&line) {
                    eprintln!(
                        "That's not it: paste the whole address of the page the login ended \
                         on (it has “code=” in it)."
                    );
                } else if paste.send(line).is_err() {
                    break; // logged in already
                }
            }
        });
    if let Err(e) = spawned {
        log::debug!("not reading pasted addresses: {e}");
    }
}

async fn search(auth: &Auth, query: &str, limit: usize) -> Result<ExitCode> {
    let meta = Meta::new(connect(auth).await?);
    let spinner = ui().spinner(format!("Searching for “{query}”..."));
    let ids = meta.search(query, limit).await?;
    let tracks = meta.tracks(&ids, |_| {}).await;
    spinner.finish_and_clear();

    if tracks.is_empty() {
        ui().println(format!("No tracks found for “{query}”."));
        return Ok(ExitCode::FAILURE);
    }
    let width = tracks.len().to_string().len();
    let out = ui::out();
    for (i, (_, track)) in tracks.iter().enumerate() {
        let Ok(t) = track else { continue };
        let year = t
            .album
            .year()
            .map(|y| format!(" ({y})"))
            .unwrap_or_default();
        println!(
            "{:>width$}. {}",
            i + 1,
            out.clone().bold().apply_to(t.display())
        );
        println!(
            "{:width$}  {}",
            "",
            out.clone().dim().apply_to(format!(
                "{}{year} · {} · {}",
                t.album.name,
                duration_text(t.duration_ms),
                t.link()
            ))
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn download(auth: &Auth, args: DownloadArgs) -> Result<ExitCode> {
    // validate everything before logging in or touching the network
    let inputs = input::collect(&args.inputs, args.input_file.as_deref())?;
    if inputs.is_empty() {
        bail!("nothing to download; pass a link, ID or search query (see `spotdl --help`)");
    }
    let settings = Settings::from_args(&args)?;
    if args.format.needs_ffmpeg() && !args.dry_run && !output::ffmpeg_available().await {
        bail!(
            "saving as {} needs ffmpeg, which wasn't found.\n\
             {}, or pass `-f ogg` to keep Spotify's original files without converting.",
            args.format.extension(),
            output::FFMPEG_HINT
        );
    }

    let session = connect(auth).await?;
    ui().println(ui::dim(&format!(
        "Logged in as {}",
        auth::describe(&session)
    )));
    let conn = Arc::new(Connection::new(auth.clone(), session));
    let reporter = Box::new(Terminal::new(args.dry_run));
    let mut downloader = Downloader::new(conn, settings, reporter)?;

    let started = Instant::now();
    for (n, input) in inputs.iter().enumerate() {
        if downloader.halted() {
            downloader.stats.inputs_not_started = inputs.len() - n;
            break;
        }
        downloader.process(input).await;
        // every input goes to the same folder, so the rest would fail the same way
        if downloader.stats.unwritable.is_some() {
            downloader.stats.inputs_not_started = inputs.len() - n - 1;
            break;
        }
    }
    Ok(summary(
        &downloader.stats,
        downloader.halted(),
        args.dry_run,
        &args.output,
        started.elapsed(),
    ))
}

fn summary(s: &Stats, halted: bool, dry_run: bool, output: &Path, elapsed: Duration) -> ExitCode {
    let mut parts = Vec::new();
    if dry_run {
        parts.push(format!("{} to download", count(s.planned, "track")));
    } else if s.downloaded > 0 {
        parts.push(format!(
            "{} downloaded ({})",
            count(s.downloaded, "track"),
            HumanBytes(s.bytes)
        ));
    }
    if s.existing > 0 {
        parts.push(format!("{} already downloaded", s.existing));
    }
    if !s.failures.is_empty() {
        parts.push(format!("{} failed", s.failures.len()));
    }
    if s.not_attempted > 0 {
        parts.push(format!("{} not attempted", count(s.not_attempted, "track")));
    }
    if s.inputs_not_started > 0 {
        parts.push(format!(
            "{} not started",
            count(s.inputs_not_started, "input")
        ));
    }
    if parts.is_empty() {
        parts.push("nothing to download".into());
    }

    ui().println("");
    let done = if dry_run {
        "Dry run".to_string()
    } else {
        format!("Done in {}", ui::human_duration(elapsed))
    };
    ui().println(ui::header(&format!("{done}: {}", parts.join(", "))));
    if s.failures.len() > 1 {
        for failure in &s.failures {
            ui().println(ui::failed(failure));
        }
    }
    if halted {
        ui().println(format!("{} {}", ui::error_label(), engine::HALT_MESSAGE));
    }
    if let Some(problem) = &s.unwritable {
        ui().println(format!(
            "{} {problem}. Stopped there; run it again once that's fixed (tracks you already \
             have won't be downloaded again).",
            ui::error_label()
        ));
    }
    let output = std::path::absolute(output).unwrap_or_else(|_| PathBuf::from(output));
    ui().println(ui::dim(&format!("Output: {}", output.display())));

    if s.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Renders engine events in the terminal: finished lines scroll up, with at most one
/// live spinner or progress bar underneath.
struct Terminal {
    dry_run: bool,
    live: Option<(Live, ProgressBar)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Live {
    Status,
    Waiting,
    Download,
}

impl Terminal {
    fn new(dry_run: bool) -> Self {
        Self {
            dry_run,
            live: None,
        }
    }

    fn clear(&mut self) {
        if let Some((_, bar)) = self.live.take() {
            bar.finish_and_clear();
        }
    }

    /// The live bar of this kind, replacing whatever was there.
    fn live(&mut self, kind: Live, make: impl FnOnce() -> ProgressBar) -> &ProgressBar {
        if self.live.as_ref().map(|(k, _)| *k) != Some(kind) {
            self.clear();
            self.live = Some((kind, make()));
        }
        &self.live.as_ref().expect("just set").1
    }

    fn track(&mut self, pos: Pos, label: &str, state: TrackState<'_>) {
        let prefix = if pos.total > 1 {
            let width = pos.total.to_string().len();
            format!("[{:>width$}/{}] ", pos.index, pos.total)
        } else {
            String::new()
        };
        match state {
            TrackState::Waiting { left, retry } => {
                let left = ui::human_duration(left);
                // the countdown goes first so a narrow terminal never cuts it off
                let message = match retry {
                    Some(reason) => format!(
                        "{}Retrying in {left}: {label} {}",
                        ui::dim(&prefix),
                        ui::dim(&format!("({reason})"))
                    ),
                    None => format!(
                        "{}Next download in {left}: {label} {}",
                        ui::dim(&prefix),
                        ui::dim("(pacing to stay under Spotify's rate limit; see --delay)")
                    ),
                };
                self.live(Live::Waiting, || ui().spinner(""))
                    .set_message(message);
            }
            TrackState::Downloading { done, total } => {
                let bar = self.live(Live::Download, || {
                    ui().download_bar(prefix.trim_end(), label)
                });
                bar.set_length(total);
                bar.set_position(done);
            }
            TrackState::Saving => {
                if let Some((Live::Download, bar)) = &self.live {
                    ui().busy(bar, format!("{label} — saving..."));
                }
            }
            TrackState::Paused => {
                self.live(Live::Waiting, || ui().spinner(""))
                    .set_message(format!("{}Paused: {label}", ui::dim(&prefix)));
            }
            TrackState::Retrying {
                reason,
                wait,
                next_attempt,
                attempts,
            } => {
                self.clear();
                ui().println(ui::warn(&format!(
                    "{prefix}{label}: {reason}; retrying in {} (attempt {next_attempt}/{attempts})",
                    ui::human_duration(wait)
                )));
            }
            TrackState::Downloaded { size, kbps } => {
                self.clear();
                let details = ui::dim(&format!("{} · {kbps} kbps source", HumanBytes(size)));
                ui().println(ui::ok(&format!("{prefix}{label}  {details}")));
            }
            TrackState::Existing { relative } => {
                self.clear();
                if self.dry_run {
                    println!(
                        "{prefix}{} {}",
                        relative.display(),
                        ui::out().dim().apply_to("(already downloaded)")
                    );
                } else {
                    ui().println(ui::skipped(&format!(
                        "{prefix}{label} (already downloaded)"
                    )));
                }
            }
            TrackState::Planned { relative } => {
                self.clear();
                println!("{prefix}{}", relative.display());
            }
            TrackState::Failed { reason } => {
                self.clear();
                ui().println(ui::failed(&format!("{prefix}{label}: {reason}")));
            }
        }
    }
}

impl Reporter for Terminal {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Status(Some(text)) => {
                self.live(Live::Status, || ui().spinner(""))
                    .set_message(text.to_string());
            }
            Event::Status(None) => {
                if matches!(self.live, Some((Live::Status, _))) {
                    self.clear();
                }
            }
            Event::Resolved { header, .. } => {
                if let Some(header) = header {
                    ui().println(ui::header(header));
                }
            }
            Event::Warning(text) => ui().println(ui::warn(text)),
            Event::Failed(text) => {
                self.clear();
                ui().println(ui::failed(text));
            }
            Event::WrotePlaylist(path) => {
                ui().println(ui::ok(&format!("Wrote playlist {}", path.display())));
            }
            Event::Track { pos, label, state } => self.track(pos, label, state),
        }
    }
}
