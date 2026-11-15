//! Fetches a track's original audio file from Spotify's CDN and decrypts it.
//!
//! Getting a track takes two separate requests: the decryption key, which Spotify
//! rate-limits per account, and the encrypted file itself from the CDN, which it
//! doesn't. They're kept apart so a failed download can be retried with the key already
//! in hand instead of asking Spotify for it again.

use std::fmt;

use aes::cipher::{KeyIvInit, StreamCipher};
use anyhow::{Context, anyhow};
use librespot_core::audio_key::{AudioKey, AudioKeyError};
use librespot_core::{FileId, Session, cdn_url::CdnUrl};
use librespot_metadata::audio::AudioFileFormat;

use crate::cli::Bitrate;
use crate::meta::Playable;

/// Spotify prepends its own header page to Ogg files; the real stream starts here.
const OGG_HEADER_END: usize = 0xa7;
/// Offset of the loudness-normalisation values inside that header.
const NORMALISATION_OFFSET: usize = 144;

/// Spotify encrypts audio files with AES-128 in CTR mode under this fixed IV. Same
/// scheme as librespot's `AudioDecrypt`, which the tests check `decrypt` against.
type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;
const AUDIO_AESIV: [u8; 16] = [
    0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63, 0x0d, 0x93,
];

pub const OGG_FORMATS: [AudioFileFormat; 3] = [
    AudioFileFormat::OGG_VORBIS_320,
    AudioFileFormat::OGG_VORBIS_160,
    AudioFileFormat::OGG_VORBIS_96,
];

/// The audio file picked for a track.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub file: FileId,
    pub kbps: u32,
}

pub struct Audio {
    /// A standard Ogg Vorbis stream.
    pub ogg: Vec<u8>,
    pub gain: Option<Gain>,
}

/// Spotify's loudness-normalisation data, written out as ReplayGain tags.
#[derive(Debug, Clone, Copy)]
pub struct Gain {
    pub track_gain_db: f32,
    pub track_peak: f32,
    pub album_gain_db: f32,
    pub album_peak: f32,
}

/// Why Spotify didn't hand over a decryption key.
#[derive(Debug)]
pub enum KeyError {
    /// Spotify answered with a refusal: it's rate-limiting this account.
    Refused,
    /// No answer in time even though the connection is up. Spotify may throttle by
    /// staying silent, so this is treated like a refusal.
    NoAnswer,
    /// The connection to Spotify broke; nothing to do with rate limits.
    Connection(anyhow::Error),
}

impl KeyError {
    /// Whether this looks like Spotify pushing back on the account (back off, and stop
    /// the run if it keeps happening) rather than a connection problem.
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, Self::Refused | Self::NoAnswer)
    }

    fn classify(e: librespot_core::Error) -> Self {
        match e.error.downcast_ref::<AudioKeyError>() {
            Some(AudioKeyError::AesKey) => Self::Refused,
            Some(AudioKeyError::Timeout) => Self::NoAnswer,
            _ => Self::Connection(anyhow!(e)),
        }
    }
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused => f.write_str("Spotify refused the audio key (rate limited)"),
            Self::NoAnswer => f.write_str("Spotify didn't answer the audio key request"),
            Self::Connection(e) => write!(f, "couldn't request the audio key: {e:#}"),
        }
    }
}

/// Why fetching the file failed once the key was in hand.
#[derive(Debug)]
pub enum FetchError {
    /// A network or CDN hiccup; retry soon with the same key.
    Network(anyhow::Error),
    /// The file didn't decrypt to valid audio, so the key is probably wrong.
    BadKey,
}

impl FetchError {
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Network(e) => e,
            Self::BadKey => anyhow!("{self}"),
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(e) => write!(f, "{e:#}"),
            Self::BadKey => f.write_str("the downloaded audio didn't decrypt correctly"),
        }
    }
}

