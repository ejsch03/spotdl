//! Turns what the user typed into download targets.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use librespot_core::SpotifyId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Track(SpotifyId),
    Album(SpotifyId),
    Playlist(SpotifyId),
    Artist(SpotifyId),
    /// A bare ID whose kind (track, album, ...) has to be looked up.
    Id(SpotifyId),
    LikedSongs,
    SavedAlbums,
    /// A shortened share link (spotify.link) that has to be followed first.
    ShortLink(String),
    Search(String),
}

/// One thing to download, with the text it came from (for messages).
#[derive(Debug, Clone)]
pub struct Input {
    pub text: String,
    pub target: Target,
}

/// Collects inputs from the command line and the optional input file, failing up front
/// (before logging in) if any of them can't be understood.
pub fn collect(args: &[String], file: Option<&Path>) -> Result<Vec<Input>> {
    let mut texts = group_words(args);

    if let Some(path) = file {
        let content = if path == Path::new("-") {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("reading inputs from stdin")?;
            s
        } else {
            std::fs::read_to_string(path)
                .with_context(|| format!("couldn't read input file {}", path.display()))?
        };
        texts.extend(
            content
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from),
        );
    }

    let mut inputs = Vec::with_capacity(texts.len());
    let mut errors = Vec::new();
    for text in texts {
        match parse(&text) {
            Ok(target) => inputs.push(Input { text, target }),
            Err(e) => errors.push(format!("  {text}: {e}")),
        }
    }
    if !errors.is_empty() {
        bail!("couldn't understand some inputs:\n{}", errors.join("\n"));
    }
    Ok(inputs)
}

/// `spotdl never gonna give you up` arrives as five arguments; join runs of loose words
/// into one search query. Links, IDs and quoted multi-word arguments stay separate, and
/// a run made only of keywords (`liked saved-albums`) is kept apart too.
fn group_words(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut run: Vec<&str> = Vec::new();

    fn flush(run: &mut Vec<&str>, out: &mut Vec<String>) {
        if run.iter().all(|w| keyword(w).is_some()) {
            out.extend(run.iter().map(|w| w.to_string()));
        } else {
            out.push(run.join(" "));
        }
        run.clear();
    }

    for arg in args.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
        let loose_word = !arg.contains(char::is_whitespace)
            && matches!(
                parse(arg),
                Ok(Target::Search(_) | Target::LikedSongs | Target::SavedAlbums)
            );
        if loose_word {
            run.push(arg);
        } else {
            if !run.is_empty() {
                flush(&mut run, &mut out);
            }
            out.push(arg.to_string());
        }
    }
    if !run.is_empty() {
        flush(&mut run, &mut out);
    }
    out
}

fn keyword(s: &str) -> Option<Target> {
    match s.to_ascii_lowercase().as_str() {
        "liked" | "liked-songs" | "liked_songs" => Some(Target::LikedSongs),
        "saved-albums" | "saved_albums" => Some(Target::SavedAlbums),
        _ => None,
    }
}

pub fn parse(input: &str) -> Result<Target> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty input");
    }
    if let Some(target) = keyword(s) {
        return Ok(target);
    }

    let lower = s.to_ascii_lowercase();
    let without_scheme = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    let looks_like_link = without_scheme.is_some()
        || ["open.spotify.com/", "play.spotify.com/", "spotify.link/"]
            .iter()
            .any(|p| lower.starts_with(p));
    if looks_like_link {
        return parse_link(s);
    }

    if lower.starts_with("spotify:") {
        return parse_uri(s);
    }

    // shorthand `album:<id>` etc.
    if let Some((kind, id)) = s.split_once(':')
        && looks_like_id(id)
        && let Some(target) = by_kind(&kind.to_ascii_lowercase(), id)
    {
        return target;
    }

    if looks_like_id(s) {
        return Ok(Target::Id(parse_id(s)?));
    }

    Ok(Target::Search(s.to_string()))
}

fn parse_link(s: &str) -> Result<Target> {
    let rest = s.split_once("://").map_or(s, |(_, rest)| rest);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = path.split(['?', '#']).next().unwrap_or_default();

    match host {
        "open.spotify.com" | "play.spotify.com" => {
            let segments: Vec<&str> = path
                .split('/')
                .filter(|seg| !seg.is_empty())
                .skip_while(|seg| seg.starts_with("intl-") || *seg == "embed")
                .collect();
            match segments.as_slice() {
                ["collection"] | ["collection", "tracks"] => Ok(Target::LikedSongs),
                ["collection", "albums"] => Ok(Target::SavedAlbums),
                ["user", _, "playlist", id] => Ok(Target::Playlist(parse_id(id)?)),
                [kind, id] => by_kind(kind, id)
                    .unwrap_or_else(|| Err(anyhow!("unsupported Spotify link type `{kind}`"))),
                _ => bail!("unrecognised Spotify link"),
            }
        }
        "spotify.link" | "spotify.app.link" => Ok(Target::ShortLink(format!("https://{rest}"))),
        _ => bail!("not a Spotify link"),
    }
}

