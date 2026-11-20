//! Keeping playlists in Plex Media Server in step with Spotify.
//!
//! Plex never imports `.m3u8` files that sit inside a library, and its own playlist import
//! (`POST /playlists/upload`) quietly ignores `.m3u8` files and matches `.m3u` paths
//! case-sensitively, dropping tracks whose folder is spelled in another case on the drive.
//! So once a playlist is downloaded, spotdl has Plex scan the music library, matches the
//! playlist's files against the tracks Plex has, itself, and writes the playlist through
//! Plex's HTTP API: created the first time, and after that replaced in place, so it keeps
//! its ID. Plex's database is never written to directly.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::header::{ACCEPT, HeaderValue};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::engine::count;
use crate::output::{self, M3uEntry};

/// Where Plex Media Server listens when it runs on this computer.
pub const DEFAULT_URL: &str = "http://127.0.0.1:32400";

/// Long enough for a server on the local network to answer, short enough that a computer
/// without Plex hardly notices the try.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest one request may take. A page of tracks is about 1.3 MB, and Plex answers
/// even that in a fraction of a second.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How often to ask whether a scan is still going.
const POLL: Duration = Duration::from_millis(500);
/// How long a scan may take to show up. Until one does, a scan of a few folders may
/// already be over, too quickly to be seen at all.
const SCAN_APPEARS: Duration = Duration::from_secs(5);
/// The longest to wait for a scan, even of a big library on a slow drive.
const SCAN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Tracks asked for at a time when listing the library (it has thousands).
const PAGE: usize = 1000;
/// Tracks added to a playlist per request: they're all in the URL, which has to stay
/// short enough for Plex to take.
const CHUNK: usize = 100;
/// How spotdl introduces itself to Plex.
const PRODUCT: &str = "spotdl";

/// A Plex token. It lets anyone do anything on the server that its account can, so it's
/// never shown: not in messages, logs or `Debug` output, and it's sent in a header
/// (marked sensitive), never in a URL.
#[derive(Clone, Default)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(hidden)")
    }
}

impl std::str::FromStr for Token {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.trim().to_string()))
    }
}

/// Where the token came from, for saying which one Plex turned down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Given,
    Found,
}

/// Which Plex server to keep playlists in, and the token to get in with.
#[derive(Debug, Clone)]
pub struct Config {
    url: String,
    /// `--plex-token`; without one, Plex Media Server's own on this computer is used.
    token: Option<Token>,
}

impl Config {
    /// `url` may leave out `http://`, as in `192.168.1.20:32400`.
    pub fn new(url: &str, token: Option<Token>) -> Result<Self> {
        let url = url.trim();
        let url = if url.contains("://") {
            url.to_string()
        } else {
            format!("http://{url}")
        };
        let parsed = reqwest::Url::parse(&url).with_context(|| format!("“{url}” isn't a URL"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some(),
            "“{url}” isn't the address of a server (like {DEFAULT_URL})"
        );
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.0.is_empty()),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether Plex is set up here, as far as spotdl can tell without asking it: a token
    /// was given, or Plex Media Server keeps one on this computer.
    pub async fn set_up(&self) -> bool {
        self.token().await.is_some()
    }

    async fn token(&self) -> Option<(Token, Source)> {
        if let Some(token) = &self.token {
            return Some((token.clone(), Source::Given));
        }
        find_token().await.map(|token| (token, Source::Found))
    }
}

/// How syncing a playlist to Plex went.
#[derive(Debug)]
pub enum Outcome {
    /// Plex is turned off (no `--plex`), or no token was given or found, so it isn't set
    /// up here. Nothing was tried.
    NotSetUp,
    /// Plex has no music library holding the download folder.
    NoLibrary,
    Synced(Synced),
    /// It didn't work. The message says why, and how to try again.
    Failed(String),
}

impl Outcome {
    /// Whether the playlist's `.m3u8` file is all there is of it: Plex isn't set up, or
    /// doesn't keep the download folder. Otherwise it's in Plex, or will be once running
    /// spotdl on the same link again works.
    pub fn needs_m3u(&self) -> bool {
        matches!(self, Self::NotSetUp | Self::NoLibrary)
    }
}

/// A playlist brought up to date in Plex.
#[derive(Debug, Default)]
pub struct Synced {
    /// Its title in Plex: the playlist's name on Spotify.
    pub title: String,
    /// Plex's ID for it (its `ratingKey`).
    pub id: String,
    pub created: bool,
    /// The title it was found under and renamed from: the name its `.m3u8` file had,
    /// from before Plex kept the playlist (characters file names can't have replaced).
    pub renamed_from: Option<String>,
    /// It already had exactly these tracks, so they were left as they were.
    pub unchanged: bool,
    /// Downloaded tracks (each file once) that are in it now, out of `total`.
    pub matched: usize,
    pub total: usize,
    /// The tracks Plex doesn't have, as paths under the download folder.
    pub unmatched: Vec<String>,
    /// Tracks that are in the Spotify playlist more than once. Plex can't hold a track
    /// twice in a playlist, so these are in it once.
    pub repeats: usize,
    /// Anything else worth knowing: other playlists with the same title, or Plex's
    /// playlist not coming out as it was sent.
    pub warnings: Vec<String>,
}

impl Synced {
    /// What was done, e.g. `Updated the playlist “Chill” in Plex (48/50 tracks)`.
    pub fn summary(&self) -> String {
        let title = &self.title;
        let mut details = vec![format!("{}/{} tracks", self.matched, self.total)];
        if self.repeats > 0 {
            details.push(format!(
                "{} in it once, as Plex can't hold a track twice",
                count(self.repeats, "repeat")
            ));
        }
        let details = details.join("; ");
        match (&self.renamed_from, self.created, self.unchanged) {
            (_, true, _) => format!("Added the playlist “{title}” to Plex ({details})"),
            (Some(old), _, true) => {
                format!("Renamed the playlist “{old}” in Plex to “{title}” ({details})")
            }
            (Some(old), _, false) => format!(
                "Updated the playlist “{title}” in Plex, and renamed it from “{old}” ({details})"
            ),
            (None, _, true) => format!("The playlist “{title}” in Plex is up to date ({details})"),
            (None, _, false) => format!("Updated the playlist “{title}” in Plex ({details})"),
        }
    }

