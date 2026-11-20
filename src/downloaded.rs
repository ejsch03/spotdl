//! Telling whether something is already downloaded, without downloading anything.
//!
//! Every track's path is worked out exactly as the downloader works it out, so
//! "downloaded" means that downloading it again would do nothing. That takes each
//! track's details, which are metadata requests (never audio keys). They're kept, so
//! checking again after the download folder or format changes only looks at the disk.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use librespot_core::{Session, SpotifyId};

use crate::engine::Settings;
use crate::input::Target;
use crate::meta::{self, Meta, TrackInfo};
use crate::output::{self, PlaylistPos};

/// Albums and playlists with more tracks than this aren't checked: telling could mean
/// loading thousands of tracks' details just to grey out a button.
const MAX_TRACKS: usize = 1000;
/// Playlists looked up at the same time.
const PLAYLIST_LOOKUPS: usize = 4;
/// Loading tracks is split into about this many rounds of requests, so that how far an
/// album or playlist has got keeps changing while it goes (and stops if it's stuck).
const TRACK_ROUNDS: usize = 6;
/// The fewest tracks asked for in one request.
const MIN_CHUNK: usize = 10;
/// Track details to keep before starting over.
const CACHE_LIMIT: usize = 5000;
/// Playlists change, so their contents are looked up again after this long.
const PLAYLIST_TTL: Duration = Duration::from_secs(10 * 60);

struct PlaylistTracks {
    name: String,
    tracks: Vec<SpotifyId>,
    loaded: Instant,
}

/// What's known about whether a target is downloaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Downloaded {
    /// It's still being worked out. For an album or playlist whose tracks are known,
    /// this says how far checking them has got.
    Checking(Option<Progress>),
    Known(bool),
}

/// How far checking an album's or playlist's tracks has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Tracks whose details are in, so whether they're there is known.
    pub checked: usize,
    pub total: usize,
}

/// What has to be loaded before a target can be checked.
enum Need {
    Album(SpotifyId),
    Playlist(SpotifyId),
    Tracks(Vec<SpotifyId>),
}

#[derive(Default)]
struct Needs {
    albums: HashSet<SpotifyId>,
    playlists: HashSet<SpotifyId>,
    tracks: HashSet<SpotifyId>,
}

impl Needs {
    fn add(&mut self, need: Need) {
        match need {
            Need::Album(id) => _ = self.albums.insert(id),
            Need::Playlist(id) => _ = self.playlists.insert(id),
            Need::Tracks(ids) => self.tracks.extend(ids),
        }
    }
}

/// Whether [`Checker::check`] looks into `target` at all. It only checks songs, albums
/// and playlists, and says anything else isn't downloaded straight away.
pub fn checkable(target: &Target) -> bool {
    matches!(
        target,
        Target::Track(_) | Target::Album(_) | Target::Playlist(_)
    )
}

/// Checks search results (or any targets) against the download folder, keeping what it
/// loads for the next check.
pub struct Checker {
    meta: Meta,
    tracks: HashMap<SpotifyId, Arc<TrackInfo>>,
    playlists: HashMap<SpotifyId, PlaylistTracks>,
}

impl Checker {
    pub fn new(session: Session) -> Self {
        Self {
            meta: Meta::new(session),
            tracks: HashMap::new(),
            playlists: HashMap::new(),
        }
    }

    pub fn set_session(&mut self, session: Session) {
        self.meta.set_session(session);
    }

    /// Keeps track details loaded elsewhere (by a search, say), so they needn't be
    /// fetched again.
    pub fn add_tracks(&mut self, tracks: impl IntoIterator<Item = TrackInfo>) {
        for track in tracks {
            self.meta.remember_album(track.album.clone());
            self.tracks.insert(track.id, Arc::new(track));
        }
    }

