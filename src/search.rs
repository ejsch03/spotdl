//! Searching Spotify for songs, albums, artists and playlists.
//!
//! With a Spotify developer app set up (see [`WebApi`]), everything is searched through
//! Spotify's Web API. Without one, or if that fails, songs are found with the same search
//! `spotdl search` uses, and their albums and artists are listed alongside. (The Web API
//! can't be used with the login session itself: Spotify permanently rate-limits the
//! client ID that session belongs to.)

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use librespot_core::{FileId, SpotifyId};
use serde_json::Value;

use crate::engine::count;
use crate::input::Target;
use crate::meta::{self, AlbumInfo, Meta, THUMBNAIL_MIN_PX, TrackInfo};

/// Where a developer app's client ID and secret are kept, inside the config directory.
pub const DEVELOPER_APP_FILE: &str = "developer-app.json";

/// Songs to look up per search without the Web API. Albums and artists come from these,
/// so a few more than fit on screen gives them something to draw from.
const SONGS: usize = 20;
/// Web API results per kind. Development-mode apps may be capped at 10.
const PER_KIND: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Track,
    Album,
    Artist,
    Playlist,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Track, Kind::Album, Kind::Artist, Kind::Playlist];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Track => "Song",
            Kind::Album => "Album",
            Kind::Artist => "Artist",
            Kind::Playlist => "Playlist",
        }
    }

    fn api_name(self) -> &'static str {
        match self {
            Kind::Track => "track",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist => "playlist",
        }
    }
}

/// One search result.
#[derive(Debug, Clone)]
pub struct Hit {
    pub kind: Kind,
    pub id: SpotifyId,
    /// What it is, for people: Song, Album, Single, EP, Artist, Playlist...
    pub label: String,
    pub title: String,
    /// Who and what: artists, album and year, owner, genres...
    pub subtitle: String,
    /// A short fact on the side: duration, track count, followers...
    pub detail: String,
    /// A small picture: cover art, the artist's photo...
    pub image: Option<String>,
}

impl Hit {
    pub fn target(&self) -> Target {
        match self.kind {
            Kind::Track => Target::Track(self.id),
            Kind::Album => Target::Album(self.id),
            Kind::Artist => Target::Artist(self.id),
            Kind::Playlist => Target::Playlist(self.id),
        }
    }

    pub fn link(&self) -> String {
        meta::link(self.kind.api_name(), self.id)
    }

    fn track(t: &TrackInfo) -> Self {
        Hit {
            kind: Kind::Track,
            id: t.id,
            label: Kind::Track.label().into(),
            title: t.title.clone(),
            subtitle: join([t.artist(), t.album.name.clone(), album_year(&t.album)]),
            detail: duration_text(t.duration_ms),
            image: cover_url(t.album.thumbnail),
        }
    }

    fn album(a: &AlbumInfo) -> Self {
        Hit {
            kind: Kind::Album,
            id: a.id,
            label: a.kind.into(),
            title: a.name.clone(),
            subtitle: join([a.artists.join(", "), album_year(a)]),
            detail: count(a.tracks.len(), "track"),
            image: cover_url(a.thumbnail),
        }
    }
}

pub struct Results {
    /// Songs first, then albums, artists and playlists.
    pub hits: Vec<Hit>,
    /// Set when the Web API failed and only songs (and their albums and artists) were
    /// searched; says why.
    pub note: Option<String>,
    /// The songs' details when they were searched by song (they're loaded for that), so
    /// they needn't be fetched again.
    pub tracks: Vec<TrackInfo>,
}

/// Searches with the developer app if there is one, else (or if it fails) by songs.
pub async fn search(meta: &Meta, web: Option<&WebApi>, query: &str) -> Result<Results> {
    let mut note = None;
    if let Some(web) = web {
        match web.search(query, &meta.session().country()).await {
            Ok(hits) => {
                return Ok(Results {
                    hits,
                    note: None,
                    tracks: Vec::new(),
                });
            }
            Err(e) => {
                log::warn!("Web API search failed: {e:#}");
                note = Some(format!("Searched songs only: {e:#}"));
            }
        }
    }
    let tracks = song_search(meta, query).await?;
    Ok(Results {
        hits: hits_from_songs(&tracks),
        note,
        tracks,
    })
}

