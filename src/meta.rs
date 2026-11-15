//! Spotify metadata, fetched through the logged-in session (no developer app needed).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, stream};
use librespot_core::{FileId, Session, SpotifyId, SpotifyUri, date::Date, error::ErrorKind};
use librespot_metadata::{
    Album, Artist, Metadata, Playlist, Track, album::AlbumType, audio::AudioFileFormat,
};
use librespot_protocol::playlist4_external::{ListAttributes, MetaItem, SelectedListContent};
use protobuf::{CodedInputStream, CodedOutputStream, Message, rt::WireType};

/// How many metadata requests to keep in flight at once.
const CONCURRENCY: usize = 8;
const PLAYLIST_PAGE: usize = 100;
/// Playlists per page of the user's playlist list (what the Spotify app asks for too).
const ROOTLIST_PAGE: usize = 120;

#[derive(Debug, Clone, Copy)]
pub struct ReleaseDate {
    pub year: i32,
    pub month: Option<u8>,
    pub day: Option<u8>,
}

impl fmt::Display for ReleaseDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}", self.year)?;
        if let Some(m) = self.month {
            write!(f, "-{m:02}")?;
            if let Some(d) = self.day {
                write!(f, "-{d:02}")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AlbumInfo {
    pub id: SpotifyId,
    pub name: String,
    pub kind: &'static str,
    pub artists: Vec<String>,
    pub date: Option<ReleaseDate>,
    pub label: Option<String>,
    pub copyright: Option<String>,
    /// (disc number, number of tracks on it)
    pub discs: Vec<(u32, u32)>,
    pub tracks: Vec<SpotifyId>,
    /// The largest cover image, for tagging.
    pub cover: Option<FileId>,
    /// A small cover image (at least `THUMBNAIL_MIN_PX` wide when there is one), for
    /// showing in lists.
    pub thumbnail: Option<FileId>,
}

/// The smallest cover worth showing as a thumbnail: sharp at 48 px on a 2x screen.
pub const THUMBNAIL_MIN_PX: i32 = 96;

impl AlbumInfo {
    pub fn disc_count(&self) -> u32 {
        self.discs.len().max(1) as u32
    }

    pub fn tracks_on_disc(&self, disc: u32) -> u32 {
        self.discs
            .iter()
            .find(|(n, _)| *n == disc)
            .map_or(self.tracks.len() as u32, |(_, count)| *count)
    }

    pub fn year(&self) -> Option<i32> {
        self.date.map(|d| d.year)
    }
}

/// The version of a track that can actually be downloaded: either the track itself or
/// one of its regional alternatives.
#[derive(Debug, Clone)]
pub struct Playable {
    pub id: SpotifyId,
    pub files: HashMap<AudioFileFormat, FileId>,
}

#[derive(Debug)]
pub struct TrackInfo {
    pub id: SpotifyId,
    pub title: String,
    pub artists: Vec<String>,
    pub artist_ids: Vec<SpotifyId>,
    pub album: Arc<AlbumInfo>,
    pub number: u32,
    pub disc: u32,
    pub duration_ms: u32,
    pub isrc: Option<String>,
    pub playable: Result<Playable, String>,
}

impl TrackInfo {
    pub fn artist(&self) -> String {
        self.artists.join(", ")
    }

    pub fn display(&self) -> String {
        format!("{} — {}", self.title, self.artist())
    }

    pub fn link(&self) -> String {
        link("track", self.id)
    }
}

pub fn link(kind: &str, id: SpotifyId) -> String {
    format!(
        "https://open.spotify.com/{kind}/{}",
        id.to_base62().unwrap_or_default()
    )
}

/// A playlist in the user's library: one they made or follow.
pub struct LibraryPlaylist {
    pub id: SpotifyId,
    pub name: String,
    /// The owner's username (an ID, not a display name).
    pub owner: String,
    pub tracks: usize,
    /// A picture for showing in lists, if the playlist has one of its own.
    pub image: Option<String>,
}

pub struct ArtistInfo {
    pub name: String,
    pub albums: Vec<SpotifyId>,
    pub singles: Vec<SpotifyId>,
    pub compilations: Vec<SpotifyId>,
    pub appears_on: Vec<SpotifyId>,
    pub top_tracks: Vec<SpotifyId>,
}

pub struct Meta {
    session: Session,
    albums: Mutex<HashMap<SpotifyId, Arc<AlbumInfo>>>,
}

impl Meta {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            albums: Default::default(),
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn set_session(&mut self, session: Session) {
        self.session = session;
    }

    /// The album, if it was loaded before.
    pub fn cached_album(&self, id: SpotifyId) -> Option<Arc<AlbumInfo>> {
        self.albums.lock().unwrap().get(&id).cloned()
    }

    /// Keeps an album loaded elsewhere, so it needn't be fetched again.
    pub fn remember_album(&self, album: Arc<AlbumInfo>) {
        self.albums.lock().unwrap().entry(album.id).or_insert(album);
    }

    pub async fn album(&self, id: SpotifyId) -> Result<Arc<AlbumInfo>> {
        if let Some(album) = self.cached_album(id) {
            return Ok(album);
        }

        let uri = SpotifyUri::Album { id };
        // parse the raw message ourselves too: librespot's `Date` fills in a missing
        // month/day with 1, and we want to know whether the release date is exact
        let bytes = throttled(|| Album::request(&self.session, &uri))
            .await
            .map_err(missing("album"))?;
        let msg = <Album as Metadata>::Message::parse_from_bytes(&bytes)?;
        let album = Album::parse(&msg, &uri)?;
        let date = msg
            .date
            .as_ref()
            .filter(|d| d.has_year())
            .map(|d| ReleaseDate {
                year: d.year(),
                month: d.has_month().then(|| d.month() as u8),
                day: (d.has_month() && d.has_day()).then(|| d.day() as u8),
            });

        let images: Vec<_> = album
            .covers
            .iter()
            .chain(album.cover_group.iter())
            .collect();
        let largest = images.iter().max_by_key(|img| img.width);
        let info = Arc::new(AlbumInfo {
            id,
            kind: match album.album_type {
                AlbumType::SINGLE => "Single",
                AlbumType::COMPILATION => "Compilation",
                AlbumType::EP => "EP",
                _ => "Album",
            },
            artists: album.artists.iter().map(|a| a.name.clone()).collect(),
            date,
            label: Some(album.label.clone()).filter(|l| !l.is_empty()),
            copyright: album
                .copyrights
                .first()
                .map(|c| c.text.clone())
                .filter(|c| !c.is_empty()),
            discs: album
                .discs
                .0
                .iter()
                .map(|d| (d.number.max(1) as u32, d.tracks.0.len() as u32))
                .collect(),
            tracks: album.tracks().filter_map(track_id).collect(),
            cover: largest.map(|img| img.id),
            thumbnail: images
                .iter()
                .filter(|img| img.width >= THUMBNAIL_MIN_PX)
                .min_by_key(|img| img.width)
                .or(largest)
                .map(|img| img.id),
            name: album.name,
        });
        self.albums.lock().unwrap().insert(id, info.clone());
        Ok(info)
    }

    pub async fn track(&self, id: SpotifyId) -> Result<TrackInfo> {
        let uri = SpotifyUri::Track { id };
        let track = throttled(|| Track::get(&self.session, &uri))
            .await
            .map_err(missing("track"))?;
        let album_id = match &track.album.id {
            SpotifyUri::Album { id } => *id,
            _ => bail!("track has no album"),
        };
        let album = self.album(album_id).await?;
        let playable = self.playable(&track).await;

        Ok(TrackInfo {
            id,
            artists: track.artists.iter().map(|a| a.name.clone()).collect(),
            artist_ids: track
                .artists
                .iter()
                .filter_map(|a| artist_id(&a.id))
                .collect(),
            number: track.number.max(0) as u32,
            disc: track.disc_number.max(1) as u32,
            duration_ms: track.duration.max(0) as u32,
            isrc: track
                .external_ids
                .0
                .iter()
                .find(|e| e.external_type.eq_ignore_ascii_case("isrc"))
                .map(|e| e.id.clone()),
            title: track.name,
            album,
            playable,
        })
    }

    /// Fetches many tracks concurrently, keeping their order. `progress` is called after
    /// each one finishes.
    pub async fn tracks(
        &self,
        ids: &[SpotifyId],
        mut progress: impl FnMut(usize),
    ) -> Vec<(SpotifyId, Result<TrackInfo>)> {
        let mut done = 0;
        stream::iter(ids.iter().copied())
            .map(|id| async move { (id, self.track(id).await) })
            .buffered(CONCURRENCY)
            .inspect(|_| {
                done += 1;
                progress(done);
            })
            .collect()
            .await
    }

    pub async fn albums(&self, ids: &[SpotifyId]) -> Vec<Result<Arc<AlbumInfo>>> {
        stream::iter(ids.iter().copied())
            .map(|id| self.album(id))
            .buffered(CONCURRENCY)
            .collect()
            .await
    }

    async fn playable(&self, track: &Track) -> Result<Playable, String> {
        let reason = match self.availability(track) {
            Ok(()) if has_audio(&track.files.0) => return Ok(playable(track)),
            Ok(()) => "no downloadable audio".to_string(),
            Err(reason) => reason,
        };
        // Spotify links to the same recording released for other regions
        for alt in track.alternatives.0.iter() {
            if let Ok(alt) = throttled(|| Track::get(&self.session, alt)).await
                && self.availability(&alt).is_ok()
                && has_audio(&alt.files.0)
            {
                return Ok(playable(&alt));
            }
        }
        Err(reason)
    }

    /// Mirrors librespot's own availability rules, with readable reasons.
    fn availability(&self, track: &Track) -> Result<(), String> {
        let now = Date::now_utc();
        if now < track.earliest_live_timestamp
            || (!track.availability.is_empty()
                && !track.availability.iter().any(|a| now >= a.start))
        {
            return Err("not released yet".into());
        }

        let user = self.session.user_data();
        let catalogue = user
            .attributes
            .get("catalogue")
            .map_or("premium", String::as_str);
        let unavailable = || Err(format!("not available in your country ({})", user.country));
        for restriction in track
            .restrictions
            .iter()
            .filter(|r| r.catalogue_strs.iter().any(|c| c == catalogue))
        {
            if let Some(allowed) = &restriction.countries_allowed {
                return if allowed.contains(&user.country) {
                    Ok(())
                } else {
                    unavailable()
                };
            }
            if let Some(forbidden) = &restriction.countries_forbidden {
                return if forbidden.contains(&user.country) {
                    unavailable()
                } else {
                    Ok(())
                };
            }
        }
        Ok(())
    }

    /// Returns the playlist's name and its items (which may include podcast episodes
    /// and local files, not just tracks).
    pub async fn playlist(&self, id: SpotifyId) -> Result<(String, Vec<SpotifyUri>)> {
        let uri = SpotifyUri::Playlist { id, user: None };
        let first = throttled(|| Playlist::get(&self.session, &uri))
            .await
            .map_err(missing("playlist"))?;
        let name = first.name().to_string();
        let total = first.length.max(0) as usize;
        let mut items: Vec<SpotifyUri> = first.contents.items.0.into_iter().map(|i| i.id).collect();

        // very large playlists can come back truncated; fetch the rest page by page
        while items.len() < total {
            let endpoint = format!(
                "/playlist/v2/playlist/{}?from={}&length={PLAYLIST_PAGE}",
                id.to_base62()?,
                items.len()
            );
            let bytes = throttled(|| {
                self.session
                    .spclient()
                    .request(&http::Method::GET, &endpoint, None, None)
            })
            .await?;
            let page = Playlist::parse(&SelectedListContent::parse_from_bytes(&bytes)?, &uri)?;
            if page.contents.position as usize != items.len() || page.contents.items.is_empty() {
                log::warn!(
                    "playlist paging stopped at {} of {total} items",
                    items.len()
                );
                break;
            }
            items.extend(page.contents.items.0.into_iter().map(|i| i.id));
        }
        Ok((name, items))
    }

    /// The playlists in the user's library (made or followed), in the order the Spotify
    /// app lists them, with folders flattened. One request per 120 playlists.
    pub async fn library_playlists(&self) -> Result<Vec<LibraryPlaylist>> {
        let mut playlists = Vec::new();
        let mut from = 0;
        loop {
            let bytes = throttled(|| {
                self.session
                    .spclient()
                    .get_rootlist(from, Some(ROOTLIST_PAGE))
            })
            .await?;
            let page = SelectedListContent::parse_from_bytes(&bytes)?;
            let Some(contents) = page.contents.as_ref() else {
                break;
            };
            // the details (name, length, owner) come in a list alongside the items
            let details =
                (contents.meta_items.len() == contents.items.len()).then_some(&contents.meta_items);
            for (i, item) in contents.items.iter().enumerate() {
                // folders show up as start and end markers between the playlists
                let Some(id) = item
                    .uri()
                    .strip_prefix("spotify:playlist:")
                    .and_then(|id| SpotifyId::from_base62(id).ok())
                else {
                    continue;
                };
                playlists.push(match details {
                    Some(details) => library_playlist(id, &details[i]),
                    // not seen so far; look the playlist up itself rather than guess
                    None => {
                        let (name, items) = self.playlist(id).await?;
                        LibraryPlaylist {
                            id,
                            name,
                            owner: String::new(),
                            tracks: items.len(),
                            image: None,
                        }
                    }
                });
            }
            from += contents.items.len();
            if contents.items.is_empty() || from >= page.length().max(0) as usize {
                break;
            }
        }
        Ok(playlists)
    }

    pub async fn artist(&self, id: SpotifyId) -> Result<ArtistInfo> {
        let uri = SpotifyUri::Artist { id };
        let artist = throttled(|| Artist::get(&self.session, &uri))
            .await
            .map_err(missing("artist"))?;
        fn ids<'a>(uris: impl Iterator<Item = &'a SpotifyUri>) -> Vec<SpotifyId> {
            uris.filter_map(album_id).collect()
        }
        Ok(ArtistInfo {
            albums: ids(artist.albums_current()),
            singles: ids(artist.singles_current()),
            compilations: ids(artist.compilations_current()),
            appears_on: ids(artist.appears_on_albums_current()),
            top_tracks: artist
                .top_tracks
                .for_country(&self.session.country())
                .0
                .iter()
                .filter_map(track_id)
                .collect(),
            name: artist.name.clone(),
        })
    }

    /// Track IDs from a search, best match first.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SpotifyId>> {
        let uri = format!("spotify:search:{}", encode_query(query));
        let context = throttled(|| self.session.spclient().get_context(&uri))
            .await
            .context("searching Spotify")?;
        Ok(context
            .pages
            .iter()
            .flat_map(|page| page.tracks.iter())
            .filter_map(|t| SpotifyUri::from_uri(t.uri()).ok())
            .filter_map(|uri| track_id(&uri))
            .take(limit)
            .collect())
    }

    /// Items in the user's library (`set` = "collection" holds both Liked Songs and saved
    /// albums), most recently added first.
    pub async fn library(&self) -> Result<Vec<SpotifyUri>> {
        let mut items: Vec<(i32, SpotifyUri)> = Vec::new();
        let mut token = String::new();
        let mut headers = http::HeaderMap::new();
        let proto = http::HeaderValue::from_static("application/vnd.collection-v2.spotify.proto");
        headers.insert(http::header::CONTENT_TYPE, proto.clone());
        headers.insert(http::header::ACCEPT, proto);

        loop {
            let mut body = Vec::new();
            {
                let mut out = CodedOutputStream::vec(&mut body);
                out.write_string(1, &self.session.username())?;
                out.write_string(2, "collection")?;
                if !token.is_empty() {
                    out.write_string(3, &token)?;
                }
                out.write_int32(4, 300)?;
                out.flush()?;
            }
            let response = throttled(|| {
                self.session.spclient().request(
                    &http::Method::POST,
                    "/collection/v2/paging",
                    Some(headers.clone()),
                    Some(&body),
                )
            })
            .await
            .context("loading your library")?;
            let (page, next) = parse_library_page(&response)?;
            items.extend(page);
            if next.is_empty() || next == token {
                break;
            }
            token = next;
        }

        items.sort_by_key(|(added, _)| std::cmp::Reverse(*added));
        Ok(items.into_iter().map(|(_, uri)| uri).collect())
    }

    /// Works out what a bare ID refers to.
    pub async fn kind_of(&self, id: SpotifyId) -> Result<&'static str> {
        let session = &self.session;
        let (track, playlist, artist) = (
            SpotifyUri::Track { id },
            SpotifyUri::Playlist { id, user: None },
            SpotifyUri::Artist { id },
        );
        if throttled(|| Track::get(session, &track)).await.is_ok() {
            return Ok("track");
        }
        if self.album(id).await.is_ok() {
            return Ok("album");
        }
        if throttled(|| Playlist::get(session, &playlist))
            .await
            .is_ok()
        {
            return Ok("playlist");
        }
        if throttled(|| Artist::get(session, &artist)).await.is_ok() {
            return Ok("artist");
        }
        Err(anyhow!("no track, album, playlist or artist has this ID"))
    }
}