    /// Works out whether each target is already downloaded, calling `found` with its
    /// index and the answer as soon as it's known: straight away for whatever can be
    /// told from what's loaded, then the rest as their details arrive. Meanwhile albums
    /// and playlists also report how far along they are, as their tracks are looked at.
    /// Only tracks, albums and playlists are checked. Anything else, and anything that
    /// can't be told for sure, counts as not downloaded.
    pub async fn check(
        &mut self,
        targets: &[Target],
        settings: &Settings,
        mut found: impl FnMut(usize, Downloaded),
    ) {
        if self.tracks.len() > CACHE_LIMIT {
            *self = Self::new(self.meta.session().clone());
        }
        // with --overwrite everything is downloaded again
        if settings.overwrite {
            (0..targets.len()).for_each(|i| found(i, Downloaded::Known(false)));
            return;
        }

        // Loads, in rounds, what the targets still unanswered need. Every round loads or
        // gives up on each thing it asks for, so this ends.
        let mut failed = HashSet::new();
        let mut pending: Vec<usize> = (0..targets.len()).collect();
        loop {
            let mut needs = Needs::default();
            pending.retain(|&i| match self.status(&targets[i], settings, &failed) {
                Ok(downloaded) => {
                    found(i, Downloaded::Known(downloaded));
                    false
                }
                Err(need) => {
                    if let Some(progress) = self.progress(&targets[i]) {
                        found(i, Downloaded::Checking(Some(progress)));
                    }
                    needs.add(need);
                    true
                }
            });
            if pending.is_empty() {
                return;
            }
            self.load_lists(needs.albums, needs.playlists, &mut failed)
                .await;

            // the tracks come a chunk at a time: say how far each collection has got as
            // they do, and settle any whose tracks are all in
            let ids: Vec<SpotifyId> = needs.tracks.into_iter().collect();
            let size = ids
                .len()
                .div_ceil(TRACK_ROUNDS * meta::CHUNKS_AT_ONCE)
                .max(MIN_CHUNK);
            let mut chunks = std::pin::pin!(self.meta.track_chunks(&ids, size));
            while let Some(chunk) = chunks.next().await {
                for (id, track) in chunk {
                    match track {
                        Ok(track) => _ = self.tracks.insert(id, Arc::new(track)),
                        Err(e) => {
                            log::debug!("couldn't check track {}: {e:#}", meta::link("track", id));
                            failed.insert(id);
                        }
                    }
                }
                pending.retain(|&i| {
                    let Some(progress) = self.progress(&targets[i]) else {
                        return true;
                    };
                    if progress.checked == progress.total
                        && let Ok(downloaded) = self.status(&targets[i], settings, &failed)
                    {
                        found(i, Downloaded::Known(downloaded));
                        return false;
                    }
                    found(i, Downloaded::Checking(Some(progress)));
                    true
                });
            }
        }
    }

    /// How far checking an album or playlist has got, once its tracks are known.
    fn progress(&self, target: &Target) -> Option<Progress> {
        match *target {
            Target::Album(id) => progress(&self.meta.cached_album(id)?.tracks, &self.tracks),
            Target::Playlist(id) => progress(&self.playlists.get(&id)?.tracks, &self.tracks),
            _ => None,
        }
    }

    fn status(
        &self,
        target: &Target,
        settings: &Settings,
        failed: &HashSet<SpotifyId>,
    ) -> Result<bool, Need> {
        match *target {
            Target::Track(id) => collection(&[id], None, &self.tracks, settings, failed),
            Target::Album(id) if failed.contains(&id) => Ok(false),
            Target::Album(id) => {
                let album = self.meta.cached_album(id).ok_or(Need::Album(id))?;
                collection(&album.tracks, None, &self.tracks, settings, failed)
            }
            Target::Playlist(id) if failed.contains(&id) => Ok(false),
            Target::Playlist(id) => match self.playlists.get(&id) {
                Some(p) if p.loaded.elapsed() < PLAYLIST_TTL => {
                    playlist(p, &self.tracks, settings, failed)
                }
                _ => Err(Need::Playlist(id)),
            },
            // an artist's downloads depend on all their releases; too much to load here
            // (see `checkable`)
            _ => Ok(false),
        }
    }