/// Songs matching `query`, best first.
async fn song_search(meta: &Meta, query: &str) -> Result<Vec<TrackInfo>> {
    let ids = meta
        .search(query, SONGS)
        .await
        .context("searching Spotify")?;
    Ok(meta
        .tracks(&ids, |_| {})
        .await
        .into_iter()
        .filter_map(|(_, track)| track.ok())
        .collect())
}

/// The songs, then the albums they're on, then their artists (those on the most
/// matching songs first).
fn hits_from_songs(tracks: &[TrackInfo]) -> Vec<Hit> {
    let mut hits: Vec<Hit> = tracks.iter().map(Hit::track).collect();

    let mut albums = HashSet::new();
    for t in tracks {
        if albums.insert(t.album.id) {
            hits.push(Hit::album(&t.album));
        }
    }

    // (id, name, matching songs), in order of first appearance
    let mut artists: Vec<(SpotifyId, &str, usize)> = Vec::new();
    let mut index: HashMap<SpotifyId, usize> = HashMap::new();
    for t in tracks {
        // names and IDs line up unless Spotify sent an odd artist entry; skip those
        if t.artists.len() != t.artist_ids.len() {
            continue;
        }
        for (id, name) in t.artist_ids.iter().zip(&t.artists) {
            match index.get(id) {
                Some(&i) => artists[i].2 += 1,
                None => {
                    index.insert(*id, artists.len());
                    artists.push((*id, name, 1));
                }
            }
        }
    }
    artists.sort_by_key(|&(_, _, songs)| std::cmp::Reverse(songs));
    hits.extend(artists.into_iter().map(|(id, name, songs)| Hit {
        kind: Kind::Artist,
        id,
        label: Kind::Artist.label().into(),
        title: name.to_string(),
        subtitle: String::new(),
        detail: format!("on {}", count(songs, "matching song")),
        // only the artist's own metadata has pictures, and it isn't loaded here
        image: None,
    }));
    hits
}

/// The playlists in the user's library (made or followed), in the order the Spotify app
/// lists them.
pub async fn library_playlists(meta: &Meta) -> Result<Vec<Hit>> {
    let me = meta.session().username();
    Ok(meta
        .library_playlists()
        .await?
        .into_iter()
        .map(|p| Hit {
            kind: Kind::Playlist,
            id: p.id,
            label: Kind::Playlist.label().into(),
            title: p.name,
            subtitle: match p.owner.as_str() {
                owner if owner == me => "made by you",
                "spotify" => "by Spotify",
                _ => "followed",
            }
            .into(),
            detail: count(p.tracks, "track"),
            image: p.image,
        })
        .collect())
}

// ---------------------------------------------------------------------------------------
// The Web API, through the user's own developer app