    /// What's missing from the playlist in Plex, in a sentence, if anything is.
    pub fn left_out(&self) -> Option<String> {
        let first = self.unmatched.first()?;
        let title = &self.title;
        let n = self.unmatched.len();
        Some(if n == 1 {
            format!(
                "{first} isn't in Plex's music library, so it's left out of the playlist \
                 “{title}” there. Running spotdl on the same link again adds it once Plex has it."
            )
        } else {
            format!(
                "{n} of the tracks of “{title}” aren't in Plex's music library (one is \
                 {first}), so they're left out of its playlist there. Running spotdl on the \
                 same link again adds them once Plex has them."
            )
        })
    }
}

/// Brings the playlist `name` up to date in Plex: `entries` are its downloaded tracks, in
/// order, with paths under `output`. Calls `status` with what it's doing meanwhile, since
/// waiting for a scan can take a while. Never fails: problems are in the outcome.
pub async fn sync(
    config: &Config,
    output: &Path,
    name: &str,
    entries: &[M3uEntry],
    status: &mut (dyn FnMut(&str) + Send),
) -> Outcome {
    let Some((token, source)) = config.token().await else {
        log::debug!("no Plex token given or found, so Plex isn't set up here");
        return Outcome::NotSetUp;
    };
    let result = match Client::new(&config.url, &token, source) {
        Ok(client) => run(&client, output, name, entries, status).await,
        Err(e) => Err(e),
    };
    result.unwrap_or_else(|e| Outcome::Failed(failure(name, &e)))
}

/// What to tell the user when syncing failed: what happened, and how to try again.
fn failure(name: &str, e: &anyhow::Error) -> String {
    let once = if e.is::<Unreachable>() {
        "once Plex is running"
    } else {
        "once that's fixed"
    };
    format!(
        "Couldn't update the playlist “{name}” in Plex: {e:#}. Run spotdl on the same link \
         again {once} (tracks you already have won't be downloaded again)."
    )
}

/// What to tell the user when playlists should go to Plex but there's no token to use.
/// The playlist gets its `.m3u8` file instead (unless `--no-m3u`), so it isn't lost.
pub fn no_token(name: &str) -> String {
    format!(
        "Couldn't add the playlist “{name}” to Plex: no Plex token was found on this computer. \
         Is Plex Media Server set up here? If it's on another computer, give its address and \
         token with --plex-url and --plex-token."
    )
}

async fn run(
    client: &Client,
    output: &Path,
    name: &str,
    entries: &[M3uEntry],
    status: &mut (dyn FnMut(&str) + Send),
) -> Result<Outcome> {
    status("Connecting to Plex...");
    let server = client.server_id().await?;
    let sections = client.sections().await?;
    // the folders as this computer sees them (symlinks followed), to compare with
    // the download folder; slow if the drive has to spin up, so off the async threads
    let mut roots = HashMap::new();
    for location in sections
        .iter()
        .filter(|s| s.music)
        .flat_map(|s| &s.locations)
    {
        let local = tokio::fs::canonicalize(location)
            .await
            .unwrap_or_else(|_| PathBuf::from(location));
        roots.insert(location.clone(), local);
    }
    let folder = tokio::fs::canonicalize(output)
        .await
        .unwrap_or_else(|_| output.to_path_buf());
    let Some(library) = library_for(&sections, &folder, &|location| roots[location].clone()) else {
        return Ok(Outcome::NoLibrary);
    };
    let (files, repeats) = files(entries, output, &folder, &library.local);

    status("Waiting for Plex to scan the music library...");
    client.scan(&library, &[]).await?;
    status("Matching the playlist with Plex's tracks...");
    let mut plan = Plan::new(&files, &client.tracks(&library).await?);
    if !plan.unmatched.is_empty() {
        // A scan can miss files, say ones Plex's own scan of the folder was in the middle
        // of. One more look, at just their folders.
        let missing = plan.unmatched.iter().map(|&i| &files[i]);
        let folders = plex_folders(missing, &library).await;
        if !folders.is_empty() {
            let n = plan.unmatched.len();
            status(&format!(
                "Asking Plex to look again for {} it doesn't have...",
                count(n, "track")
            ));
            client.scan(&library, &folders).await?;
            plan = Plan::new(&files, &client.tracks(&library).await?);
        }
    }

    let playlists = client.playlists().await?;
    let found = find(&playlists, name);
    if plan.keys.is_empty() {
        let left = if found.is_some() {
            "so its playlist there was left as it was"
        } else {
            "so it wasn't added"
        };
        bail!(
            "none of its {} are in Plex's music library, {left}",
            count(files.len(), "track")
        );
    }

    status(&format!("Updating the playlist “{name}” in Plex..."));
    let mut warnings = Vec::new();
    let (id, created, renamed_from, unchanged, now) = match found {
        Some(found) => {
            let id = found.playlist.key.clone();
            if found.others > 0 {
                warnings.push(format!(
                    "Plex has {} playlists called “{}”; spotdl updated the first one",
                    found.others + 1,
                    found.playlist.title
                ));
            }
            let renamed_from = if found.rename {
                client.rename(&id, name).await?;
                Some(found.playlist.title.clone())
            } else {
                None
            };
            let before = client.items(&id).await?;
            let unchanged = before == plan.keys;
            let now = if unchanged {
                before
            } else {
                client.fill(&server, Some(&id), name, &plan.keys).await?;
                client.items(&id).await?
            };
            (id, false, renamed_from, unchanged, now)
        }
        None => {
            let id = client.fill(&server, None, name, &plan.keys).await?;
            let now = client.items(&id).await?;
            (id, true, None, false, now)
        }
    };
    if let Some(problem) = mismatch(&now, &plan.keys) {
        warnings.push(format!(
            "The playlist “{name}” in Plex didn't come out as it was sent: {problem}. Run \
             spotdl on the same link again to fix it."
        ));
    }

    Ok(Outcome::Synced(Synced {
        title: name.to_string(),
        id,
        created,
        renamed_from,
        unchanged,
        matched: files.len() - plan.unmatched.len(),
        total: files.len(),
        unmatched: plan
            .unmatched
            .iter()
            .map(|&i| files[i].shown.clone())
            .collect(),
        repeats,
        warnings,
    }))
}

/// How the playlist Plex has (`now`) differs from what was sent, if it does.
fn mismatch(now: &[String], sent: &[String]) -> Option<String> {
    if now == sent {
        return None;
    }
    if now.len() != sent.len() {
        return Some(format!("it has {} tracks, not {}", now.len(), sent.len()));
    }
    let mut a = now.to_vec();
    let mut b = sent.to_vec();
    a.sort();
    b.sort();
    Some(if a == b {
        "its tracks are in another order".into()
    } else {
        "some of its tracks are different ones".into()
    })
}

// ---------------------------------------------------------------------------------------
// Talking to Plex

/// Plex couldn't be reached at all: it isn't running, or isn't at that address.
#[derive(Debug)]
struct Unreachable(String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreachable {}

/// One Plex server, as the token's account.
struct Client {
    http: reqwest::Client,
    url: String,
    token: HeaderValue,
    source: Source,
}

impl Client {
    fn new(url: &str, token: &Token, source: Source) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // Plex is on this computer or the local network, where a proxy set up for the
            // internet would only be in the way
            .no_proxy()
            .build()?;
        let mut token = HeaderValue::from_str(&token.0)
            .map_err(|_| anyhow!("the Plex token has characters a token can't have"))?;
        // (kept out of reqwest's and hyper's debug output)
        token.set_sensitive(true);
        Ok(Self {
            http,
            url: url.to_string(),
            token,
            source,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.url))
            .header("X-Plex-Token", self.token.clone())
            .header(ACCEPT, "application/json")
            .header("X-Plex-Client-Identifier", PRODUCT)
            .header("X-Plex-Product", PRODUCT)
    }

    /// Sends a request, makes sure Plex took it, and returns what it answered.
    async fn send(&self, request: RequestBuilder) -> Result<String> {
        let response = request.send().await.map_err(|e| self.unreachable(e))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            bail!(self.refused());
        }
        ensure!(status.is_success(), "Plex answered {status}");
        response.text().await.map_err(|e| self.unreachable(e))
    }