    /// Loads albums' and playlists' track lists.
    async fn load_lists(
        &mut self,
        albums: HashSet<SpotifyId>,
        playlists: HashSet<SpotifyId>,
        failed: &mut HashSet<SpotifyId>,
    ) {
        let albums: Vec<SpotifyId> = albums.into_iter().collect();
        for (id, album) in albums.iter().zip(self.meta.albums(&albums).await) {
            if let Err(e) = album {
                log::debug!("couldn't check album {}: {e:#}", meta::link("album", *id));
                failed.insert(*id);
            }
        }

        let meta = &self.meta;
        let playlists: Vec<_> = stream::iter(playlists)
            .map(|id| async move { (id, meta.playlist(id).await) })
            .buffer_unordered(PLAYLIST_LOOKUPS)
            .collect()
            .await;
        for (id, playlist) in playlists {
            match playlist {
                Ok((name, items)) => {
                    let tracks = items.iter().filter_map(meta::track_id).collect();
                    let loaded = Instant::now();
                    self.playlists.insert(
                        id,
                        PlaylistTracks {
                            name,
                            tracks,
                            loaded,
                        },
                    );
                }
                Err(e) => {
                    log::debug!(
                        "couldn't check playlist {}: {e:#}",
                        meta::link("playlist", id)
                    );
                    failed.insert(id);
                }
            }
        }
    }
}

/// How many of a collection's tracks (`ids`) have their details loaded, out of how many
/// it has. A song, being one track, has nothing to count.
fn progress(ids: &[SpotifyId], tracks: &HashMap<SpotifyId, Arc<TrackInfo>>) -> Option<Progress> {
    (ids.len() > 1).then(|| Progress {
        checked: ids.iter().filter(|id| tracks.contains_key(id)).count(),
        total: ids.len(),
    })
}

/// Like [`collection`], for a playlist. When downloading one always writes its `.m3u8`
/// file (by default, or with `--m3u`), that has to be there too; without it the playlist
/// isn't downloaded, which settles it without loading any track details. When playlists
/// go to Plex, the file may rightly be missing (Plex has the playlist instead), so the
/// tracks are all that count.
fn playlist(
    p: &PlaylistTracks,
    tracks: &HashMap<SpotifyId, Arc<TrackInfo>>,
    settings: &Settings,
    failed: &HashSet<SpotifyId>,
) -> Result<bool, Need> {
    if settings.always_writes_m3u() && !settings.playlist_file(&p.name).is_file() {
        return Ok(false);
    }
    collection(&p.tracks, Some(&p.name), tracks, settings, failed)
}