/// librespot throttles its own HTTP requests (300 per 30 s, refilling 10 per second) and
/// fails instead of waiting when that budget runs out, which big playlists and
/// discographies easily hit. Wait for a free slot instead.
pub async fn throttled<T, F, Fut>(mut request: F) -> Result<T, librespot_core::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, librespot_core::Error>>,
{
    for _ in 0..240 {
        match request().await {
            Err(e) if e.kind == ErrorKind::ResourceExhausted => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            result => return result,
        }
    }
    request().await
}

fn library_playlist(id: SpotifyId, details: &MetaItem) -> LibraryPlaylist {
    LibraryPlaylist {
        id,
        name: details.attributes.name().to_string(),
        owner: details.owner_username().to_string(),
        tracks: details.length().max(0) as usize,
        image: playlist_image(&details.attributes),
    }
}

/// The playlist's own picture: preferably the 300 px one (sharp at 44 px on any
/// screen), else the uploaded picture's ID. Playlists showing a mosaic of their albums
/// have neither.
fn playlist_image(attributes: &ListAttributes) -> Option<String> {
    let sized = |target: &str| {
        attributes
            .picture_size
            .iter()
            .find(|p| p.target_name() == target && !p.url().is_empty())
            .map(|p| p.url().to_string())
    };
    sized("default").or_else(|| sized("large")).or_else(|| {
        let id = <[u8; 20]>::try_from(attributes.picture()).ok()?;
        Some(format!(
            "https://i.scdn.co/image/{}",
            FileId(id).to_base16().ok()?
        ))
    })
}