    async fn call(&self, method: Method, path: &str) -> Result<String> {
        self.send(self.request(method, path)).await
    }

    async fn get(&self, path: &str) -> Result<Value> {
        json(&self.call(Method::GET, path).await?)
    }

    /// Goes through a list that Plex hands out a page at a time, passing `each` every
    /// page's items. A page at a time, because each track comes with all its details:
    /// the whole library at once would take tens of MB.
    async fn pages(&self, path: &str, mut each: impl FnMut(&[Value])) -> Result<()> {
        let mut start = 0;
        loop {
            let request = self
                .request(Method::GET, path)
                .header("X-Plex-Container-Start", start)
                .header("X-Plex-Container-Size", PAGE);
            let page = json(&self.send(request).await?)?;
            let container = &page["MediaContainer"];
            let items = container["Metadata"]
                .as_array()
                .map_or(&[][..], Vec::as_slice);
            each(items);
            start += items.len();
            let total = container["totalSize"].as_u64();
            if items.len() < PAGE || total.is_some_and(|total| start as u64 >= total) {
                return Ok(());
            }
        }
    }

    fn unreachable(&self, e: reqwest::Error) -> anyhow::Error {
        let url = &self.url;
        let what = if e.is_timeout() {
            format!("Plex at {url} didn't answer in time")
        } else if e.is_connect() {
            format!("couldn't connect to Plex at {url}")
        } else {
            // reqwest's own message is just "error sending request"; the why follows
            let e = e.without_url();
            let mut reason = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                _ = write!(reason, ": {cause}");
                source = cause.source();
            }
            format!("lost the connection to Plex at {url} ({reason})")
        };
        Unreachable(what).into()
    }

    fn refused(&self) -> String {
        match self.source {
            Source::Given => {
                "Plex didn't accept the token given with --plex-token or SPOTDL_PLEX_TOKEN".into()
            }
            Source::Found => "Plex didn't accept the token Plex Media Server keeps on this \
                computer (give it one with --plex-token)"
                .into(),
        }
    }

    /// The server's own ID, which the URIs of its tracks start with.
    async fn server_id(&self) -> Result<String> {
        let identity = self.get("/identity").await?;
        text(&identity["MediaContainer"]["machineIdentifier"])
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Plex didn't say which server it is"))
    }

    async fn sections(&self) -> Result<Vec<Section>> {
        let json = self
            .get("/library/sections")
            .await
            .context("listing Plex's libraries")?;
        Ok(sections(&json))
    }

    async fn section(&self, key: &str) -> Result<Section> {
        self.sections()
            .await?
            .into_iter()
            .find(|s| s.key == key)
            .ok_or_else(|| anyhow!("the music library is gone from Plex"))
    }

    /// Has Plex scan the library, or just `folders` in it, and waits until it's done.
    async fn scan(&self, library: &Library, folders: &[String]) -> Result<()> {
        let before = self.section(&library.key).await?.scanned_at;
        let refresh = format!("/library/sections/{}/refresh", library.key);
        if folders.is_empty() {
            self.call(Method::GET, &refresh).await
        } else {
            // Plex queues these and scans the folders one after another
            let mut result = Ok(String::new());
            for folder in folders {
                let path = format!("{refresh}?path={}", encode(folder));
                result = self.call(Method::GET, &path).await;
                if result.is_err() {
                    break;
                }
            }
            result
        }
        .context("asking Plex to scan the music library")?;
        self.wait_for_scan(&library.key, before).await
    }

    /// Waits for Plex to finish scanning the library. A scan can take a moment to show
    /// up, and a small one can be over before it's seen; so it's done once it's been
    /// seen and gone, or once Plex notes a finished scan, or if none shows up at all for
    /// a few seconds. Gone means not seen twice in a row, as queued scans run one after
    /// another with moments in between.
    async fn wait_for_scan(&self, key: &str, scanned_before: Option<i64>) -> Result<()> {
        let started = Instant::now();
        let mut seen = false;
        let mut idle = 0;
        loop {
            let section = self.section(key).await?;
            if section.refreshing || self.scanning(key).await? {
                seen = true;
                idle = 0;
            } else {
                idle += 1;
            }
            let over =
                seen || section.scanned_at != scanned_before || started.elapsed() >= SCAN_APPEARS;
            if idle >= 2 && over {
                return Ok(());
            }
            if started.elapsed() >= SCAN_TIMEOUT {
                bail!(
                    "Plex was still scanning the music library after {} minutes",
                    SCAN_TIMEOUT.as_secs() / 60
                );
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Whether Plex is busy scanning the library `key`, as its activities say.
    async fn scanning(&self, key: &str) -> Result<bool> {
        let json = self.get("/activities").await?;
        let activities = json["MediaContainer"]["Activity"].as_array();
        Ok(activities.into_iter().flatten().any(|activity| {
            // (a scan says which library it is a moment after it starts)
            let library = text(&activity["Context"]["librarySectionID"]);
            activity["type"] == "library.update.section" && library.is_none_or(|l| l == key)
        }))
    }

    /// Plex's tracks in the library, by where their files are in it.
    async fn tracks(&self, library: &Library) -> Result<HashMap<String, String>> {
        let path = format!("/library/sections/{}/all?type=10", library.key);
        let root = Path::new(&library.location);
        let mut tracks = HashMap::new();
        self.pages(&path, |items| index(&mut tracks, items, root))
            .await
            .context("listing the music library's tracks")?;
        Ok(tracks)
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        let json = self
            .get("/playlists?playlistType=audio")
            .await
            .context("listing Plex's playlists")?;
        Ok(playlists(&json))
    }

    /// The tracks in the playlist `id`, in order.
    async fn items(&self, id: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let path = format!("/playlists/{id}/items");
        self.pages(&path, |items| {
            keys.extend(items.iter().filter_map(|i| text(&i["ratingKey"])));
        })
        .await
        .context("reading the playlist back")?;
        Ok(keys)
    }

    async fn rename(&self, id: &str, title: &str) -> Result<()> {
        let path = format!("/playlists/{id}?title={}", encode(title));
        self.call(Method::PUT, &path)
            .await
            .context("renaming the playlist")?;
        Ok(())
    }

    /// Puts `keys` (at least one) into a playlist, in order, in place of what was there:
    /// the playlist `id`, or a new one called `title`. Returns its ID.
    async fn fill(
        &self,
        server: &str,
        id: Option<&str>,
        title: &str,
        keys: &[String],
    ) -> Result<String> {
        let uris = uris(server, keys);
        let mut uris = uris.iter();
        let id = match id {
            Some(id) => {
                self.call(Method::DELETE, &format!("/playlists/{id}/items"))
                    .await
                    .context("emptying the playlist")?;
                id.to_string()
            }
            None => {
                let first = uris.next().context("no tracks to add")?;
                let path = format!(
                    "/playlists?type=audio&smart=0&title={}&uri={}",
                    encode(title),
                    encode(first)
                );
                let created = json(
                    &self
                        .call(Method::POST, &path)
                        .await
                        .context("creating the playlist")?,
                )?;
                text(&created["MediaContainer"]["Metadata"][0]["ratingKey"])
                    .ok_or_else(|| anyhow!("Plex didn't say which playlist it created"))?
            }
        };
        // added in order, a chunk at a time, they stay in order
        for uri in uris {
            let path = format!("/playlists/{id}/items?uri={}", encode(uri));
            self.call(Method::PUT, &path)
                .await
                .context("adding tracks to the playlist")?;
        }
        Ok(id)
    }
}

fn json(body: &str) -> Result<Value> {
    serde_json::from_str(body).context("Plex's answer wasn't what was expected")
}

/// A JSON string or number as text: Plex sends IDs as strings, but not everywhere.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A JSON flag, which Plex may send as `true` or as `1`.
fn flag(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_u64().is_some_and(|n| n != 0),
        Value::String(s) => s == "1" || s == "true",
        _ => false,
    }
}

/// Percent-encodes `text` for a URL's query: everything but letters, digits and `-._~`,
/// so a title's `+`, `&`, `#` or `?` reach Plex as themselves.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The `uri`s that add `keys` to a playlist, in order, [`CHUNK`] tracks per request.
fn uris(server: &str, keys: &[String]) -> Vec<String> {
    keys.chunks(CHUNK)
        .map(|chunk| {
            format!(
                "server://{server}/com.plexapp.plugins.library/library/metadata/{}",
                chunk.join(",")
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Libraries and tracks

/// A library in Plex.
#[derive(Debug, Clone)]
struct Section {
    key: String,
    music: bool,
    /// Its folders, as Plex knows them.
    locations: Vec<String>,
    /// Whether it's being scanned.
    refreshing: bool,
    /// When Plex last finished scanning it.
    scanned_at: Option<i64>,
}

fn sections(json: &Value) -> Vec<Section> {
    let sections = json["MediaContainer"]["Directory"].as_array();
    sections
        .into_iter()
        .flatten()
        .filter_map(|d| {
            Some(Section {
                key: text(&d["key"])?,
                music: d["type"] == "artist",
                locations: d["Location"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["path"].as_str().map(String::from))
                    .collect(),
                refreshing: flag(&d["refreshing"]),
                scanned_at: d["scannedAt"].as_i64(),
            })
        })
        .collect()
}

/// The music library the playlist goes into.
#[derive(Debug, Clone, PartialEq)]
struct Library {
    key: String,
    /// Its folder that holds the download folder, as Plex knows it: Plex's paths for the
    /// tracks in it start with this...
    location: String,
    /// ...and as this computer sees it, which the downloaded files' paths start with.
    local: PathBuf,
}

/// The music library holding `folder` (as this computer sees it). `local` says how this
/// computer sees each library folder. If more than one holds it, the one closest to it.
fn library_for(
    sections: &[Section],
    folder: &Path,
    local: &dyn Fn(&str) -> PathBuf,
) -> Option<Library> {
    sections
        .iter()
        .filter(|s| s.music)
        .flat_map(|s| s.locations.iter().map(move |location| (s, location)))
        .map(|(s, location)| (s, location, local(location)))
        .filter(|(_, _, root)| relative_key(folder, root).is_some())
        .max_by_key(|(_, _, root)| root.components().count())
        .map(|(s, location, root)| Library {
            key: s.key.clone(),
            location: location.clone(),
            local: root,
        })
}

/// Where `path` is under `root`, as a key that compares the way the music drive does: in
/// Unicode NFC (an accented letter can be one character or two), and ignoring case. exFAT,
/// APFS and NTFS ignore case by default, so a track whose folder differs from an existing
/// one only in case lands in that folder, and spotdl's path for it and Plex's differ.
/// `None` if `path` isn't under `root`.
fn relative_key(path: &Path, root: &Path) -> Option<String> {
    let path = folded(path);
    let rest = path.strip_prefix(folded(root).as_slice())?;
    Some(rest.join("/"))
}

fn folded(path: &Path) -> Vec<String> {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| {
            c.as_os_str()
                .to_string_lossy()
                .to_lowercase()
                .nfc()
                .collect()
        })
        .collect()
}

/// Adds Plex's tracks (`/library/sections/…/all` items) to `index`, by where their files
/// are under `root` (see [`relative_key`]), each mapped to its `ratingKey`.
fn index(index: &mut HashMap<String, String>, items: &[Value], root: &Path) {
    for item in items {
        let Some(key) = text(&item["ratingKey"]) else {
            continue;
        };
        let media = item["Media"].as_array().into_iter().flatten();
        let parts = media.flat_map(|m| m["Part"].as_array().into_iter().flatten());
        for part in parts {
            if let Some(file) = part["file"].as_str()
                && let Some(relative) = relative_key(Path::new(file), root)
            {
                index.entry(relative).or_insert_with(|| key.clone());
            }
        }
    }
}

/// One of the playlist's downloaded files.
#[derive(Debug)]
struct File {
    /// Where it is in the library, for matching (see [`relative_key`]); `None` if it's
    /// outside the library's folder.
    key: Option<String>,
    /// Where it is under the download folder, as spelled, for messages.
    shown: String,
    /// The file as this computer sees it.
    local: PathBuf,
}

/// The playlist's files, each once and in order, and how many repeats were left out.
/// `entries` are under `output`, which this computer sees as `folder`; the library's
/// folder is `root`.
fn files(entries: &[M3uEntry], output: &Path, folder: &Path, root: &Path) -> (Vec<File>, usize) {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for entry in entries {
        let relative = entry.path.strip_prefix(output).unwrap_or(&entry.path);
        let local = folder.join(relative);
        let key = relative_key(&local, root);
        let id = key
            .clone()
            .unwrap_or_else(|| local.to_string_lossy().into_owned());
        if seen.insert(id) {
            let shown = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.push(File { key, shown, local });
        }
    }
    let repeats = entries.len() - files.len();
    (files, repeats)
}

/// What the playlist in Plex should hold, worked out before anything there is changed.
#[derive(Debug, PartialEq)]
struct Plan {
    /// Plex's IDs for the tracks, in order, each once.
    keys: Vec<String>,
    /// The files Plex has no track for (indexes into the files).
    unmatched: Vec<usize>,
}

impl Plan {
    fn new(files: &[File], tracks: &HashMap<String, String>) -> Self {
        let mut keys = Vec::new();
        let mut unmatched = Vec::new();
        for (i, file) in files.iter().enumerate() {
            match file.key.as_ref().and_then(|key| tracks.get(key)) {
                Some(key) => keys.push(key.clone()),
                None => unmatched.push(i),
            }
        }
        Self {
            keys: dedupe(keys),
            unmatched,
        }
    }
}

/// Keeps the first of each, in order. Plex silently drops a track that's added to a
/// playlist twice, which would leave it never quite matching what was sent.
fn dedupe(keys: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    keys.into_iter()
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

/// The folders holding `files`, as Plex knows them, for scanning just those. Each is
/// spelled as it is on the drive (which canonicalizing gives on macOS and Windows): told
/// to scan a folder by another spelling, Plex could add its tracks a second time.
async fn plex_folders(files: impl Iterator<Item = &File>, library: &Library) -> Vec<String> {
    let mut folders = Vec::new();
    for file in files {
        let Some(dir) = file.local.parent() else {
            continue;
        };
        let Ok(dir) = tokio::fs::canonicalize(dir).await else {
            continue;
        };
        let Ok(relative) = dir.strip_prefix(&library.local) else {
            continue;
        };
        let folder = if relative.as_os_str().is_empty() {
            library.location.clone()
        } else {
            let folder = Path::new(&library.location).join(relative);
            folder.to_string_lossy().into_owned()
        };
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders
}

// ---------------------------------------------------------------------------------------
// Playlists

#[derive(Debug, Clone, PartialEq)]
struct Playlist {
    key: String,
    title: String,
    smart: bool,
    audio: bool,
}

fn playlists(json: &Value) -> Vec<Playlist> {
    let playlists = json["MediaContainer"]["Metadata"].as_array();
    playlists
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Playlist {
                key: text(&p["ratingKey"])?,
                title: p["title"].as_str()?.to_string(),
                smart: flag(&p["smart"]),
                audio: p["playlistType"] == "audio",
            })
        })
        .collect()
}

/// Plex's playlist for a Spotify playlist.
#[derive(Debug, PartialEq)]
struct Found<'a> {
    playlist: &'a Playlist,
    /// It has the title the playlist's `.m3u8` file was named with, and needs renaming.
    rename: bool,
    /// How many more there are with the same title.
    others: usize,
}

/// Plex's playlist for the Spotify playlist `name`: the one with the same title, or else
/// the one titled the way its `.m3u8` file was named (playlists moved into Plex from
/// those files). Smart playlists are left alone.
fn find<'a>(playlists: &'a [Playlist], name: &str) -> Option<Found<'a>> {
    let file_name = output::sanitize(name);
    [name, file_name.as_str()].into_iter().find_map(|title| {
        let mut same = playlists
            .iter()
            .filter(|p| p.audio && !p.smart && p.title == title);
        let playlist = same.next()?;
        Some(Found {
            playlist,
            rename: title != name,
            others: same.count(),
        })
    })
}