/// A Spotify developer app (from developer.spotify.com), used only to search. It gets
/// its own token with the client-credentials flow, which needs no user login, and talks
/// to the Web API with a plain HTTP client: bounded by a timeout, never retrying.
pub struct WebApi {
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    /// The current token and when to stop using it.
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl WebApi {
    /// The app with this client ID and secret, as copied from the developer dashboard
    /// (surrounding spaces are fine). Doesn't check them with Spotify; see [`Self::check`].
    pub fn new(client_id: &str, client_secret: &str) -> Result<Self> {
        let (client_id, client_secret) = (client_id.trim(), client_secret.trim());
        for (value, name) in [(client_id, "client ID"), (client_secret, "client secret")] {
            if value.is_empty() {
                bail!("the {name} is missing");
            }
            if !value.chars().all(|c| c.is_ascii_alphanumeric()) {
                bail!("the {name} should be letters and digits only; check what was copied");
            }
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            http,
            token: tokio::sync::Mutex::new(None),
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Whether `SPOTDL_CLIENT_ID` and `SPOTDL_CLIENT_SECRET` are set; they take the place
    /// of a saved app.
    pub fn in_environment() -> bool {
        Self::from_environment().is_some()
    }

    fn from_environment() -> Option<(String, String)> {
        let env = |name| {
            std::env::var(name)
                .ok()
                .filter(|v: &String| !v.trim().is_empty())
        };
        Some((env("SPOTDL_CLIENT_ID")?, env("SPOTDL_CLIENT_SECRET")?))
    }

    /// Makes sure Spotify accepts the app, by getting it a token.
    pub async fn check(&self) -> Result<()> {
        self.token().await.map(drop)
    }

    /// Saves the app in `config_dir`, for next time (readable by its owner only).
    pub fn save(&self, config_dir: &Path) -> Result<()> {
        crate::auth::create_private_dir(config_dir)?;
        let json = serde_json::json!({
            "client_id": self.client_id,
            "client_secret": self.client_secret,
        });
        let text = serde_json::to_string_pretty(&json)? + "\n";
        crate::auth::write_private(&config_dir.join(DEVELOPER_APP_FILE), text.as_bytes())
    }

    /// Whether an app is saved in `config_dir`.
    pub fn is_saved(config_dir: &Path) -> bool {
        config_dir.join(DEVELOPER_APP_FILE).exists()
    }

    /// Forgets the saved app. Returns whether there was one.
    pub fn remove(config_dir: &Path) -> Result<bool> {
        let path = config_dir.join(DEVELOPER_APP_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// The app from `SPOTDL_CLIENT_ID` and `SPOTDL_CLIENT_SECRET`, or else from
    /// `developer-app.json` in `config_dir`. `None` if neither is set up.
    pub fn load(config_dir: &Path) -> Result<Option<Self>> {
        let (client_id, client_secret) = match Self::from_environment() {
            Some(app) => app,
            None => {
                let path = config_dir.join(DEVELOPER_APP_FILE);
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => {
                        return Err(e).with_context(|| format!("reading {}", path.display()));
                    }
                };
                // it holds a secret
                crate::auth::restrict_permissions(&path);
                let json: Value = serde_json::from_str(&text)
                    .with_context(|| format!("{} isn't valid JSON", path.display()))?;
                let field = |key: &str| {
                    Some(text_at(&json, key).trim())
                        .filter(|value| !value.is_empty())
                        .map(String::from)
                        .ok_or_else(|| anyhow!("{} has no \"{key}\"", path.display()))
                };
                (field("client_id")?, field("client_secret")?)
            }
        };
        Self::new(&client_id, &client_secret).map(Some)
    }

    async fn token(&self) -> Result<String> {
        let mut cached = self.token.lock().await;
        if let Some((token, until)) = &*cached
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let response = self
            .http
            .post("https://accounts.spotify.com/api/token")
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body("grant_type=client_credentials")
            .send()
            .await
            .context("asking Spotify for a Web API token")?;
        let status = response.status();
        let body: Value = serde_json::from_slice(&response.bytes().await?).unwrap_or_default();
        if !status.is_success() {
            let reason = text_at(&body, "error_description");
            let reason = if reason.is_empty() {
                status.to_string()
            } else {
                reason.to_string()
            };
            bail!(
                "Spotify didn't accept the developer app ({reason}); check its client ID and secret"
            );
        }
        let token = text_at(&body, "access_token").to_string();
        if token.is_empty() {
            bail!("Spotify sent no Web API token");
        }
        let lifetime = body["expires_in"].as_u64().unwrap_or(3600);
        // renew a minute early, so a token never expires mid-request
        let until = Instant::now() + Duration::from_secs(lifetime.saturating_sub(60));
        *cached = Some((token.clone(), until));
        Ok(token)
    }

    /// One request, no retries. `market` (a country code) picks the versions of tracks
    /// playable there.
    async fn search(&self, query: &str, market: &str) -> Result<Vec<Hit>> {
        let token = self.token().await?;
        let types: Vec<_> = Kind::ALL.iter().map(|k| k.api_name()).collect();
        let mut url = format!(
            "https://api.spotify.com/v1/search?q={}&type={}&limit={PER_KIND}",
            meta::encode_query(query),
            types.join(",")
        );
        if !market.is_empty() {
            url.push_str(&format!("&market={market}"));
        }
        let response = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("searching Spotify's Web API")?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(|secs| format!(" (try again in {secs}s)"))
                .unwrap_or_default();
            bail!("Spotify is rate-limiting your developer app{wait}");
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            *self.token.lock().await = None; // get a fresh one next time
            bail!("Spotify didn't accept the Web API token");
        }
        if !status.is_success() {
            bail!("Spotify's Web API answered {status}");
        }
        parse(&response.bytes().await?)
    }
}

fn parse(body: &[u8]) -> Result<Vec<Hit>> {
    let json: Value = serde_json::from_slice(body).context("reading the search results")?;
    let mut hits = Vec::new();
    for kind in Kind::ALL {
        let items = json
            .pointer(&format!("/{}s/items", kind.api_name()))
            .and_then(Value::as_array);
        // playlists in particular come back with `null` entries now and then
        hits.extend(
            items
                .into_iter()
                .flatten()
                .filter_map(|item| hit(kind, item)),
        );
    }
    Ok(hits)
}

fn hit(kind: Kind, item: &Value) -> Option<Hit> {
    let id = SpotifyId::from_base62(item.get("id")?.as_str()?).ok()?;
    let mut label = kind.label();
    let mut images = item.get("images");
    let (subtitle, detail) = match kind {
        Kind::Track => {
            let album = item.get("album").unwrap_or(&Value::Null);
            images = album.get("images");
            (
                join([
                    artist_names(item),
                    text_at(album, "name").into(),
                    json_year(album).into(),
                ]),
                item.get("duration_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| duration_text(ms as u32))
                    .unwrap_or_default(),
            )
        }
        Kind::Album => {
            label = release_kind(text_at(item, "album_type"));
            (
                join([artist_names(item), json_year(item).into()]),
                number(item, "/total_tracks")
                    .map(|n| count(n as usize, "track"))
                    .unwrap_or_default(),
            )
        }
        Kind::Artist => {
            let genres: Vec<&str> = item
                .get("genres")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(3)
                .collect();
            (
                genres.join(", "),
                number(item, "/followers/total")
                    .map(|n| format!("{} followers", compact(n)))
                    .unwrap_or_default(),
            )
        }
        Kind::Playlist => {
            let owner = item
                .pointer("/owner/display_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (
                if owner.is_empty() {
                    String::new()
                } else {
                    format!("by {owner}")
                },
                // newer API versions call the track count `items`
                number(item, "/tracks/total")
                    .or_else(|| number(item, "/items/total"))
                    .map(|n| count(n as usize, "track"))
                    .unwrap_or_default(),
            )
        }
    };
    Some(Hit {
        kind,
        id,
        label: label.into(),
        title: text_at(item, "name").to_string(),
        subtitle,
        detail,
        image: thumbnail_url(images),
    })
}

/// From a Web API `images` list: the smallest image at least `THUMBNAIL_MIN_PX` wide, or
/// else the largest. Images without a size (playlist mosaics) count as large.
fn thumbnail_url(images: Option<&Value>) -> Option<String> {
    let mut sized: Vec<(u64, &str)> = images?
        .as_array()?
        .iter()
        .filter_map(|img| {
            let width = img.get("width").and_then(Value::as_u64);
            Some((width.unwrap_or(u64::MAX), img.get("url")?.as_str()?))
        })
        .collect();
    sized.sort_by_key(|(width, _)| *width);
    let enough = THUMBNAIL_MIN_PX as u64;
    sized
        .iter()
        .find(|(width, _)| *width >= enough)
        .or(sized.last())
        .map(|(_, url)| url.to_string())
}

/// Spotify's image CDN serves cover art by file ID.
fn cover_url(id: Option<FileId>) -> Option<String> {
    Some(format!("https://i.scdn.co/image/{}", id?.to_base16().ok()?))
}

fn text_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn number(v: &Value, pointer: &str) -> Option<u64> {
    v.pointer(pointer).and_then(Value::as_u64)
}

fn artist_names(v: &Value) -> String {
    let names: Vec<&str> = v
        .get("artists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| text_at(a, "name"))
        .filter(|n| !n.is_empty())
        .collect();
    names.join(", ")
}

/// The year of a `release_date` like `2004-06-23` or `2004`.
fn json_year(album: &Value) -> &str {
    text_at(album, "release_date").get(..4).unwrap_or_default()
}

fn album_year(album: &AlbumInfo) -> String {
    album.year().map(|y| y.to_string()).unwrap_or_default()
}

fn release_kind(album_type: &str) -> &'static str {
    match album_type {
        "single" => "Single",
        "compilation" => "Compilation",
        _ => "Album",
    }
}

/// Joins the non-empty parts with ` · `.
fn join<const N: usize>(parts: [String; N]) -> String {
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn duration_text(ms: u32) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// `1234567` → `1.2M`
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3).replace(".0K", "K"),
        _ => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::ReleaseDate;
    use std::sync::Arc;

    const ID: &str = "3uNaLrScYulpXLRmQquP9I";

    fn id(n: u8) -> SpotifyId {
        // IDs are 128-bit numbers written as 22 base62 digits; small ones are easy to make
        SpotifyId::from_base62(&format!("{n:0>22}")).unwrap()
    }

    fn album(n: u8, name: &str, tracks: usize) -> Arc<AlbumInfo> {
        Arc::new(AlbumInfo {
            id: id(n),
            name: name.into(),
            kind: "Album",
            artists: vec!["Nujabes".into()],
            date: Some(ReleaseDate {
                year: 2005,
                month: None,
                day: None,
            }),
            label: None,
            copyright: None,
            discs: vec![],
            tracks: vec![id(0); tracks],
            cover: None,
            thumbnail: None,
        })
    }

    fn track(n: u8, title: &str, album: &Arc<AlbumInfo>, artists: &[(u8, &str)]) -> TrackInfo {
        TrackInfo {
            id: id(n),
            title: title.into(),
            artists: artists.iter().map(|(_, name)| name.to_string()).collect(),
            artist_ids: artists.iter().map(|(n, _)| id(*n)).collect(),
            album: album.clone(),
            number: 1,
            disc: 1,
            duration_ms: 245_000,
            isrc: None,
            playable: Err("test".into()),
        }
    }

    type Row = (String, String, String, String);

    fn rows(hits: Vec<Hit>) -> Vec<Row> {
        hits.into_iter()
            .map(|h| (h.label, h.title, h.subtitle, h.detail))
            .collect()
    }

    fn row(label: &str, title: &str, subtitle: &str, detail: &str) -> Row {
        (label.into(), title.into(), subtitle.into(), detail.into())
    }

    #[test]
    fn gathers_albums_and_artists_from_songs() {
        let modal = album(10, "Modal Soul", 14);
        let meta = album(11, "Metaphorical Music", 16);
        let tracks = [
            track(1, "Feather", &modal, &[(20, "Nujabes"), (21, "Cise Starr")]),
            track(2, "Luv(sic)", &meta, &[(20, "Nujabes"), (22, "Shing02")]),
            track(3, "Reflection Eternal", &modal, &[(20, "Nujabes")]),
        ];
        assert_eq!(
            rows(hits_from_songs(&tracks)),
            [
                row(
                    "Song",
                    "Feather",
                    "Nujabes, Cise Starr · Modal Soul · 2005",
                    "4:05"
                ),
                row(
                    "Song",
                    "Luv(sic)",
                    "Nujabes, Shing02 · Metaphorical Music · 2005",
                    "4:05"
                ),
                row(
                    "Song",
                    "Reflection Eternal",
                    "Nujabes · Modal Soul · 2005",
                    "4:05"
                ),
                row("Album", "Modal Soul", "Nujabes · 2005", "14 tracks"),
                row("Album", "Metaphorical Music", "Nujabes · 2005", "16 tracks"),
                row("Artist", "Nujabes", "", "on 3 matching songs"),
                row("Artist", "Cise Starr", "", "on 1 matching song"),
                row("Artist", "Shing02", "", "on 1 matching song"),
            ]
        );
    }

    #[test]
    fn hits_point_at_what_they_show() {
        let a = album(10, "Modal Soul", 14);
        let found = hits_from_songs(&[track(1, "Feather", &a, &[(20, "Nujabes")])]);
        assert_eq!(found[0].target(), Target::Track(id(1)));
        assert_eq!(found[1].target(), Target::Album(id(10)));
        assert_eq!(found[2].target(), Target::Artist(id(20)));
        assert!(found[1].link().contains("/album/"));
        assert!(hits_from_songs(&[]).is_empty());

        let mut with_cover = (*a).clone();
        with_cover.thumbnail = Some(FileId([0xab; 20]));
        let found = hits_from_songs(&[track(1, "Feather", &Arc::new(with_cover), &[])]);
        let url = format!("https://i.scdn.co/image/{}", "ab".repeat(20));
        assert_eq!(found[0].image.as_deref(), Some(url.as_str()));
        assert_eq!(found[1].image.as_deref(), Some(url.as_str()));
    }

    #[test]
    fn parses_web_api_results_and_tolerates_gaps() {
        let body = serde_json::json!({
            "tracks": { "items": [{
                "id": ID, "name": "battlecry", "duration_ms": 245000,
                "artists": [{ "name": "Nujabes" }, { "name": "Shing02" }],
                "album": { "name": "departure", "release_date": "2004-06-23", "images": [
                    { "url": "https://i.scdn.co/image/640", "width": 640 },
                    { "url": "https://i.scdn.co/image/300", "width": 300 },
                    { "url": "https://i.scdn.co/image/64", "width": 64 }
                ]}
            }]},
            "albums": { "items": [{
                "id": ID, "name": "departure", "album_type": "compilation",
                "total_tracks": 17, "release_date": "2004",
                "artists": [{ "name": "Nujabes" }]
            }]},
            "artists": { "items": [{
                "id": ID, "name": "Nujabes", "genres": ["jazz rap", "lo-fi"],
                "followers": { "total": 1_234_567 },
                "images": [{ "url": "https://i.scdn.co/image/small", "width": 64 }]
            }]},
            "playlists": { "items": [
                null,
                { "id": ID, "name": "Chill", "owner": { "display_name": "me" },
                  "tracks": { "total": 1 },
                  "images": [{ "url": "https://mosaic.scdn.co/640/abc", "width": null }] },
                { "id": "not-an-id", "name": "broken" },
                { "id": ID, "name": "Newer", "items": { "total": 3 } }
            ]}
        })
        .to_string();

        let hits = parse(body.as_bytes()).unwrap();
        assert_eq!(hits[3].target(), Target::Playlist(hits[3].id));
        let images: Vec<_> = hits.iter().map(|h| h.image.as_deref()).collect();
        assert_eq!(
            images,
            [
                // smallest that's big enough
                Some("https://i.scdn.co/image/300"),
                None,
                // too small, but all there is
                Some("https://i.scdn.co/image/small"),
                // no size given
                Some("https://mosaic.scdn.co/640/abc"),
                None,
            ]
        );
        assert_eq!(
            rows(hits),
            [
                row(
                    "Song",
                    "battlecry",
                    "Nujabes, Shing02 · departure · 2004",
                    "4:05"
                ),
                row("Compilation", "departure", "Nujabes · 2004", "17 tracks"),
                row("Artist", "Nujabes", "jazz rap, lo-fi", "1.2M followers"),
                row("Playlist", "Chill", "by me", "1 track"),
                row("Playlist", "Newer", "", "3 tracks"),
            ]
        );
    }

    #[test]
    fn odd_web_api_responses_give_no_hits() {
        assert!(parse(b"{}").unwrap().is_empty());
        assert!(
            parse(br#"{"tracks": {"items": "nope"}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn loads_the_developer_app_file() {
        let dir = std::env::temp_dir().join(format!("spotdl-app-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // (the environment variables would win; they aren't set in tests)
        assert!(WebApi::load(&dir).unwrap().is_none());

        let file = dir.join(DEVELOPER_APP_FILE);
        std::fs::write(&file, r#"{"client_id": " abc ", "client_secret": "xyz"}"#).unwrap();
        let app = WebApi::load(&dir).unwrap().unwrap();
        assert_eq!(
            (app.client_id.as_str(), app.client_secret.as_str()),
            ("abc", "xyz")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "readable by others");
        }

        std::fs::write(&file, r#"{"client_id": "abc"}"#).unwrap();
        assert!(WebApi::load(&dir).is_err());
        _ = std::fs::remove_dir_all(&dir);
    }

    /// What the desktop app's settings do: save an app, load it back, remove it.
    #[test]
    fn saves_and_removes_the_developer_app() {
        let dir = std::env::temp_dir().join(format!("spotdl-save-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        assert!(!WebApi::is_saved(&dir));

        let app = WebApi::new(" 0123abcd\n", "\tsecret99 ").unwrap();
        app.save(&dir).unwrap();
        assert!(WebApi::is_saved(&dir));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = dir.join(DEVELOPER_APP_FILE);
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "readable by others");
        }
        let loaded = WebApi::load(&dir).unwrap().unwrap();
        assert_eq!(
            (loaded.client_id(), loaded.client_secret.as_str()),
            ("0123abcd", "secret99")
        );
        // saving again replaces it
        WebApi::new("ffff", "eeee").unwrap().save(&dir).unwrap();
        assert_eq!(WebApi::load(&dir).unwrap().unwrap().client_id(), "ffff");

        assert!(WebApi::remove(&dir).unwrap());
        assert!(!WebApi::remove(&dir).unwrap());
        assert!(WebApi::load(&dir).unwrap().is_none());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_what_isnt_a_client_id() {
        assert!(WebApi::new("", "secret").is_err());
        assert!(WebApi::new("abc", "  ").is_err());
        let e = WebApi::new("Client ID: abc", "secret").err().unwrap();
        assert!(e.to_string().contains("letters and digits"), "{e}");
    }

    #[test]
    fn compacts_numbers() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_000), "1K");
        assert_eq!(compact(12_345), "12.3K");
        assert_eq!(compact(2_000_000), "2M");
    }
}