/// Spotify answers "no such item" in a couple of different ways; say it plainly.
fn missing(kind: &'static str) -> impl FnOnce(librespot_core::Error) -> anyhow::Error {
    move |e| match e.kind {
        ErrorKind::NotFound | ErrorKind::FailedPrecondition => {
            anyhow!("this {kind} doesn't exist on Spotify (check the link or ID)")
        }
        _ => anyhow!(e),
    }
}

/// Parses a `collection.proto.v2.PageResponse`; those messages aren't compiled into
/// librespot-protocol, and we only need three fields.
fn parse_library_page(bytes: &[u8]) -> Result<(Vec<(i32, SpotifyUri)>, String)> {
    let mut items = Vec::new();
    let mut next = String::new();
    let mut input = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        match tag >> 3 {
            1 => {
                let item = input.read_bytes()?;
                let (mut uri, mut added, mut removed) = (String::new(), 0, false);
                let mut fields = CodedInputStream::from_bytes(&item);
                while let Some(tag) = fields.read_raw_tag_or_eof()? {
                    match tag >> 3 {
                        1 => uri = fields.read_string()?,
                        2 => added = fields.read_int32()?,
                        3 => removed = fields.read_bool()?,
                        _ => skip(&mut fields, tag)?,
                    }
                }
                if !removed && let Ok(uri) = SpotifyUri::from_uri(&uri) {
                    items.push((added, uri));
                }
            }
            2 => next = input.read_string()?,
            _ => skip(&mut input, tag)?,
        }
    }
    Ok((items, next))
}