/// Picks the file closest to the requested quality, falling back to the others.
pub fn choose(track: &Playable, bitrate: Bitrate) -> anyhow::Result<Source> {
    let preference = match bitrate {
        Bitrate::B320 => [0, 1, 2],
        Bitrate::B160 => [1, 2, 0],
        Bitrate::B96 => [2, 1, 0],
    };
    let (format, file) = preference
        .iter()
        .map(|&i| OGG_FORMATS[i])
        .find_map(|f| track.files.get(&f).map(|id| (f, *id)))
        .ok_or_else(|| anyhow!("no Ogg Vorbis audio is available"))?;
    let kbps = match format {
        AudioFileFormat::OGG_VORBIS_320 => 320,
        AudioFileFormat::OGG_VORBIS_160 => 160,
        _ => 96,
    };
    log::debug!("picked {format:?} file {file} for track {}", track.id);
    Ok(Source { file, kbps })
}

/// Asks Spotify for the file's decryption key. This is the request Spotify rate-limits.
pub async fn request_key(
    session: &Session,
    track: &Playable,
    source: Source,
) -> Result<AudioKey, KeyError> {
    session
        .audio_key()
        .request(track.id, source.file)
        .await
        .map_err(|e| {
            log::debug!("audio key request failed: {e}");
            match KeyError::classify(e) {
                // a dead connection goes unanswered too, and that's not a rate limit
                KeyError::NoAnswer if session.is_invalid() => {
                    KeyError::Connection(anyhow!("lost the connection to Spotify"))
                }
                other => other,
            }
        })
}

/// Called with (bytes downloaded, total bytes) as the file comes in; the total is 0
/// when the server doesn't say.
pub type Progress<'a> = &'a mut (dyn FnMut(u64, u64) + Send);

/// Downloads the encrypted file from the CDN and decrypts it with `key`.
pub async fn fetch(
    session: &Session,
    http: &reqwest::Client,
    source: Source,
    key: AudioKey,
    progress: Progress<'_>,
) -> Result<Audio, FetchError> {
    let cdn_url = CdnUrl::new(source.file);
    let cdn = crate::meta::throttled(|| cdn_url.resolve_audio(session))
        .await
        .map_err(|e| {
            FetchError::Network(anyhow!(e).context("resolving the audio file's location"))
        })?;
    let urls = cdn.try_get_urls().map_err(|e| {
        FetchError::Network(anyhow!(e).context("resolving the audio file's location"))
    })?;

    let mut encrypted = None;
    let mut last_error = None;
    for url in urls {
        match download(http, url, &mut *progress).await {
            Ok(bytes) => {
                encrypted = Some(bytes);
                break;
            }
            Err(e) => {
                log::debug!("CDN download failed: {e:#}");
                last_error = Some(e);
            }
        }
    }
    let mut data = encrypted.ok_or_else(|| {
        FetchError::Network(last_error.unwrap_or_else(|| anyhow!("no download locations")))
    })?;
    decrypt(&key, &mut data);

    if data.get(OGG_HEADER_END..OGG_HEADER_END + 4) != Some(b"OggS") {
        return Err(FetchError::BadKey);
    }
    let gain = parse_gain(&data);
    data.drain(..OGG_HEADER_END);

    Ok(Audio { ogg: data, gain })
}

async fn download(
    http: &reqwest::Client,
    url: &str,
    progress: Progress<'_>,
) -> anyhow::Result<Vec<u8>> {
    let mut response = http
        .get(url)
        .header(reqwest::header::RANGE, "bytes=0-")
        .send()
        .await?
        .error_for_status()?;

    // a ranged response reports the full size after the slash: `bytes 0-123/124`
    let total = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse().ok())
        .or_else(|| response.content_length());

    progress(0, total.unwrap_or(0));
    let mut body = Vec::new();
    if let Some(total) = total {
        // the size comes from the server: refuse an absurd one instead of aborting on it
        usize::try_from(total)
            .ok()
            .and_then(|size| body.try_reserve_exact(size).ok())
            .with_context(|| format!("not enough memory for a {total}-byte download"))?;
    }
    while let Some(chunk) = response.chunk().await.context("downloading audio")? {
        body.extend_from_slice(&chunk);
        progress(body.len() as u64, total.unwrap_or(0));
    }
    if let Some(total) = total
        && body.len() as u64 != total
    {
        anyhow::bail!("download cut off at {} of {total} bytes", body.len());
    }
    Ok(body)
}

