//! The download engine shared by the command line and the desktop app.
//!
//! Everything that affects the account lives here: the gap between audio key requests,
//! retries and back-off, reusing keys, and stopping when Spotify keeps refusing. Front
//! ends only render the [`Event`]s it reports, so they can't diverge on any of that.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use librespot_core::{FileId, Session, SpotifyId};
use tokio::sync::watch;

use crate::audio::{self, Audio, FetchError, KeyError};
use crate::auth::Connection;
use crate::awake::KeepAwake;
use crate::cli::{Bitrate, DownloadArgs, Format, Release};
use crate::input::{self, Input, Target};
use crate::meta::{self, AlbumInfo, Meta, TrackInfo};
use crate::output::{self, M3uEntry, PartFile, PlaylistPos, TagData, Template, Unwritable};
use crate::plex;

/// How many audio key requests in a row Spotify may refuse (or ignore) before
/// downloading stops. With the default `--retries` that's one track's full set of
/// retries, about five minutes of backing off.
const KEY_FAILURE_LIMIT: u32 = 5;

/// When the last audio key was requested (milliseconds since 1970), in the config
/// directory, so the gap between key requests holds across runs too.
const LAST_KEY_REQUEST_FILE: &str = "last-key-request";

/// What the system is told keeps it awake (shown by `pmset -g assertions` on macOS).
const AWAKE_REASON: &str = "spotdl is downloading music";

pub const HALT_MESSAGE: &str = "\
Stopped early: Spotify refused or ignored several audio key requests in a row, which \
usually means it's rate-limiting this account. Carrying on could get the account flagged, \
so the remaining tracks were skipped. Try again later; tracks you already have won't be \
downloaded again.";

/// What to download and how. Front ends may change it between inputs.
#[derive(Debug, Clone)]
pub struct Settings {
    pub output: PathBuf,
    pub format: Format,
    pub bitrate: Bitrate,
    pub template: Template,
    pub playlist_template: Template,
    pub releases: Vec<Release>,
    pub m3u: M3u,
    /// The Plex server playlists go to; `None` unless `--plex` turns that on.
    pub plex: Option<plex::Config>,
    pub overwrite: bool,
    pub dry_run: bool,
    /// Minimum gap between audio key requests.
    pub delay: Duration,
    pub retries: u32,
}

impl Settings {
    pub fn from_args(args: &DownloadArgs) -> Result<Self> {
        Ok(Self {
            output: output::expand_home(&args.output),
            format: args.format,
            bitrate: args.bitrate,
            template: Template::parse(&args.template).context("invalid --template")?,
            // playlist songs go under their albums too unless told otherwise
            playlist_template: Template::parse(
                args.playlist_template.as_deref().unwrap_or(&args.template),
            )
            .context("invalid --playlist-template")?,
            releases: args.releases.clone(),
            m3u: match (args.m3u, args.no_m3u) {
                (true, _) => M3u::Always,
                (_, true) => M3u::Never,
                _ => M3u::Auto,
            },
            plex: match args.plex {
                true => Some(Self::plex_server(args)?),
                false => None,
            },
            overwrite: args.overwrite,
            dry_run: args.dry_run,
            delay: Duration::from_secs(args.delay),
            retries: args.retries,
        })
    }

    /// The Plex server `--plex-url` and `--plex-token` give, whether or not `--plex` turns
    /// playlists in Plex on. (The desktop app turns them on and off in its Settings.)
    pub fn plex_server(args: &DownloadArgs) -> Result<plex::Config> {
        plex::Config::new(&args.plex_url, args.plex_token.clone()).context("invalid --plex-url")
    }

    /// Whether downloading a playlist always leaves its `.m3u8` file, as far as can be told
    /// beforehand: with `--m3u`, and by default when playlists don't go to Plex.
    pub fn always_writes_m3u(&self) -> bool {
        match self.m3u {
            M3u::Auto => self.plex.is_none(),
            M3u::Always => true,
            M3u::Never => false,
        }
    }

    /// Where a track is saved, relative to `output`. Tracks from a playlist (or Liked
    /// Songs) use the playlist template.
    pub fn relative_path(&self, track: &TrackInfo, playlist: Option<PlaylistPos>) -> PathBuf {
        let template = if playlist.is_some() {
            &self.playlist_template
        } else {
            &self.template
        };
        output::with_extension(&template.render(track, playlist), self.format.extension())
    }

    /// Where a playlist's `.m3u8` file goes.
    pub fn playlist_file(&self, name: &str) -> PathBuf {
        self.output.join(output::with_extension(
            Path::new(&output::sanitize(name)),
            "m3u8",
        ))
    }
}

/// When a playlist (or Liked Songs) gets an `.m3u8` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum M3u {
    /// Unless Plex has the playlist instead. Plex doesn't read `.m3u8` files inside its
    /// libraries, and downloading the playlist again rebuilds it there without the file.
    /// So it's written without `--plex`, and with it when Plex isn't set up here or has no
    /// music library holding the download folder.
    Auto,
    /// Always (`--m3u`).
    Always,
    /// Never (`--no-m3u`).
    Never,
}

/// Whether a playlist gets an `.m3u8` file, given how it went in Plex.
fn writes_m3u(mode: M3u, plex: &plex::Outcome) -> bool {
    match mode {
        M3u::Auto => plex.needs_m3u(),
        M3u::Always => true,
        M3u::Never => false,
    }
}

/// Where a track sits in the input being downloaded (1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub index: usize,
    pub total: usize,
}