// ---------------------------------------------------------------------------------------
// Plex Media Server's own token

/// The token Plex Media Server keeps on this computer, from signing in to plex.tv. It's
/// the server owner's, so playlists made with it are the owner's, as if made in Plex.
#[cfg(target_os = "macos")]
async fn find_token() -> Option<Token> {
    let output = tokio::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.plexapp.plexmediaserver", "PlexOnlineToken"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim();
    (output.status.success() && !token.is_empty()).then(|| Token(token.to_string()))
}

/// See the macOS version. On Linux it's in Plex Media Server's `Preferences.xml`, which
/// often only the `plex` user can read.
#[cfg(target_os = "linux")]
async fn find_token() -> Option<Token> {
    let found = tokio::task::spawn_blocking(|| {
        preferences_files().into_iter().find_map(|path| {
            let xml = std::fs::read_to_string(path).ok()?;
            token_in_preferences(&xml)
        })
    });
    found.await.ok().flatten().map(Token)
}

/// Where Plex Media Server keeps its `Preferences.xml` on Linux: where
/// `PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR` says, the packages' and the snap's places,
/// and the home folder, for a server run as this user.
#[cfg(target_os = "linux")]
fn preferences_files() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("PLEX_MEDIA_SERVER_APPLICATION_SUPPORT_DIR") {
        roots.push(dir.into());
    }
    roots.push("/var/lib/plexmediaserver/Library/Application Support".into());
    roots.push("/var/snap/plexmediaserver/common/Library/Application Support".into());
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Library").join("Application Support"));
    }
    roots
        .into_iter()
        .map(|root| root.join("Plex Media Server").join("Preferences.xml"))
        .collect()
}