fn skip(input: &mut CodedInputStream, tag: u32) -> Result<()> {
    let wire_type = WireType::new(tag & 7).ok_or_else(|| anyhow!("malformed library response"))?;
    Ok(input.skip_field(wire_type)?)
}

/// `spotify:search:` URIs use `+` for spaces; everything else non-alphanumeric is escaped.
pub(crate) fn encode_query(query: &str) -> String {
    let mut out = String::new();
    for word in query.split_whitespace() {
        if !out.is_empty() {
            out.push('+');
        }
        for b in word.bytes() {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn has_audio(files: &HashMap<AudioFileFormat, FileId>) -> bool {
    crate::audio::OGG_FORMATS
        .iter()
        .any(|f| files.contains_key(f))
}

fn playable(track: &Track) -> Playable {
    Playable {
        id: track_id(&track.id).expect("track metadata has a track ID"),
        files: track.files.0.clone(),
    }
}

pub fn track_id(uri: &SpotifyUri) -> Option<SpotifyId> {
    match uri {
        SpotifyUri::Track { id } => Some(*id),
        _ => None,
    }
}

pub fn album_id(uri: &SpotifyUri) -> Option<SpotifyId> {
    match uri {
        SpotifyUri::Album { id } => Some(*id),
        _ => None,
    }
}

fn artist_id(uri: &SpotifyUri) -> Option<SpotifyId> {
    match uri {
        SpotifyUri::Artist { id } => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_date_precision() {
        let d = |year, month, day| ReleaseDate { year, month, day }.to_string();
        assert_eq!(d(2004, None, None), "2004");
        assert_eq!(d(2004, Some(6), None), "2004-06");
        assert_eq!(d(2004, Some(6), Some(23)), "2004-06-23");
    }

    #[test]
    fn query_encoding() {
        assert_eq!(encode_query("  daft punk  "), "daft+punk");
        assert_eq!(encode_query("AC/DC back in black"), "AC%2FDC+back+in+black");
        assert_eq!(encode_query("Beyoncé"), "Beyonc%C3%A9");
    }

    #[test]
    fn library_page_roundtrip() {
        let mut item = Vec::new();
        {
            let mut out = CodedOutputStream::vec(&mut item);
            out.write_string(1, "spotify:track:3uNaLrScYulpXLRmQquP9I")
                .unwrap();
            out.write_int32(2, 42).unwrap();
            out.write_string(4, "ignored").unwrap();
            out.flush().unwrap();
        }
        let mut page = Vec::new();
        {
            let mut out = CodedOutputStream::vec(&mut page);
            out.write_bytes(1, &item).unwrap();
            out.write_string(2, "next-token").unwrap();
            out.write_string(3, "sync").unwrap();
            out.flush().unwrap();
        }
        let (items, next) = parse_library_page(&page).unwrap();
        assert_eq!(next, "next-token");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, 42);
    }
}