/// What the engine is doing, for the front end to show.
#[derive(Debug)]
pub enum Event<'a> {
    /// Slow work that isn't about one track, like looking up an input or loading track
    /// details. `None` means it's over.
    Status(Option<&'a str>),
    /// An input was looked up and is about to be downloaded.
    Resolved {
        header: Option<&'a str>,
        total: usize,
    },
    /// Worth knowing, but nothing failed.
    Warning(&'a str),
    /// Something other than a track failed (an input lookup, a playlist file).
    Failed(&'a str),
    WrotePlaylist(&'a Path),
    /// What became of a playlist in Plex (see [`crate::plex`]).
    Plex(PlexEvent<'a>),
    Track {
        pos: Pos,
        label: &'a str,
        state: TrackState<'a>,
    },
}

/// What became of a playlist in Plex. Never a failed download: the tracks are there either
/// way, and running spotdl on the same link again brings the playlist up to date.
#[derive(Debug)]
pub enum PlexEvent<'a> {
    /// Dry run: the playlist would be added to Plex, or brought up to date there.
    WouldSync {
        title: &'a str,
    },
    /// Plex has no music library holding the download folder, so the playlist isn't in
    /// Plex (and its `.m3u8` file is written instead).
    NoLibrary {
        folder: &'a Path,
    },
    Synced(&'a plex::Synced),
    /// It didn't work. The message says why, and how to try again.
    Failed(&'a str),
}

#[derive(Debug)]
pub enum TrackState<'a> {
    /// Counting down (whole seconds) to the next key request, or to a retry.
    Waiting {
        left: Duration,
        retry: Option<&'a str>,
    },
    /// Bytes so far and in total (0 if unknown).
    Downloading {
        done: u64,
        total: u64,
    },
    /// Converting, tagging and writing the file.
    Saving,
    /// Held off (before the next key request) because the front end paused downloading.
    Paused,
    Retrying {
        reason: &'a str,
        wait: Duration,
        next_attempt: u32,
        attempts: u32,
    },
    Downloaded {
        size: u64,
        kbps: u32,
    },
    Existing {
        relative: &'a Path,
    },
    /// Dry run: where it would be saved.
    Planned {
        relative: &'a Path,
    },
    Failed {
        reason: &'a str,
    },
}

/// Receives the engine's events. Implementations should return quickly: they're called
/// for every chunk of a download.
pub trait Reporter: Send {
    fn event(&mut self, event: Event<'_>);
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub downloaded: usize,
    pub bytes: u64,
    pub existing: usize,
    pub planned: usize,
    pub failures: Vec<String>,
    /// Tracks skipped because downloading stopped early.
    pub not_attempted: usize,
    /// Inputs never looked at because downloading stopped early.
    pub inputs_not_started: usize,
    /// Why the download folder couldn't take files, when that stopped an input early
    /// (the drive isn't connected, is full or read-only, ...).
    pub unwritable: Option<String>,
    /// Playlists brought up to date in Plex.
    pub plex_synced: usize,
    /// What went wrong with playlists in Plex, or is missing from them. Never a failed
    /// download (see [`PlexEvent`]).
    pub plex_problems: Vec<String>,
}

/// What one input expands to.
struct Collection {
    header: Option<String>,
    /// Set for playlists and Liked Songs: selects the playlist template, and the playlist
    /// goes to Plex or into an `.m3u8` file.
    playlist: Option<String>,
    items: Vec<Item>,
    notes: Vec<String>,
}

impl Collection {
    fn of(header: Option<String>, items: Vec<Item>) -> Self {
        Self {
            header,
            playlist: None,
            items,
            notes: Vec::new(),
        }
    }
}

enum Item {
    Track(TrackInfo),
    Missing { label: String, reason: String },
}

/// Stops downloading when Spotify keeps refusing audio keys, instead of asking again
/// and again for every remaining track.
#[derive(Default)]
struct KeyBreaker {
    in_a_row: u32,
}

impl KeyBreaker {
    fn success(&mut self) {
        self.in_a_row = 0;
    }

    /// Records a failed key request; returns true once it's time to stop. Connection
    /// problems don't count, since they say nothing about the account.
    fn failure(&mut self, error: &KeyError) -> bool {
        if error.is_rate_limit() {
            self.in_a_row += 1;
        }
        self.in_a_row >= KEY_FAILURE_LIMIT
    }
}

/// Downloads inputs one track at a time. Keep one per account and run inputs through
/// it in turn, so the gap between key requests holds across all of them.
pub struct Downloader {
    conn: Arc<Connection>,
    meta: Meta,
    http: reqwest::Client,
    pub settings: Settings,
    pub reporter: Box<dyn Reporter>,
    pub stats: Stats,
    /// While this says `true`, no new track is started: downloading holds off before the
    /// next audio key request (a track already downloading is finished first).
    pub pause: Option<watch::Receiver<bool>>,
    covers: HashMap<SpotifyId, Option<Arc<Vec<u8>>>>,
    last_key_request: Option<Instant>,
    breaker: KeyBreaker,
    halted: bool,
}

impl Downloader {
    pub fn new(
        conn: Arc<Connection>,
        settings: Settings,
        reporter: Box<dyn Reporter>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        let last_key_request = last_key_request(&conn.auth().dir().join(LAST_KEY_REQUEST_FILE));
        Ok(Self {
            meta: Meta::new(conn.current()),
            conn,
            http,
            settings,
            reporter,
            stats: Stats::default(),
            pause: None,
            covers: HashMap::new(),
            last_key_request,
            breaker: KeyBreaker::default(),
            halted: false,
        })
    }

    /// True once Spotify refused too many audio keys in a row. Nothing more will be
    /// downloaded by this downloader.
    pub fn halted(&self) -> bool {
        self.halted
    }

    /// Looks up one input and downloads everything in it. The computer is kept from
    /// falling asleep meanwhile, except while downloading is paused.
    pub async fn process(&mut self, input: &Input) {
        let pause = self.pause.clone();
        tokio::select! {
            () = self.process_input(input) => {}
            () = stay_awake(pause) => {}
        }
    }

    async fn process_input(&mut self, input: &Input) {
        self.status(&format!("Looking up {}...", input.text));
        let collection = self.resolve(&input.target).await;
        self.reporter.event(Event::Status(None));

        match collection {
            Ok(collection) => {
                self.reporter.event(Event::Resolved {
                    header: collection.header.as_deref(),
                    total: collection.items.len(),
                });
                for note in &collection.notes {
                    self.reporter.event(Event::Warning(note));
                }
                self.download_collection(collection).await;
            }
            Err(e) => {
                let message = format!("{}: {e:#}", input.text);
                self.reporter.event(Event::Failed(&message));
                self.stats.failures.push(message);
            }
        }
    }

    fn status(&mut self, text: &str) {
        self.reporter.event(Event::Status(Some(text)));
    }

    fn warn(&mut self, text: &str) {
        self.reporter.event(Event::Warning(text));
    }

    fn track(&mut self, pos: Pos, label: &str, state: TrackState<'_>) {
        self.reporter.event(Event::Track { pos, label, state });
    }

    async fn session(&mut self) -> Result<Session> {
        if self.conn.current().is_invalid() {
            self.warn("Lost the connection to Spotify; reconnecting...");
        }
        let session = self.conn.session().await?;
        self.meta.set_session(session.clone());
        Ok(session)
    }

    // -----------------------------------------------------------------------------------
    // Resolving inputs into tracks

    async fn resolve(&mut self, target: &Target) -> Result<Collection> {
        self.session().await?;
        // bare IDs and short links have to be looked up before we know what they are
        let target = match target {
            Target::Id(id) => match self.meta.kind_of(*id).await? {
                "track" => Target::Track(*id),
                "album" => Target::Album(*id),
                "playlist" => Target::Playlist(*id),
                _ => Target::Artist(*id),
            },
            Target::ShortLink(url) => follow_short_link(&self.http, url).await?,
            other => other.clone(),
        };

        match target {
            Target::Track(id) => Ok(Collection::of(
                None,
                vec![Item::Track(self.meta.track(id).await?)],
            )),

            Target::Album(id) => {
                let album = self.meta.album(id).await?;
                let items = self.load_tracks(&album.tracks).await;
                let header = format!(
                    "{} · {} — {}{} · {}",
                    album.kind,
                    album.name,
                    album.artists.join(", "),
                    album.year().map(|y| format!(" · {y}")).unwrap_or_default(),
                    count(items.len(), "track"),
                );
                Ok(Collection::of(Some(header), items))
            }

            Target::Playlist(id) => {
                let (name, uris) = self.meta.playlist(id).await?;
                let ids: Vec<_> = uris.iter().filter_map(meta::track_id).collect();
                let items = self.load_tracks(&ids).await;
                let mut collection = Collection::of(
                    Some(format!("Playlist · {name} · {}", count(ids.len(), "track"))),
                    items,
                );
                if uris.len() > ids.len() {
                    collection.notes.push(format!(
                        "Skipping {} that aren't Spotify tracks (podcast episodes or local files)",
                        count(uris.len() - ids.len(), "item")
                    ));
                }
                collection.playlist = Some(name);
                Ok(collection)
            }

            Target::Artist(id) => self.resolve_artist(id).await,

            Target::LikedSongs => {
                let library = self.meta.library().await?;
                let ids: Vec<_> = library.iter().filter_map(meta::track_id).collect();
                let items = self.load_tracks(&ids).await;
                let mut collection = Collection::of(
                    Some(format!("Liked Songs · {}", count(ids.len(), "track"))),
                    items,
                );
                collection.playlist = Some("Liked Songs".into());
                Ok(collection)
            }

            Target::SavedAlbums => {
                let library = self.meta.library().await?;
                let ids: Vec<_> = library.iter().filter_map(meta::album_id).collect();
                let (albums, items) = self.load_albums(&ids, &HashSet::new(), None).await;
                Ok(Collection::of(
                    Some(format!(
                        "Saved albums · {} · {}",
                        count(albums, "album"),
                        count(items.len(), "track")
                    )),
                    items,
                ))
            }

            Target::Search(query) => {
                let id = *self
                    .meta
                    .search(&query, 1)
                    .await?
                    .first()
                    .ok_or_else(|| anyhow!("no tracks found"))?;
                let track = self.meta.track(id).await?;
                Ok(Collection::of(
                    Some(format!("Best match for “{query}”")),
                    vec![Item::Track(track)],
                ))
            }

            Target::Id(_) | Target::ShortLink(_) => {
                bail!("couldn't work out what this link points to")
            }
        }
    }

    async fn resolve_artist(&mut self, id: SpotifyId) -> Result<Collection> {
        let artist = self.meta.artist(id).await?;
        let releases = self.settings.releases.clone();

        let mut album_ids = Vec::new();
        let mut appears_on = HashSet::new();
        let mut seen = HashSet::new();
        for kind in &releases {
            let list = match kind {
                Release::Album => &artist.albums,
                Release::Single => &artist.singles,
                Release::Compilation => &artist.compilations,
                Release::AppearsOn => &artist.appears_on,
                Release::Top => continue,
            };
            for &album in list {
                if seen.insert(album) {
                    album_ids.push(album);
                    if *kind == Release::AppearsOn {
                        appears_on.insert(album);
                    }
                }
            }
        }

        let mut items = Vec::new();
        if releases.contains(&Release::Top) {
            items = self.load_tracks(&artist.top_tracks).await;
        }
        let (albums, album_items) = self.load_albums(&album_ids, &appears_on, Some(id)).await;
        let known: HashSet<SpotifyId> = items
            .iter()
            .filter_map(|i| match i {
                Item::Track(t) => Some(t.id),
                Item::Missing { .. } => None,
            })
            .collect();
        items.extend(
            album_items
                .into_iter()
                .filter(|i| !matches!(i, Item::Track(t) if known.contains(&t.id))),
        );

        Ok(Collection::of(
            Some(format!(
                "Artist · {} · {} · {}",
                artist.name,
                count(albums, "release"),
                count(items.len(), "track")
            )),
            items,
        ))
    }

    /// Loads every track of the given albums. On `appears_on` albums only the tracks
    /// featuring `artist` are kept. Returns the number of albums and the tracks.
    async fn load_albums(
        &mut self,
        ids: &[SpotifyId],
        appears_on: &HashSet<SpotifyId>,
        artist: Option<SpotifyId>,
    ) -> (usize, Vec<Item>) {
        self.status(&format!("Loading {}...", count(ids.len(), "release")));
        let mut track_ids = Vec::new();
        let mut filtered: HashSet<SpotifyId> = HashSet::new();
        let mut items = Vec::new();
        for (id, album) in ids.iter().zip(self.meta.albums(ids).await) {
            match album {
                Ok(album) => {
                    track_ids.extend(&album.tracks);
                    if appears_on.contains(id) {
                        filtered.extend(&album.tracks);
                    }
                }
                Err(e) => items.push(Item::Missing {
                    label: meta::link("album", *id),
                    reason: format!("couldn't load album: {e}"),
                }),
            }
        }
        let mut unique = HashSet::new();
        track_ids.retain(|t| unique.insert(*t));

        items.extend(
            self.load_tracks(&track_ids)
                .await
                .into_iter()
                .filter(|item| match item {
                    Item::Track(t) if filtered.contains(&t.id) => {
                        artist.is_some_and(|a| t.artist_ids.contains(&a))
                    }
                    _ => true,
                }),
        );
        (ids.len(), items)
    }

    async fn load_tracks(&mut self, ids: &[SpotifyId]) -> Vec<Item> {
        let total = ids.len();
        let reporter = &mut self.reporter;
        let mut progress = |done: usize| {
            let text = format!("Loading track details {done}/{total}...");
            reporter.event(Event::Status(Some(&text)));
        };
        progress(0);
        self.meta
            .tracks(ids, progress)
            .await
            .into_iter()
            .map(|(id, track)| match track {
                Ok(t) => Item::Track(t),
                Err(e) => Item::Missing {
                    label: meta::link("track", id),
                    reason: format!("couldn't load track details: {e}"),
                },
            })
            .collect()
    }

    // -----------------------------------------------------------------------------------
    // Downloading

    async fn download_collection(&mut self, collection: Collection) {
        let total = collection.items.len();
        let mut m3u = Vec::new();
        let mut unwritable = false;

        for (i, item) in collection.items.into_iter().enumerate() {
            if self.halted {
                self.stats.not_attempted += total - i;
                break;
            }
            let pos = Pos {
                index: i + 1,
                total,
            };
            let track = match item {
                Item::Track(t) => t,
                Item::Missing { label, reason } => {
                    self.fail(pos, &label, &reason);
                    continue;
                }
            };
            let label = track.display();
            let playlist = collection.playlist.as_deref().map(|name| PlaylistPos {
                name,
                index: i + 1,
                len: total,
            });
            let relative = self.settings.relative_path(&track, playlist);
            let path = self.settings.output.join(&relative);
            let entry = || M3uEntry {
                path: path.clone(),
                seconds: track.duration_ms / 1000,
                label: format!("{} - {}", track.artist(), track.title),
            };

            if output::already_downloaded(&path) && !self.settings.overwrite {
                self.stats.existing += 1;
                self.track(
                    pos,
                    &label,
                    TrackState::Existing {
                        relative: &relative,
                    },
                );
                m3u.push(entry());
                continue;
            }
            if let Err(reason) = &track.playable {
                self.fail(pos, &label, reason);
                continue;
            }
            if self.settings.dry_run {
                self.stats.planned += 1;
                self.track(
                    pos,
                    &label,
                    TrackState::Planned {
                        relative: &relative,
                    },
                );
                continue;
            }

            match self.download(&track, &path, pos).await {
                Ok((size, kbps)) => {
                    self.stats.downloaded += 1;
                    self.stats.bytes += size;
                    self.track(pos, &label, TrackState::Downloaded { size, kbps });
                    m3u.push(entry());
                }
                Err(e) => {
                    self.fail(pos, &label, &format!("{e:#}"));
                    // every track left would fail the same way
                    if let Some(problem) = e.downcast_ref::<Unwritable>() {
                        self.stats.not_attempted += total - pos.index;
                        self.stats.unwritable = Some(problem.to_string());
                        unwritable = true;
                        break;
                    }
                }
            }
        }

        // a run that stopped early would replace a complete playlist with a partial one
        if let Some(name) = &collection.playlist
            && !self.halted
            && !unwritable
        {
            if self.settings.dry_run {
                self.plan_playlist(name).await;
            } else if !m3u.is_empty() {
                self.finish_playlist(name, &m3u).await;
            }
        }
    }

    /// Brings a downloaded playlist up to date in Plex, and writes its `.m3u8` file if the
    /// settings say to. `entries` are its tracks that are there, in order. A problem with
    /// Plex doesn't fail the download; it's reported, and the run carries on.
    async fn finish_playlist(&mut self, name: &str, entries: &[M3uEntry]) {
        let outcome = match &self.settings.plex {
            Some(config) => {
                let reporter = &mut self.reporter;
                let mut status = |text: &str| reporter.event(Event::Status(Some(text)));
                let output = &self.settings.output;
                let outcome = plex::sync(config, output, name, entries, &mut status).await;
                self.reporter.event(Event::Status(None));
                outcome
            }
            None => plex::Outcome::NotSetUp,
        };
        match &outcome {
            plex::Outcome::NotSetUp => {
                // turned on, but there's no token to use
                if self.settings.plex.is_some() {
                    let message = plex::no_token(name);
                    self.reporter
                        .event(Event::Plex(PlexEvent::Failed(&message)));
                    self.stats.plex_problems.push(message);
                }
            }
            plex::Outcome::NoLibrary => {
                let folder = &self.settings.output;
                let event = PlexEvent::NoLibrary { folder };
                self.reporter.event(Event::Plex(event));
            }
            plex::Outcome::Synced(synced) => {
                self.stats.plex_synced += 1;
                self.reporter.event(Event::Plex(PlexEvent::Synced(synced)));
                self.stats.plex_problems.extend(synced.left_out());
                for warning in &synced.warnings {
                    self.warn(warning);
                    self.stats.plex_problems.push(warning.clone());
                }
            }
            plex::Outcome::Failed(message) => {
                self.reporter.event(Event::Plex(PlexEvent::Failed(message)));
                self.stats.plex_problems.push(message.clone());
            }
        }

        if writes_m3u(self.settings.m3u, &outcome) {
            let path = self.settings.playlist_file(name);
            match output::write_m3u(&path, entries) {
                Ok(()) => self.reporter.event(Event::WrotePlaylist(&path)),
                Err(e) => {
                    let message = format!("{}: {e:#}", path.display());
                    self.reporter.event(Event::Failed(&message));
                    self.stats.failures.push(message);
                }
            }
        }
    }

    /// Dry run: says the playlist would go to Plex, if Plex is set up here. Plex itself
    /// isn't asked anything.
    async fn plan_playlist(&mut self, title: &str) {
        if let Some(config) = &self.settings.plex
            && config.set_up().await
        {
            self.reporter
                .event(Event::Plex(PlexEvent::WouldSync { title }));
        }
    }

    fn fail(&mut self, pos: Pos, label: &str, reason: &str) {
        self.track(pos, label, TrackState::Failed { reason });
        self.stats.failures.push(format!("{label}: {reason}"));
    }

    /// Downloads one track to `path`, retrying network problems and Spotify refusing the
    /// audio key (with growing back-off). The key is kept between retries, so a track
    /// normally costs exactly one key request. Returns the file size and source bitrate.
    async fn download(&mut self, track: &TrackInfo, path: &Path, pos: Pos) -> Result<(u64, u32)> {
        let playable = track
            .playable
            .as_ref()
            .map_err(|reason| anyhow!("{reason}"))?;
        let source = audio::choose(playable, self.settings.bitrate)?;
        let label = track.display();
        let retries = self.settings.retries;
        let mut key = None;
        let mut retry: Option<(Instant, String)> = None;
        // where the track is written before it's moved into place; whatever happens to
        // the download, dropping this removes it
        let mut part = Some(PartFile::new(path));

        for attempt in 1..=retries {
            self.wait_while_paused(pos, &label).await;
            // only key requests are rate-limited, so a retry with the key skips the delay
            self.wait_turn(pos, &label, key.is_none(), retry.take())
                .await;
            let session = self.session().await?;
            // exponential backoff; rate limits need much longer to clear than network blips
            let step = 1u64 << (attempt - 1).min(4);

            let k = match key {
                Some(k) => k,
                None => {
                    // spend a key only on a track that can be saved
                    let part = part.as_ref().expect("kept until saved").path();
                    self.check_destination(track, part, source.kbps).await?;
                    self.last_key_request = Some(Instant::now());
                    remember_key_request(&self.conn).await;
                    match audio::request_key(&session, playable, source).await {
                        Ok(k) => {
                            self.breaker.success();
                            *key.insert(k)
                        }
                        Err(e) => {
                            if self.breaker.failure(&e) {
                                self.halted = true;
                                bail!("{e}");
                            }
                            if attempt == retries {
                                bail!("{e}");
                            }
                            let secs = if e.is_rate_limit() {
                                20 * step
                            } else {
                                5 * step
                            };
                            retry =
                                Some(self.retry_later(pos, &label, secs, e.to_string(), attempt));
                            continue;
                        }
                    }
                }
            };

            self.track(pos, &label, TrackState::Downloading { done: 0, total: 0 });
            let reporter = &mut self.reporter;
            let mut progress = |done, total| {
                reporter.event(Event::Track {
                    pos,
                    label: &label,
                    state: TrackState::Downloading { done, total },
                });
            };
            let fetched = audio::fetch(&session, &self.http, source, k, &mut progress).await;
            let error = match fetched {
                Ok(audio) => {
                    self.track(pos, &label, TrackState::Saving);
                    let part = part.take().expect("saved once");
                    let size = self.save(track, audio, part, path).await?;
                    return Ok((size, source.kbps));
                }
                Err(e) => e,
            };
            if matches!(error, FetchError::BadKey) {
                key = None;
            }
            if attempt == retries {
                return Err(error.into_anyhow());
            }
            retry = Some(self.retry_later(pos, &label, 5 * step, error.to_string(), attempt));
        }
        unreachable!("the last attempt always returns")
    }

    /// Announces a retry and returns when it's due, for `wait_turn`.
    fn retry_later(
        &mut self,
        pos: Pos,
        label: &str,
        secs: u64,
        reason: String,
        attempt: u32,
    ) -> (Instant, String) {
        let wait = Duration::from_secs(secs);
        self.track(
            pos,
            label,
            TrackState::Retrying {
                reason: &reason,
                wait,
                next_attempt: attempt + 1,
                attempts: self.settings.retries,
            },
        );
        (Instant::now() + wait, reason)
    }

    /// Waits out a retry backoff and, when `paced` (the next step requests an audio
    /// key), the minimum gap between key requests.
    async fn wait_turn(
        &mut self,
        pos: Pos,
        label: &str,
        paced: bool,
        retry: Option<(Instant, String)>,
    ) {
        let paced = self
            .last_key_request
            .filter(|_| paced)
            .map(|t| t + self.settings.delay);
        let Some(until) = paced.max(retry.as_ref().map(|(t, _)| *t)) else {
            return;
        };
        loop {
            // pausing stops the countdown; the gap since the last key request only grows
            self.wait_while_paused(pos, label).await;
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let whole_seconds =
                Duration::from_secs(left.as_secs() + u64::from(left.subsec_nanos() > 0));
            self.track(
                pos,
                label,
                TrackState::Waiting {
                    left: whole_seconds,
                    retry: retry.as_ref().map(|(_, reason)| reason.as_str()),
                },
            );
            tokio::time::sleep(left.min(Duration::from_secs(1))).await;
        }
    }

    /// Returns once downloading isn't paused (straight away if it isn't).
    async fn wait_while_paused(&mut self, pos: Pos, label: &str) {
        let Some(mut pause) = self.pause.clone() else {
            return;
        };
        if !*pause.borrow_and_update() {
            return;
        }
        self.track(pos, label, TrackState::Paused);
        // an error means nothing can resume any more; carry on rather than hang
        _ = pause.wait_for(|paused| !paused).await;
    }

    /// Writes, tags and moves the track into place. Takes the audio by value so the only
    /// copy of the track is freed before tagging reads the file back in: one track's
    /// worth of memory at a time, not two.
    async fn save(
        &mut self,
        track: &TrackInfo,
        audio: Audio,
        part: PartFile,
        path: &Path,
    ) -> Result<u64> {
        // the drive may have filled up or gone away since `check_destination`
        let output = self.settings.output.clone();
        let unwritable = |e: anyhow::Error| match Unwritable::find(&output, &e) {
            Some(unwritable) => unwritable.into(),
            None => e,
        };
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))
                .map_err(unwritable)?;
        }
        let tags = TagData::new(track, audio.gain, self.cover(&track.album).await);
        let ogg = match self.settings.format {
            // written below, on the blocking thread that then owns it
            Format::Ogg => Some(audio.ogg),
            format => {
                output::transcode(&audio.ogg, format, part.path()).await?;
                drop(audio);
                None
            }
        };
        let path = path.to_path_buf();
        // writing, tagging and flushing to disk block, so keep them off the async threads.
        // Once started this runs to the end even if the download is cancelled, so it
        // leaves either a complete track or nothing, never a stray half-written `.part`.
        tokio::task::spawn_blocking(move || {
            if let Some(ogg) = ogg {
                std::fs::write(part.path(), ogg).context("writing file")?;
            }
            output::write_tags(part.path(), &tags)?;
            part.finish(&path)
        })
        .await?
        .map_err(unwritable)
    }

    /// Makes sure the track can be saved (see [`output::check_destination`]). The future
    /// doesn't borrow the downloader, whose reporter isn't `Sync`.
    fn check_destination(
        &self,
        track: &TrackInfo,
        part: &Path,
        kbps: u32,
    ) -> impl Future<Output = Result<()>> + Send + use<> {
        let output = self.settings.output.clone();
        let part = part.to_path_buf();
        let needed = room_needed(track.duration_ms, self.settings.format, kbps);
        // the drive may need a few seconds to spin up; keep that off the async threads
        async move {
            tokio::task::spawn_blocking(move || output::check_destination(&output, &part, needed))
                .await?
        }
    }

    async fn cover(&mut self, album: &AlbumInfo) -> Option<Arc<Vec<u8>>> {
        if let Some(cover) = self.covers.get(&album.id) {
            return cover.clone();
        }
        let cover = match album.cover {
            Some(id) => match fetch_cover(&self.http, id).await {
                Ok(bytes) => Some(Arc::new(bytes)),
                Err(e) => {
                    self.warn(&format!(
                        "Couldn't download cover art for {}: {e:#}",
                        album.name
                    ));
                    None
                }
            },
            None => None,
        };
        // tracks of an album are usually downloaded together; don't hoard images
        if self.covers.len() >= 16 {
            self.covers.clear();
        }
        self.covers.insert(album.id, cover.clone());
        cover
    }
}

/// Keeps the computer awake for as long as it runs, except while `pause` says paused.
/// Never finishes.
async fn stay_awake(pause: Option<watch::Receiver<bool>>) {
    let Some(mut pause) = pause else {
        let _awake = KeepAwake::new(AWAKE_REASON);
        return std::future::pending().await;
    };
    loop {
        let paused = *pause.borrow_and_update();
        let awake = (!paused).then(|| KeepAwake::new(AWAKE_REASON));
        if pause.changed().await.is_err() {
            // nothing can pause any more: stay as it is
            return std::future::pending().await;
        }
        drop(awake);
    }
}

/// The free space to insist on before downloading a track, generously: the file (FLAC
/// can be several times the size of the source), its cover art, and some to spare.
fn room_needed(duration_ms: u32, format: Format, source_kbps: u32) -> u64 {
    let kbps = match format {
        Format::Flac => 2_400, // more than raw 24-bit stereo at 44.1 kHz
        _ => source_kbps.max(320),
    };
    u64::from(duration_ms) * u64::from(kbps) / 8 + (64 << 20)
}

/// Notes a key request for the next run (or the other front end). Takes the connection
/// rather than the downloader, whose reporter isn't `Sync`.
async fn remember_key_request(conn: &Connection) {
    let path = conn.auth().dir().join(LAST_KEY_REQUEST_FILE);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if let Err(e) = tokio::fs::write(&path, now.to_string()).await {
        log::warn!("couldn't write {}: {e}", path.display());
    }
}

/// When an earlier run last requested an audio key, as an `Instant`, if it's recent
/// enough to matter.
fn last_key_request(path: &Path) -> Option<Instant> {
    let millis = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    let ago = time_since(millis, SystemTime::now())?;
    Instant::now().checked_sub(ago)
}

/// How long ago `millis` (since 1970) was, up to a day. A time in the future (the clock
/// was turned back) counts as just now, so the full gap is kept.
fn time_since(millis: u64, now: SystemTime) -> Option<Duration> {
    let then = SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
    let ago = now.duration_since(then).unwrap_or(Duration::ZERO);
    (ago < Duration::from_secs(24 * 60 * 60)).then_some(ago)
}

/// spotify.link share links redirect (sometimes via an HTML page) to open.spotify.com.
async fn follow_short_link(http: &reqwest::Client, url: &str) -> Result<Target> {
    let parse_full = |s: &str| match input::parse(s) {
        Ok(Target::Search(_) | Target::ShortLink(_)) | Err(_) => None,
        Ok(target) => Some(target),
    };
    let response = http
        .get(url)
        .send()
        .await
        .context("following the short link")?;
    if let Some(target) = parse_full(response.url().as_str()) {
        return Ok(target);
    }
    let body = response.text().await.unwrap_or_default();
    body.match_indices("https://open.spotify.com/")
        .filter_map(|(start, _)| {
            let link: String = body[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || ":/._-".contains(*c))
                .collect();
            parse_full(&link)
        })
        .next()
        .ok_or_else(|| {
            anyhow!("couldn't follow the short link; paste the open.spotify.com link instead")
        })
}

pub fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Downloads cover art, preferring the full-size original over the 640px rendition.
async fn fetch_cover(http: &reqwest::Client, id: FileId) -> Result<Vec<u8>> {
    let hex = id.to_base16()?;
    let mut candidates = Vec::new();
    // `ab67616d0000b273…` is the 640px album image; `ab67616d000082c1…` the original upload
    if hex.len() == 40 && hex.starts_with("ab67616d0000") {
        candidates.push(format!("ab67616d000082c1{}", &hex[16..]));
    }
    candidates.push(hex);

    let mut last_error = anyhow!("no cover art");
    for candidate in candidates {
        let result = async {
            let response = http
                .get(format!("https://i.scdn.co/image/{candidate}"))
                .send()
                .await?
                .error_for_status()?;
            anyhow::Ok(response.bytes().await?.to_vec())
        }
        .await;
        match result {
            Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
            Ok(_) => last_error = anyhow!("empty image"),
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_trips_on_refusals_in_a_row() {
        let mut breaker = KeyBreaker::default();
        for _ in 1..KEY_FAILURE_LIMIT {
            assert!(!breaker.failure(&KeyError::Refused));
        }
        assert!(breaker.failure(&KeyError::NoAnswer));
    }

    #[test]
    fn breaker_resets_on_success_and_ignores_connection_errors() {
        let mut breaker = KeyBreaker::default();
        for _ in 1..KEY_FAILURE_LIMIT {
            assert!(!breaker.failure(&KeyError::Refused));
        }
        breaker.success();
        for _ in 1..KEY_FAILURE_LIMIT {
            assert!(!breaker.failure(&KeyError::NoAnswer));
        }
        // a broken connection says nothing about the account
        for _ in 0..10 {
            assert!(!breaker.failure(&KeyError::Connection(anyhow!("reset"))));
        }
        assert!(breaker.failure(&KeyError::Refused));
    }

    #[test]
    fn playlists_go_under_albums_and_to_plex_when_asked() {
        use clap::Parser;
        let settings = |args: &[&str]| {
            let cli = crate::cli::Cli::try_parse_from(args).unwrap();
            Settings::from_args(&cli.download).unwrap()
        };
        if std::env::var_os("SPOTDL_PLAYLIST_TEMPLATE").is_none() {
            let cli = crate::cli::Cli::try_parse_from(["spotdl", "x"]).unwrap();
            assert_eq!(cli.download.playlist_template, None);
        }
        let defaults = settings(&["spotdl", "x"]);
        assert_eq!(defaults.m3u, M3u::Auto);
        if std::env::var_os("SPOTDL_PLEX").is_none() {
            assert!(defaults.plex.is_none());
            assert!(defaults.always_writes_m3u());
        }
        let plex = settings(&["spotdl", "x", "--plex"]);
        assert!(!plex.always_writes_m3u());
        if std::env::var_os("SPOTDL_PLEX_URL").is_none() {
            assert_eq!(plex.plex.unwrap().url(), plex::DEFAULT_URL);
        }
        assert_eq!(settings(&["spotdl", "x", "--no-m3u"]).m3u, M3u::Never);
        assert!(!settings(&["spotdl", "x", "--no-m3u"]).always_writes_m3u());
        let always = settings(&["spotdl", "x", "--plex", "--m3u"]);
        assert_eq!(always.m3u, M3u::Always);
        assert!(always.always_writes_m3u());
        assert!(crate::cli::Cli::try_parse_from(["spotdl", "x", "--m3u", "--no-m3u"]).is_err());
        let elsewhere = settings(&["spotdl", "x", "--plex", "--plex-url", "192.168.1.20:32400/"]);
        assert_eq!(elsewhere.plex.unwrap().url(), "http://192.168.1.20:32400");
        // a broken address only matters once Plex is turned on
        let broken = ["spotdl", "x", "--plex-url", "http://"];
        let cli = crate::cli::Cli::try_parse_from(broken).unwrap();
        assert!(Settings::plex_server(&cli.download).is_err());
        if std::env::var_os("SPOTDL_PLEX").is_none() {
            assert!(Settings::from_args(&cli.download).is_ok());
        }
        let cli =
            crate::cli::Cli::try_parse_from(["spotdl", "x", "--plex", "--plex-url", "http://"]);
        assert!(Settings::from_args(&cli.unwrap().download).is_err());

        let s = settings(&["spotdl", "x", "-t", "{title}"]);
        assert_eq!(
            format!("{:?}", s.playlist_template),
            format!("{:?}", s.template)
        );
    }

    /// The token never shows, even in the settings' debug output.
    #[test]
    fn hides_the_plex_token() {
        use clap::Parser;
        let secret = "s3cr3t-t0ken-value";
        let cli =
            crate::cli::Cli::try_parse_from(["spotdl", "x", "--plex", "--plex-token", secret]);
        let args = cli.unwrap().download;
        let settings = Settings::from_args(&args).unwrap();
        assert!(!format!("{args:?}").contains(secret));
        assert!(!format!("{settings:?}").contains(secret));
    }

    #[test]
    fn writes_playlist_files_only_when_plex_doesnt_have_the_playlist() {
        use plex::Outcome;
        let outcomes = [
            (Outcome::NotSetUp, true),
            (Outcome::NoLibrary, true),
            (Outcome::Synced(plex::Synced::default()), false),
            // the playlist goes to Plex once running spotdl again works
            (Outcome::Failed("Plex isn't running".into()), false),
        ];
        for (outcome, by_default) in outcomes {
            assert_eq!(writes_m3u(M3u::Auto, &outcome), by_default, "{outcome:?}");
            assert!(writes_m3u(M3u::Always, &outcome), "{outcome:?}");
            assert!(!writes_m3u(M3u::Never, &outcome), "{outcome:?}");
        }
    }

    #[test]
    fn waits_50_seconds_between_key_requests_by_default() {
        use clap::Parser;
        // (SPOTDL_DELAY, where it's set, takes the default's place)
        if std::env::var_os("SPOTDL_DELAY").is_none() {
            let cli = crate::cli::Cli::try_parse_from(["spotdl", "x"]).unwrap();
            let settings = Settings::from_args(&cli.download).unwrap();
            assert_eq!(settings.delay, Duration::from_secs(50));
        }
    }

    #[test]
    fn remembers_the_last_key_request_across_runs() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let millis = |t: SystemTime| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        let ten_ago = now - Duration::from_secs(10);
        assert_eq!(
            time_since(millis(ten_ago), now),
            Some(Duration::from_secs(10))
        );
        // the clock went back: wait the full gap
        let later = now + Duration::from_secs(60);
        assert_eq!(time_since(millis(later), now), Some(Duration::ZERO));
        // long ago doesn't matter
        assert_eq!(
            time_since(millis(now - Duration::from_secs(2 * 86400)), now),
            None
        );
    }

    #[test]
    fn asks_for_room_for_the_whole_track() {
        // four minutes of Spotify's 320 kbps Ogg is 9.6 MB, plus the margin
        assert_eq!(
            room_needed(240_000, Format::Ogg, 320),
            9_600_000 + (64 << 20)
        );
        assert_eq!(
            room_needed(240_000, Format::Ogg, 160),
            9_600_000 + (64 << 20)
        );
        assert_eq!(
            room_needed(240_000, Format::Flac, 320),
            72_000_000 + (64 << 20)
        );
    }

    #[cfg(unix)]
    struct Quiet;

    #[cfg(unix)]
    impl Reporter for Quiet {
        fn event(&mut self, _: Event<'_>) {}
    }

    #[cfg(unix)]
    fn playable_track(number: u32) -> TrackInfo {
        use librespot_metadata::audio::AudioFileFormat;
        let id = SpotifyId::from_base62("3uNaLrScYulpXLRmQquP9I").unwrap();
        let files = [(AudioFileFormat::OGG_VORBIS_320, FileId::from_raw(&[7; 20]))];
        TrackInfo {
            id,
            title: format!("Song {number}"),
            artists: vec!["Artist".into()],
            artist_ids: vec![],
            album: Arc::new(AlbumInfo {
                id,
                name: "Album".into(),
                kind: "Album",
                artists: vec!["Artist".into()],
                date: None,
                label: None,
                copyright: None,
                discs: vec![(1, 3)],
                tracks: vec![],
                cover: None,
                thumbnail: None,
            }),
            number,
            disc: 1,
            duration_ms: 200_000,
            isrc: None,
            playable: Ok(meta::Playable {
                id,
                files: files.into(),
            }),
        }
    }

    /// When the download folder can't take files, the input stops at its first track,
    /// before an audio key is requested for it (the session isn't even connected).
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_folder_stops_before_any_key_request() {
        use clap::Parser;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("spotdl-unwritable-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let (config, music) = (root.join("config"), root.join("music"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&music).unwrap();
        std::fs::set_permissions(&music, std::fs::Permissions::from_mode(0o555)).unwrap();
        // root may write anywhere, so there's nothing to check then
        if std::fs::write(music.join("probe"), b"").is_err() {
            let auth = crate::auth::Auth::new(Some(&config)).unwrap();
            let session = Session::new(Default::default(), None);
            let conn = Arc::new(Connection::new(auth, session));
            let args = ["spotdl", "x", "-o", music.to_str().unwrap(), "--delay", "0"];
            let cli = crate::cli::Cli::try_parse_from(args).unwrap();
            let settings = Settings::from_args(&cli.download).unwrap();
            let mut d = Downloader::new(conn, settings, Box::new(Quiet)).unwrap();

            let items = (1..=3).map(|n| Item::Track(playable_track(n))).collect();
            let run = d.download_collection(Collection::of(None, items));
            // a key request would wait for an answer that never comes
            tokio::time::timeout(Duration::from_secs(20), run)
                .await
                .expect("an audio key was requested");

            let problem = d.stats.unwritable.clone().expect("stopped for the folder");
            assert!(problem.starts_with("Can't save to "), "{problem}");
            assert_eq!((d.stats.failures.len(), d.stats.not_attempted), (1, 2));
            assert_eq!(d.last_key_request, None, "an audio key was requested");
            assert!(!config.join(LAST_KEY_REQUEST_FILE).exists());
        }
        std::fs::set_permissions(&music, std::fs::Permissions::from_mode(0o755)).unwrap();
        _ = std::fs::remove_dir_all(&root);
    }

    /// The desktop app runs downloads as a task on a multi-threaded runtime.
    #[test]
    fn downloads_can_run_on_any_thread() {
        fn send<T: Send>(_: &T) {}
        fn check(d: &mut Downloader, input: &Input) {
            send(&d.process(input));
        }
        _ = check;
    }
}