/// The `PlexOnlineToken` attribute of a `Preferences.xml`.
#[cfg(any(target_os = "linux", test))]
fn token_in_preferences(xml: &str) -> Option<String> {
    const ATTRIBUTE: &str = "PlexOnlineToken=\"";
    let (start, _) = xml.match_indices(ATTRIBUTE).find(|&(i, _)| {
        xml[..i]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    })?;
    let value = &xml[start + ATTRIBUTE.len()..];
    let token = value[..value.find('"')?].trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// See the macOS version. On Windows it's in the registry.
#[cfg(windows)]
async fn find_token() -> Option<Token> {
    registry_token().map(Token)
}

#[cfg(windows)]
fn registry_token() -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
    let key = wide(r"Software\Plex, Inc.\Plex Media Server");
    let value = wide("PlexOnlineToken");
    let mut buffer = [0u16; 256];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: both names are NUL-terminated UTF-16, and `buffer` has room for the `size`
    // bytes asked for.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let token = String::from_utf16(&buffer[..end]).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
async fn find_token() -> Option<Token> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn entry(path: &str) -> M3uEntry {
        M3uEntry {
            path: PathBuf::from(path),
            seconds: 0,
            label: String::new(),
        }
    }

    fn playlist(key: &str, title: &str, smart: bool) -> Playlist {
        Playlist {
            key: key.into(),
            title: title.into(),
            smart,
            audio: true,
        }
    }

    #[test]
    fn compares_paths_the_way_the_drive_does() {
        let root = Path::new("/Volumes/BigBoy/music");
        let key = |p: &str| relative_key(Path::new(p), root);
        let file = "/Volumes/BigBoy/music/Pixies/Death To The Pixies/01 - Cecilia Ann.ogg";
        let expected = Some("pixies/death to the pixies/01 - cecilia ann.ogg".to_string());
        assert_eq!(key(file), expected);
        // spotdl's spelling of a folder that's spelled otherwise on the drive
        let spotdl = "/Volumes/BigBoy/music/Pixies/Death to the Pixies/01 - Cecilia Ann.ogg";
        assert_eq!(key(spotdl), expected);
        // the root itself may be spelled otherwise, too
        assert_eq!(
            relative_key(Path::new(file), Path::new("/volumes/bigboy/MUSIC/")),
            expected
        );
        // an accented letter as one character (NFC) or as a letter and an accent (NFD)
        let nfc = key("/Volumes/BigBoy/music/Beyonc\u{e9}/Album/01 - Song.ogg");
        let nfd = key("/Volumes/BigBoy/music/Beyonce\u{301}/Album/01 - Song.ogg");
        assert_eq!(nfc, nfd);
        assert_eq!(
            key("/Volumes/BigBoy/music/\u{c9}t\u{e9}/x.ogg"),
            key("/Volumes/BigBoy/music/E\u{301}te\u{301}/X.ogg")
        );
        // outside the library
        assert_eq!(key("/Volumes/BigBoy/movies/x.mkv"), None);
        assert_eq!(key("/Volumes/BigBoy/music-old/x.ogg"), None);
        assert_eq!(key("/Volumes/BigBoy"), None);
        assert_eq!(key("/Volumes/BigBoy/music"), Some(String::new()));
    }

    /// A page of `/library/sections/…/all?type=10` with the given tracks.
    fn tracks(items: &[(&str, &[&str])]) -> Vec<Value> {
        items
            .iter()
            .map(|(key, files)| {
                let parts: Vec<Value> = files.iter().map(|f| json!({ "file": f })).collect();
                json!({ "ratingKey": key, "title": "x", "Media": [{ "Part": parts }] })
            })
            .collect()
    }

    #[test]
    fn matches_files_with_plexs_tracks() {
        let library = tracks(&[
            (
                "10",
                &["/Volumes/BigBoy/music/Pixies/Death To The Pixies/01 - Cecilia Ann.ogg"],
            ),
            (
                "11",
                &["/Volumes/BigBoy/music/Beyonce\u{301}/B'Day/01 - Deja Vu.ogg"],
            ),
            // one track with two files (Plex merges copies of a track)
            (
                "12",
                &[
                    "/Volumes/BigBoy/music/A/B/01 - One.ogg",
                    "/Volumes/BigBoy/music/A/B/01 - One.flac",
                ],
            ),
            ("13", &["/Volumes/Other/music/A/B/01 - One.ogg"]),
        ]);
        let mut index = HashMap::new();
        super::index(&mut index, &library, Path::new("/Volumes/BigBoy/music"));
        assert_eq!(index.len(), 4, "{index:?}");

        let output = Path::new("/Volumes/BigBoy/music");
        let entries = [
            entry("/Volumes/BigBoy/music/Beyonc\u{e9}/B'Day/01 - Deja Vu.ogg"),
            entry("/Volumes/BigBoy/music/Pixies/Death to the Pixies/01 - Cecilia Ann.ogg"),
            entry("/Volumes/BigBoy/music/Missing/Album/01 - Gone.ogg"),
            // the same file again, by another spelling
            entry("/Volumes/BigBoy/music/BEYONCÉ/B'Day/01 - Deja Vu.ogg"),
            entry("/Volumes/BigBoy/music/A/B/01 - One.flac"),
            entry("/Volumes/BigBoy/music/A/B/01 - One.ogg"),
        ];
        let (files, repeats) = files(&entries, output, output, output);
        assert_eq!((files.len(), repeats), (5, 1));
        assert_eq!(files[2].shown, "Missing/Album/01 - Gone.ogg");

        let plan = Plan::new(&files, &index);
        // two files of one track, once
        assert_eq!(plan.keys, strings(&["11", "10", "12"]));
        assert_eq!(plan.unmatched, vec![2]);
    }

    #[test]
    fn works_out_files_from_where_the_download_folder_really_is() {
        // downloads to a link to the library folder, or a relative path
        let entries = [entry("Music/Artist/Album/01 - Song.ogg")];
        let (files, _) = files(
            &entries,
            Path::new("Music"),
            Path::new("/Volumes/BigBoy/music"),
            Path::new("/Volumes/BigBoy/music"),
        );
        assert_eq!(files[0].key.as_deref(), Some("artist/album/01 - song.ogg"));
        assert_eq!(files[0].shown, "Artist/Album/01 - Song.ogg");
        assert_eq!(
            files[0].local,
            Path::new("/Volumes/BigBoy/music/Artist/Album/01 - Song.ogg")
        );
        // a download folder outside the library matches nothing
        let (outside, _) = super::files(
            &entries,
            Path::new("Music"),
            Path::new("/Users/me/Music"),
            Path::new("/Volumes/BigBoy/music"),
        );
        assert_eq!(outside[0].key, None);
        let plan = Plan::new(&outside, &HashMap::new());
        assert_eq!((plan.keys.len(), plan.unmatched.len()), (0, 1));
    }

    #[test]
    fn keeps_the_first_of_each_track() {
        let keys = strings(&["3", "1", "3", "2", "1", "4"]);
        assert_eq!(dedupe(keys), strings(&["3", "1", "2", "4"]));
        assert_eq!(dedupe(Vec::new()), Vec::<String>::new());
    }

    #[test]
    fn adds_tracks_a_hundred_at_a_time_in_order() {
        let keys: Vec<String> = (1..=250).map(|n| n.to_string()).collect();
        let sent = uris("abc123", &keys);
        assert_eq!(sent.len(), 3);
        let prefix = "server://abc123/com.plexapp.plugins.library/library/metadata/";
        let chunks: Vec<Vec<&str>> = sent
            .iter()
            .map(|uri| uri.strip_prefix(prefix).unwrap().split(',').collect())
            .collect();
        assert_eq!(
            chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            [100, 100, 50]
        );
        let all: Vec<&str> = chunks.concat();
        assert_eq!(all, keys);
        assert_eq!(uris("abc123", &strings(&["7"])), [format!("{prefix}7")]);
        assert!(uris("abc123", &[]).is_empty());
    }

    #[test]
    fn finds_the_playlist_by_title_then_by_its_file_name() {
        let playlists = [
            playlist("1", "-)", false),
            playlist("2", "Lofi Girl - beats to relax-study to", false),
            playlist("3", "chill", true),
            playlist("4", "chill", false),
            playlist("5", "chill", false),
            Playlist {
                audio: false,
                ..playlist("6", "videos", false)
            },
        ];
        // moved over from .m3u8 files, so named the way the files were
        let found = find(&playlists, ":)").unwrap();
        assert_eq!((found.playlist.key.as_str(), found.rename), ("1", true));
        let found = find(&playlists, "Lofi Girl - beats to relax/study to").unwrap();
        assert_eq!((found.playlist.key.as_str(), found.rename), ("2", true));
        // smart playlists are left alone; of two, the first, and a word about the other
        let found = find(&playlists, "chill").unwrap();
        assert_eq!(
            (found.playlist.key.as_str(), found.rename, found.others),
            ("4", false, 1)
        );
        assert_eq!(find(&playlists, "videos"), None);
        assert_eq!(find(&playlists, "new"), None);

        // the exact title wins over the file name, once renamed
        let renamed = [playlist("1", "-)", false), playlist("7", ":)", false)];
        let found = find(&renamed, ":)").unwrap();
        assert_eq!((found.playlist.key.as_str(), found.rename), ("7", false));
        // a name that's fine as a file name isn't renamed to itself
        let found = find(&renamed, "-)").unwrap();
        assert_eq!((found.playlist.key.as_str(), found.rename), ("1", false));
    }

    #[test]
    fn reads_plexs_answers() {
        let json = json!({ "MediaContainer": { "Directory": [
            { "key": "1", "type": "movie", "Location": [{ "path": "/Volumes/BigBoy/movies" }] },
            { "key": "3", "type": "artist", "refreshing": true, "scannedAt": 1795125059,
              "Location": [{ "path": "/Volumes/BigBoy/music" }, { "path": "/Music" }] },
        ]}});
        let sections = sections(&json);
        assert_eq!(sections.len(), 2);
        assert!(!sections[0].music && sections[1].music);
        assert_eq!(sections[1].locations, ["/Volumes/BigBoy/music", "/Music"]);
        assert!(sections[1].refreshing && !sections[0].refreshing);
        assert_eq!(sections[1].scanned_at, Some(1795125059));

        let json = json!({ "MediaContainer": { "size": 3, "Metadata": [
            { "ratingKey": "10622", "title": ":)", "smart": false, "playlistType": "audio" },
            { "ratingKey": 10623, "title": "Recently", "smart": true, "playlistType": "audio" },
            { "ratingKey": "1", "title": "Clips", "smart": 0, "playlistType": "video" },
        ]}});
        assert_eq!(
            playlists(&json),
            [
                playlist("10622", ":)", false),
                playlist("10623", "Recently", true),
                Playlist {
                    audio: false,
                    ..playlist("1", "Clips", false)
                },
            ]
        );
        assert!(playlists(&json!({ "MediaContainer": { "size": 0 } })).is_empty());
    }

    #[test]
    fn finds_the_music_library_holding_the_download_folder() {
        let section = |key: &str, music, locations: &[&str]| Section {
            key: key.into(),
            music,
            locations: strings(locations),
            refreshing: false,
            scanned_at: None,
        };
        let sections = [
            section("1", false, &["/Volumes/BigBoy"]),
            section("3", true, &["/Volumes/BigBoy/music"]),
            section(
                "4",
                true,
                &["/Volumes/BigBoy/music/Spotify", "/Users/me/Music"],
            ),
        ];
        let same = |location: &str| PathBuf::from(location);
        let find = |folder: &str| library_for(&sections, Path::new(folder), &same);
        let library = find("/volumes/bigboy/Music").unwrap();
        assert_eq!(
            (library.key.as_str(), library.location.as_str()),
            ("3", "/Volumes/BigBoy/music")
        );
        assert_eq!(find("/Volumes/BigBoy/music/Pixies").unwrap().key, "3");
        // the closest of the folders holding it
        assert_eq!(find("/Volumes/BigBoy/music/Spotify/x").unwrap().key, "4");
        assert_eq!(find("/Users/me/Music").unwrap().location, "/Users/me/Music");
        assert_eq!(find("/Volumes/BigBoy/movies"), None);
        assert_eq!(find("/Volumes/BigBoy"), None);

        // Plex's folder is a link to the drive: compared as this computer sees it
        let linked = [section("3", true, &["/Users/me/Music"])];
        let local = |_: &str| PathBuf::from("/Volumes/BigBoy/music");
        let library = library_for(&linked, Path::new("/Volumes/BigBoy/music"), &local).unwrap();
        assert_eq!(library.location, "/Users/me/Music");
        assert_eq!(library.local, Path::new("/Volumes/BigBoy/music"));
    }

    #[test]
    fn tells_how_the_playlist_came_out_differently() {
        let sent = strings(&["1", "2", "3"]);
        assert_eq!(mismatch(&sent, &sent), None);
        assert!(
            mismatch(&strings(&["1", "2"]), &sent)
                .unwrap()
                .contains("2 tracks, not 3")
        );
        assert!(
            mismatch(&strings(&["2", "1", "3"]), &sent)
                .unwrap()
                .contains("order")
        );
        assert!(
            mismatch(&strings(&["1", "2", "4"]), &sent)
                .unwrap()
                .contains("different")
        );
    }

    #[test]
    fn encodes_titles_for_urls() {
        assert_eq!(encode("chill"), "chill");
        assert_eq!(
            encode("a+b & c: d/e? #1"),
            "a%2Bb%20%26%20c%3A%20d%2Fe%3F%20%231"
        );
        assert_eq!(encode("ü🎧"), "%C3%BC%F0%9F%8E%A7");
        assert_eq!(encode("-._~"), "-._~");
    }

    #[test]
    fn takes_plex_addresses_with_or_without_http() {
        let url = |u: &str| Config::new(u, None).map(|c| c.url);
        assert_eq!(url(DEFAULT_URL).unwrap(), DEFAULT_URL);
        assert_eq!(
            url(" 192.168.1.20:32400/ ").unwrap(),
            "http://192.168.1.20:32400"
        );
        assert_eq!(
            url("https://plex.example.com/").unwrap(),
            "https://plex.example.com"
        );
        assert!(url("ftp://example.com").is_err());
        assert!(url("http://").is_err());
        // an empty token is no token
        let config = Config::new(DEFAULT_URL, Some(" ".parse().unwrap())).unwrap();
        assert!(config.token.is_none());
    }

    #[test]
    fn never_shows_the_token() {
        let token: Token = "s3cr3t-t0ken-value".parse().unwrap();
        let config = Config::new(DEFAULT_URL, Some(token)).unwrap();
        assert!(!format!("{config:?}").contains("s3cr3t"), "{config:?}");
    }

    #[test]
    fn reads_the_token_from_plexs_preferences() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Preferences OldestPreviousVersion="1.32.0" MachineIdentifier="abc" PlexOnlineUsername="me"
	PlexOnlineToken="AbC-123_x" PlexOnlineHome="0"/>"#;
        assert_eq!(token_in_preferences(xml).as_deref(), Some("AbC-123_x"));
        assert_eq!(
            token_in_preferences(r#"<Preferences OldPlexOnlineToken="x"/>"#),
            None
        );
        assert_eq!(
            token_in_preferences(r#"<Preferences PlexOnlineToken=""/>"#),
            None
        );
        assert_eq!(token_in_preferences("<Preferences/>"), None);
    }

    #[test]
    fn sums_up_what_was_done() {
        let synced = Synced {
            title: ":)".into(),
            matched: 186,
            total: 187,
            unmatched: strings(&["Pixies/Death to the Pixies/01 - Cecilia Ann.ogg"]),
            renamed_from: Some("-)".into()),
            ..Synced::default()
        };
        assert_eq!(
            synced.summary(),
            "Updated the playlist “:)” in Plex, and renamed it from “-)” (186/187 tracks)"
        );
        let left_out = synced.left_out().unwrap();
        assert!(left_out.starts_with("Pixies/Death to the Pixies/01 - Cecilia Ann.ogg isn't"));
        assert!(left_out.contains("same link again"), "{left_out}");

        let synced = Synced {
            title: "90s".into(),
            matched: 312,
            total: 312,
            repeats: 6,
            unchanged: true,
            ..Synced::default()
        };
        assert_eq!(
            synced.summary(),
            "The playlist “90s” in Plex is up to date (312/312 tracks; 6 repeats in it once, \
             as Plex can't hold a track twice)"
        );
        assert_eq!(synced.left_out(), None);
    }

    /// Against the Plex Media Server on this computer, with playlists of its own (deleted
    /// again afterwards, even if this fails): creating, renaming, replacing in order, and
    /// leaving alone. Run it with `cargo test --lib plex::tests::live -- --ignored`.
    #[tokio::test]
    #[ignore = "needs Plex Media Server running on this computer, with music in it"]
    async fn live() {
        let config = Config::new(DEFAULT_URL, None).unwrap();
        let (token, source) = config
            .token()
            .await
            .expect("no Plex token on this computer");
        let client = Client::new(&config.url, &token, source).unwrap();
        let server = client.server_id().await.unwrap();
        let section = client.sections().await.unwrap();
        let section = section.iter().find(|s| s.music).expect("no music library");
        let location = section.locations[0].clone();
        let audio_playlists = || async { client.playlists().await.unwrap().len() };
        let before = audio_playlists().await;

        // the first 250 tracks with a file, as Plex spells their paths
        let mut library: Vec<(String, String)> = Vec::new();
        let path = format!("/library/sections/{}/all?type=10", section.key);
        client
            .pages(&path, |items| {
                for item in items {
                    let file = item["Media"][0]["Part"][0]["file"].as_str();
                    if let (Some(key), Some(file)) = (text(&item["ratingKey"]), file) {
                        library.push((key, file.to_string()));
                    }
                }
            })
            .await
            .unwrap();
        assert!(library.len() >= 250, "needs 250 tracks to try");
        library.truncate(250);

        let name = format!("spotdl test: {}", std::process::id());
        let old_title = output::sanitize(&name);
        let titles = [name.clone(), old_title.clone()];
        let test = tokio::spawn({
            let (config, client, server) = (config.clone(), client, server);
            let (name, location) = (name.clone(), location.clone());
            async move {
                let output = Path::new(&location);
                let sync = |paths: Vec<String>| {
                    let (config, name) = (config.clone(), name.clone());
                    async move {
                        let entries: Vec<M3uEntry> = paths.iter().map(|p| entry(p)).collect();
                        super::sync(&config, output, &name, &entries, &mut |_| {}).await
                    }
                };
                let synced = |outcome: Outcome| match outcome {
                    Outcome::Synced(synced) => synced,
                    other => panic!("{other:?}"),
                };
                let paths: Vec<String> = library.iter().map(|(_, f)| f.clone()).collect();
                let keys: Vec<String> = library.iter().map(|(k, _)| k.clone()).collect();

                // moved over from an .m3u8 file, so titled the way the file was named
                let id = client
                    .fill(&server, None, &old_title, &keys[..3])
                    .await
                    .unwrap();
                let s = synced(sync(paths.clone()).await);
                assert_eq!(s.id, id, "updated in place");
                assert_eq!(s.renamed_from.as_deref(), Some(old_title.as_str()));
                assert!(!s.created && !s.unchanged && s.warnings.is_empty(), "{s:?}");
                assert_eq!((s.matched, s.total, s.repeats), (250, 250, 0));
                assert_eq!(client.items(&id).await.unwrap(), keys, "in order");

                // the other way round, with a repeat, a folder spelled in another case, and
                // a file Plex doesn't have (which has Plex look in its folder again)
                let reversed_paths: Vec<String> = paths.iter().rev().cloned().collect();
                let mut changed = reversed_paths.clone();
                changed.push(paths[7].clone());
                let relative = changed[0].strip_prefix(&location).unwrap().to_uppercase();
                changed[0] = format!("{location}{relative}");
                let dir = Path::new(&reversed_paths[0]).parent().unwrap();
                let missing = dir.join("zz - spotdl test, not a file.ogg");
                let missing = missing.to_str().unwrap().to_string();
                changed.insert(5, missing.clone());
                let s = synced(sync(changed).await);
                assert_eq!(s.id, id);
                assert!(
                    !s.created && !s.unchanged && s.renamed_from.is_none(),
                    "{s:?}"
                );
                assert_eq!((s.matched, s.total, s.repeats), (250, 251, 1));
                assert_eq!(s.unmatched.len(), 1, "{s:?}");
                assert!(s.unmatched[0].ends_with("zz - spotdl test, not a file.ogg"));
                assert!(s.warnings.is_empty(), "{:?}", s.warnings);
                let reversed: Vec<String> = keys.iter().rev().cloned().collect();
                assert_eq!(client.items(&id).await.unwrap(), reversed);

                // nothing to change
                let s = synced(sync(reversed_paths).await);
                assert!(s.unchanged && s.id == id, "{s:?}");

                // nothing matched: the playlist is left as it was
                match sync(vec![missing]).await {
                    Outcome::Failed(message) => {
                        assert!(message.contains("left as it was"), "{message}")
                    }
                    other => panic!("{other:?}"),
                }
                assert_eq!(client.items(&id).await.unwrap(), reversed);

                // a folder that isn't in any music library
                let elsewhere = std::env::temp_dir().join("spotdl-not-in-plex");
                std::fs::create_dir_all(&elsewhere).unwrap();
                let entries = [entry(elsewhere.join("a.ogg").to_str().unwrap())];
                let outcome = super::sync(&config, &elsewhere, &name, &entries, &mut |_| {}).await;
                assert!(matches!(outcome, Outcome::NoLibrary), "{outcome:?}");
                _ = std::fs::remove_dir_all(&elsewhere);
            }
        });
        let result = test.await;

        let client = Client::new(&config.url, &token, source).unwrap();
        for playlist in client.playlists().await.unwrap() {
            if titles.contains(&playlist.title) {
                let path = format!("/playlists/{}", playlist.key);
                client.call(Method::DELETE, &path).await.unwrap();
            }
        }
        assert_eq!(
            audio_playlists_after(&client).await,
            before,
            "test playlists left"
        );
        if let Err(e) = result {
            std::panic::resume_unwind(e.into_panic());
        }
    }

    async fn audio_playlists_after(client: &Client) -> usize {
        client.playlists().await.unwrap().len()
    }

    #[test]
    fn says_how_to_try_again() {
        let down = anyhow::Error::new(Unreachable("couldn't connect to Plex at x".into()));
        let message = failure("chill", &down.context("listing Plex's libraries"));
        assert!(
            message.contains("same link again once Plex is running"),
            "{message}"
        );
        let message = failure("chill", &anyhow!("Plex answered 500 Internal Server Error"));
        assert!(
            message.contains("same link again once that's fixed"),
            "{message}"
        );
    }
}