/// Decrypts a downloaded file in place. librespot's `AudioDecrypt` does the same but
/// only through `Read`, which needs a second full-size buffer for the output.
fn decrypt(key: &AudioKey, data: &mut [u8]) {
    Aes128Ctr::new(&key.0.into(), &AUDIO_AESIV.into()).apply_keystream(data);
}

fn parse_gain(data: &[u8]) -> Option<Gain> {
    let f = |i: usize| {
        let at = NORMALISATION_OFFSET + i * 4;
        Some(f32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?)).filter(|v| v.is_finite())
    };
    Some(Gain {
        track_gain_db: f(0)?,
        track_peak: f(1)?,
        album_gain_db: f(2)?,
        album_peak: f(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_core::SpotifyId;
    use std::collections::HashMap;
    use std::io::{Cursor, Read};

    /// `decrypt` must stay byte-for-byte identical to librespot's own decryption, so a
    /// librespot upgrade that changes the scheme fails here instead of in the field.
    #[test]
    fn decrypts_like_librespot() {
        let key = AudioKey(*b"\x8f\x01\x5a\xc3\x77\x10\xee\x42\x09\xb6\x3d\x91\x24\xfa\x6c\xd8");
        // an odd length, so the last keystream block is only partly used, and several
        // MB like a real track, so librespot reads it in many differently sized chunks
        let encrypted: Vec<u8> = (0..3_000_017u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();

        let mut expected = Vec::new();
        librespot_audio::AudioDecrypt::new(Some(key), Cursor::new(&encrypted))
            .read_to_end(&mut expected)
            .unwrap();
        assert_ne!(
            expected, encrypted,
            "librespot actually decrypted something"
        );

        let mut actual = encrypted;
        decrypt(&key, &mut actual);
        assert!(actual == expected, "decryption differs from librespot's");
    }

    #[test]
    fn classifies_key_errors() {
        let classify = |e: AudioKeyError| KeyError::classify(e.into());
        assert!(matches!(classify(AudioKeyError::AesKey), KeyError::Refused));
        assert!(matches!(
            classify(AudioKeyError::Timeout),
            KeyError::NoAnswer
        ));
        assert!(matches!(
            classify(AudioKeyError::Channel),
            KeyError::Connection(_)
        ));
        let not_connected = librespot_core::Error::internal(anyhow!("not connected"));
        assert!(matches!(
            KeyError::classify(not_connected),
            KeyError::Connection(_)
        ));

        assert!(KeyError::Refused.is_rate_limit());
        assert!(KeyError::NoAnswer.is_rate_limit());
        assert!(!KeyError::Connection(anyhow!("x")).is_rate_limit());
    }

    #[test]
    fn chooses_requested_quality_with_fallback() {
        let file = |n: u8| FileId([n; 20]);
        let track = |formats: &[AudioFileFormat]| Playable {
            id: SpotifyId::from_base62("3uNaLrScYulpXLRmQquP9I").unwrap(),
            files: formats
                .iter()
                .enumerate()
                .map(|(i, f)| (*f, file(i as u8)))
                .collect::<HashMap<_, _>>(),
        };
        let all = track(&OGG_FORMATS);
        assert_eq!(choose(&all, Bitrate::B320).unwrap().kbps, 320);
        assert_eq!(choose(&all, Bitrate::B160).unwrap().kbps, 160);
        assert_eq!(choose(&all, Bitrate::B96).unwrap().kbps, 96);

        let only_160 = track(&[AudioFileFormat::OGG_VORBIS_160]);
        assert_eq!(choose(&only_160, Bitrate::B320).unwrap().kbps, 160);

        let none = track(&[AudioFileFormat::MP3_320]);
        assert!(choose(&none, Bitrate::B320).is_err());
    }
}