fn parse_uri(s: &str) -> Result<Target> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [_, kind, id] => by_kind(&kind.to_ascii_lowercase(), id)
            .unwrap_or_else(|| Err(anyhow!("unsupported Spotify URI type `{kind}`"))),
        [_, "user", _, "playlist", id] => Ok(Target::Playlist(parse_id(id)?)),
        [_, "user", _, "collection"] | [_, "collection"] => Ok(Target::LikedSongs),
        _ => bail!("unrecognised Spotify URI"),
    }
}

fn by_kind(kind: &str, raw_id: &str) -> Option<Result<Target>> {
    let make = match kind {
        "track" => Target::Track,
        "album" => Target::Album,
        "playlist" => Target::Playlist,
        "artist" => Target::Artist,
        "episode" | "show" => return Some(Err(anyhow!("podcasts aren't supported"))),
        _ => return None,
    };
    Some(parse_id(raw_id).map(make))
}

fn looks_like_id(s: &str) -> bool {
    s.len() == 22 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn parse_id(s: &str) -> Result<SpotifyId> {
    if !looks_like_id(s) {
        bail!("`{s}` is not a valid Spotify ID");
    }
    SpotifyId::from_base62(s).map_err(|_| anyhow!("`{s}` is not a valid Spotify ID"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "3uNaLrScYulpXLRmQquP9I";

    fn sid() -> SpotifyId {
        SpotifyId::from_base62(ID).unwrap()
    }

    #[test]
    fn links() {
        let cases = [
            (
                format!("https://open.spotify.com/album/{ID}"),
                Target::Album(sid()),
            ),
            (
                format!("https://open.spotify.com/track/{ID}?si=abc123"),
                Target::Track(sid()),
            ),
            (
                format!("open.spotify.com/playlist/{ID}"),
                Target::Playlist(sid()),
            ),
            (
                format!("https://open.spotify.com/intl-de/artist/{ID}"),
                Target::Artist(sid()),
            ),
            (
                format!("https://open.spotify.com/embed/track/{ID}"),
                Target::Track(sid()),
            ),
            (
                format!("https://open.spotify.com/user/someone/playlist/{ID}"),
                Target::Playlist(sid()),
            ),
            (
                "https://open.spotify.com/collection/tracks".into(),
                Target::LikedSongs,
            ),
            (
                "https://open.spotify.com/collection/albums".into(),
                Target::SavedAlbums,
            ),
            (
                "https://spotify.link/AbCdEf".into(),
                Target::ShortLink("https://spotify.link/AbCdEf".into()),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse(&input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn uris_and_ids() {
        assert_eq!(
            parse(&format!("spotify:album:{ID}")).unwrap(),
            Target::Album(sid())
        );
        assert_eq!(
            parse(&format!("spotify:user:x:playlist:{ID}")).unwrap(),
            Target::Playlist(sid())
        );
        assert_eq!(parse(&format!("track:{ID}")).unwrap(), Target::Track(sid()));
        assert_eq!(parse(ID).unwrap(), Target::Id(sid()));
        assert_eq!(parse("spotify:collection").unwrap(), Target::LikedSongs);
        assert_eq!(parse("LIKED").unwrap(), Target::LikedSongs);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("https://example.com/album/x").is_err());
        assert!(parse("https://open.spotify.com/album/tooshort").is_err());
        assert!(parse(&format!("https://open.spotify.com/episode/{ID}")).is_err());
        assert!(parse("spotify:album:nope").is_err());
    }

    #[test]
    fn search_text() {
        assert_eq!(
            parse("daft punk").unwrap(),
            Target::Search("daft punk".into())
        );
        // a colon in a query isn't mistaken for a shorthand
        assert_eq!(
            parse("artist: yes").unwrap(),
            Target::Search("artist: yes".into())
        );
    }

    #[test]
    fn groups_loose_words() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            group_words(&args(&["never", "gonna", "give", "you", "up"])),
            vec!["never gonna give you up"]
        );
        assert_eq!(
            group_words(&args(&["one", ID, "two", "words"])),
            vec!["one".to_string(), ID.to_string(), "two words".to_string()]
        );
        assert_eq!(
            group_words(&args(&["liked", "saved-albums"])),
            vec!["liked", "saved-albums"]
        );
        assert_eq!(
            group_words(&args(&["i", "liked", "it"])),
            vec!["i liked it"]
        );
        assert_eq!(
            group_words(&args(&["song a", "song b"])),
            vec!["song a", "song b"]
        );
    }
}