/// Whether every track in `ids` that would be downloaded is already there, as far as
/// the loaded `tracks` tell; otherwise which tracks to load next. Tracks that can't be
/// downloaded (not available in this country, say) don't count, but at least one file
/// has to be there. A `playlist` name means the tracks are a playlist's, in order.
fn collection(
    ids: &[SpotifyId],
    playlist: Option<&str>,
    tracks: &HashMap<SpotifyId, Arc<TrackInfo>>,
    settings: &Settings,
    failed: &HashSet<SpotifyId>,
) -> Result<bool, Need> {
    if ids.is_empty() || ids.len() > MAX_TRACKS {
        return Ok(false);
    }
    let mut present = false;
    let mut unknown = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if failed.contains(id) {
            return Ok(false);
        }
        let Some(track) = tracks.get(id) else {
            unknown.push(*id);
            continue;
        };
        let pos = playlist.map(|name| PlaylistPos {
            name,
            index: i + 1,
            len: ids.len(),
        });
        if output::already_downloaded(&settings.output.join(settings.relative_path(track, pos))) {
            present = true;
        } else if track.playable.is_ok() {
            return Ok(false); // downloading would fetch this one
        }
    }
    if unknown.is_empty() {
        return Ok(present);
    }
    // Most things aren't downloaded, and the first missing track settles that, so load
    // just one track until a file turns up. Then all the rest.
    if !present {
        unknown.truncate(1);
    }
    Err(Need::Tracks(unknown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Bitrate, Format};
    use crate::engine::M3u;
    use crate::meta::{AlbumInfo, Playable};
    use crate::output::Template;
    use std::path::Path;

    fn id(n: usize) -> SpotifyId {
        SpotifyId::from_base62(&format!("{n:0>22}")).unwrap()
    }

    fn settings(output: &Path) -> Settings {
        Settings {
            output: output.to_path_buf(),
            format: Format::Ogg,
            bitrate: Bitrate::B320,
            template: Template::parse("{album}/{track} - {title}").unwrap(),
            playlist_template: Template::parse("{playlist}/{playlist_index} {title}").unwrap(),
            releases: vec![],
            m3u: M3u::Auto,
            plex: None,
            overwrite: false,
            dry_run: false,
            delay: Duration::ZERO,
            retries: 1,
        }
    }

    /// Three tracks of one album; the second can't be downloaded.
    fn album_tracks() -> HashMap<SpotifyId, Arc<TrackInfo>> {
        let album = Arc::new(AlbumInfo {
            id: id(100),
            name: "Modal Soul".into(),
            kind: "Album",
            artists: vec!["Nujabes".into()],
            date: None,
            label: None,
            copyright: None,
            discs: vec![(1, 3)],
            tracks: (1..=3).map(id).collect(),
            cover: None,
            thumbnail: None,
        });
        (1..=3)
            .map(|n| {
                let track = TrackInfo {
                    id: id(n),
                    title: format!("Song {n}"),
                    artists: vec!["Nujabes".into()],
                    artist_ids: vec![],
                    album: album.clone(),
                    number: n as u32,
                    disc: 1,
                    duration_ms: 1000,
                    isrc: None,
                    playable: if n == 2 {
                        Err("not available in your country".into())
                    } else {
                        Ok(Playable {
                            id: id(n),
                            files: HashMap::new(),
                        })
                    },
                };
                (id(n), Arc::new(track))
            })
            .collect()
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn check(
        ids: &[SpotifyId],
        playlist: Option<&str>,
        tracks: &HashMap<SpotifyId, Arc<TrackInfo>>,
        settings: &Settings,
    ) -> Result<bool, Vec<SpotifyId>> {
        collection(ids, playlist, tracks, settings, &HashSet::new()).map_err(|need| match need {
            Need::Tracks(ids) => ids,
            _ => unreachable!(),
        })
    }

    #[test]
    fn needs_every_downloadable_track_in_place() {
        let dir = std::env::temp_dir().join(format!("spotdl-downloaded-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let tracks = album_tracks();
        let ids: Vec<_> = (1..=3).map(id).collect();
        let s = settings(&dir);

        assert_eq!(
            check(&ids, None, &tracks, &s),
            Ok(false),
            "nothing there yet"
        );
        touch(&dir.join("Modal Soul/01 - Song 1.ogg"));
        assert_eq!(
            check(&ids, None, &tracks, &s),
            Ok(false),
            "song 3 is missing"
        );
        touch(&dir.join("Modal Soul/03 - Song 3.ogg"));
        // song 2 can't be downloaded, so it doesn't count
        assert_eq!(check(&ids, None, &tracks, &s), Ok(true));
        assert_eq!(
            check(&ids[1..2], None, &tracks, &s),
            Ok(false),
            "no file at all"
        );

        // another format or folder is another file
        let mut mp3 = s.clone();
        mp3.format = Format::Mp3;
        assert_eq!(check(&ids, None, &tracks, &mp3), Ok(false));
        assert_eq!(
            check(&ids, None, &tracks, &settings(&dir.join("x"))),
            Ok(false)
        );

        // a playlist's tracks go where the playlist template says
        assert_eq!(check(&ids, Some("Chill"), &tracks, &s), Ok(false));
        touch(&dir.join("Chill/01 Song 1.ogg"));
        touch(&dir.join("Chill/03 Song 3.ogg"));
        assert_eq!(check(&ids, Some("Chill"), &tracks, &s), Ok(true));

        // When a playlist goes to Plex, or with --no-m3u, its tracks are all that count...
        let chill = PlaylistTracks {
            name: "Chill: Vol. 2".into(),
            tracks: ids.clone(),
            loaded: Instant::now(),
        };
        let none = HashSet::new();
        let mut plex = s.clone();
        plex.plex = Some(crate::plex::Config::new(crate::plex::DEFAULT_URL, None).unwrap());
        assert!(matches!(playlist(&chill, &tracks, &plex, &none), Ok(false)));
        touch(&dir.join("Chill - Vol. 2/01 Song 1.ogg"));
        touch(&dir.join("Chill - Vol. 2/03 Song 3.ogg"));
        assert!(matches!(playlist(&chill, &tracks, &plex, &none), Ok(true)));
        let mut never = s.clone();
        never.m3u = M3u::Never;
        assert!(matches!(playlist(&chill, &tracks, &never, &none), Ok(true)));
        // ...but by default, and with --m3u even with Plex, its .m3u8 has to be there too
        let mut always = plex.clone();
        always.m3u = M3u::Always;
        for settings in [&s, &always] {
            assert!(matches!(
                playlist(&chill, &tracks, settings, &none),
                Ok(false)
            ));
            // (and nothing has to be loaded to tell)
            assert!(matches!(
                playlist(&chill, &HashMap::new(), settings, &none),
                Ok(false)
            ));
        }
        touch(&dir.join("Chill - Vol. 2.m3u8"));
        for settings in [&s, &always] {
            assert!(matches!(
                playlist(&chill, &tracks, settings, &none),
                Ok(true)
            ));
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_one_track_until_a_file_turns_up() {
        let dir = std::env::temp_dir().join(format!("spotdl-loading-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let s = settings(&dir);
        let mut tracks = album_tracks();
        let ids: Vec<_> = (1..=20).map(id).collect();

        // nothing loaded: just the first, which usually settles it
        assert_eq!(check(&ids, None, &HashMap::new(), &s), Err(vec![id(1)]));
        // the first is there: all the rest
        touch(&dir.join("Modal Soul/01 - Song 1.ogg"));
        tracks.retain(|k, _| *k == id(1));
        assert_eq!(
            check(&ids, None, &tracks, &s),
            Err((2..=20).map(id).collect())
        );
        // with one of the twenty looked at
        assert_eq!(
            progress(&ids, &tracks),
            Some(Progress {
                checked: 1,
                total: 20
            })
        );
        // (a song has nothing to count)
        assert_eq!(progress(&ids[..1], &tracks), None);
        // a track that couldn't be loaded can't be told about
        let failed = HashSet::from([id(5)]);
        assert!(matches!(
            collection(&ids, None, &tracks, &s, &failed),
            Ok(false)
        ));
        // a loaded track that's missing settles it, whatever isn't loaded yet
        let all = album_tracks();
        tracks.insert(id(3), all[&id(3)].clone());
        assert_eq!(check(&ids, None, &tracks, &s), Ok(false));

        // huge playlists aren't checked
        let huge: Vec<_> = (1..=MAX_TRACKS + 1).map(id).collect();
        assert!(matches!(
            collection(&huge, None, &tracks, &s, &HashSet::new()),
            Ok(false)
        ));
        _ = std::fs::remove_dir_all(&dir);
    }
}
