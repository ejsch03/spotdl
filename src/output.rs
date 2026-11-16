//! Where files go, how they're converted, and how they're tagged.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemValue, Tag, TagItem};
use tokio::io::AsyncWriteExt;

use crate::audio::Gain;
use crate::cli::Format;
use crate::meta::TrackInfo;

// ---------------------------------------------------------------------------------------
// Path templates

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Var {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Track,
    TrackNumber,
    Disc,
    Year,
    Date,
    Playlist,
    PlaylistIndex,
    Isrc,
    Id,
}

const VARS: &[(&str, Var)] = &[
    ("title", Var::Title),
    ("artist", Var::Artist),
    ("album", Var::Album),
    ("album_artist", Var::AlbumArtist),
    ("track", Var::Track),
    ("track_number", Var::TrackNumber),
    ("disc", Var::Disc),
    ("year", Var::Year),
    ("date", Var::Date),
    ("playlist", Var::Playlist),
    ("playlist_index", Var::PlaylistIndex),
    ("isrc", Var::Isrc),
    ("id", Var::Id),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Var(Var),
}

/// A parsed `--template`, e.g. `{album_artist}/{album}/{track} - {title}`.
#[derive(Debug, Clone)]
pub struct Template {
    segments: Vec<Vec<Piece>>,
}

/// Where a track sits in the playlist it's being downloaded from.
#[derive(Debug, Clone, Copy)]
pub struct PlaylistPos<'a> {
    pub name: &'a str,
    pub index: usize,
    pub len: usize,
}

impl Template {
    pub fn parse(template: &str) -> Result<Self> {
        let mut segments = Vec::new();
        for raw in template.split(['/', '\\']).filter(|s| !s.trim().is_empty()) {
            let mut pieces = Vec::new();
            let mut rest = raw;
            while let Some(open) = rest.find('{') {
                if open > 0 {
                    pieces.push(Piece::Text(rest[..open].to_string()));
                }
                let close = rest[open..]
                    .find('}')
                    .ok_or_else(|| anyhow!("unclosed `{{` in template `{template}`"))?
                    + open;
                let name = &rest[open + 1..close];
                let var = VARS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| *v)
                    .ok_or_else(|| {
                        let names: Vec<_> = VARS.iter().map(|(n, _)| format!("{{{n}}}")).collect();
                        anyhow!(
                            "unknown placeholder `{{{name}}}`; available: {}",
                            names.join(" ")
                        )
                    })?;
                pieces.push(Piece::Var(var));
                rest = &rest[close + 1..];
            }
            if !rest.is_empty() {
                pieces.push(Piece::Text(rest.to_string()));
            }
            segments.push(pieces);
        }
        if segments.is_empty() {
            bail!("the file name template is empty");
        }
        Ok(Self { segments })
    }

    /// Renders the path (relative to the output directory) without a file extension.
    pub fn render(&self, track: &TrackInfo, playlist: Option<PlaylistPos>) -> PathBuf {
        self.segments
            .iter()
            .map(|pieces| {
                let text: String = pieces
                    .iter()
                    .map(|piece| match piece {
                        Piece::Text(t) => t.clone(),
                        Piece::Var(var) => value(*var, track, playlist),
                    })
                    .collect();
                sanitize(&text)
            })
            .collect()
    }
}

fn value(var: Var, t: &TrackInfo, playlist: Option<PlaylistPos>) -> String {
    let width = |n: u32| n.to_string().len().max(2);
    let number = format!(
        "{:0w$}",
        t.number,
        w = width(t.album.tracks_on_disc(t.disc))
    );
    match var {
        Var::Title => t.title.clone(),
        Var::Artist => t.artist(),
        Var::Album => t.album.name.clone(),
        Var::AlbumArtist if t.album.artists.is_empty() => t.artist(),
        Var::AlbumArtist => t.album.artists.join(", "),
        Var::Track if t.album.disc_count() > 1 => format!("{}-{number}", t.disc),
        Var::Track | Var::TrackNumber => number,
        Var::Disc => t.disc.to_string(),
        Var::Year => t.album.year().map(|y| y.to_string()).unwrap_or_default(),
        Var::Date => t.album.date.map(|d| d.to_string()).unwrap_or_default(),
        Var::Playlist => playlist.map(|p| p.name.to_string()).unwrap_or_default(),
        Var::PlaylistIndex => playlist
            .map(|p| format!("{:0w$}", p.index, w = width(p.len as u32)))
            .unwrap_or_default(),
        Var::Isrc => t.isrc.clone().unwrap_or_default(),
        Var::Id => t.id.to_base62().unwrap_or_default(),
    }
}

/// Makes one path component safe on every major filesystem (so a library can be copied
/// to a phone or USB stick) and keeps it from escaping into other directories.
pub fn sanitize(name: &str) -> String {
    let name = name.replace(": ", " - ");
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' | '\\' | ':' | '|' => out.push('-'),
            '"' => out.push('\''),
            '?' | '*' | '<' | '>' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    // no leading dots (hidden files, `..`), no trailing dots/spaces (invalid on Windows)
    let mut out = out
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim()
        .to_string();

    const MAX_BYTES: usize = 180; // leaves room for the extension and `.part`
    if out.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out = out.trim_end_matches(['.', ' ']).to_string();
    }

    let stem = out
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        out.push('_');
    }
    if out.is_empty() { "_".into() } else { out }
}

/// `~/Music` → the Music folder in the home directory. Shells do this on Linux and
/// macOS, but not Windows' cmd and PowerShell, nor a quoted `-o "~/Music"` or a typed
/// folder in the desktop app.
pub fn expand_home(path: &Path) -> PathBuf {
    let mut parts = path.components();
    match (parts.next(), dirs::home_dir()) {
        (Some(std::path::Component::Normal(first)), Some(home)) if first == "~" => {
            home.join(parts.as_path())
        }
        _ => path.to_path_buf(),
    }
}

/// Appends an extension without clobbering dots in the name (`Vol. 2` stays intact).
pub fn with_extension(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------------------
// The download folder

/// The download folder can't take files: the drive isn't connected, it's full or
/// read-only, or spotdl isn't allowed to write there. Unlike a problem with one track,
/// every other track would fail the same way, so downloading into the folder stops.
#[derive(Debug)]
pub struct Unwritable {
    folder: PathBuf,
    reason: String,
}

impl std::fmt::Display for Unwritable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Can't save to {}: {}",
            self.folder.display(),
            self.reason
        )
    }
}

impl std::error::Error for Unwritable {}

impl Unwritable {
    fn new(folder: &Path, reason: String) -> Self {
        Self {
            folder: folder.to_path_buf(),
            reason,
        }
    }

    /// What writing into `folder` failing with `error` means, if it's about the whole
    /// folder rather than one file.
    pub fn from_io(folder: &Path, error: &io::Error) -> Option<Self> {
        use io::ErrorKind::*;
        #[cfg(windows)]
        if let Some(reason) = windows_reason(error) {
            return Some(Self::new(folder, reason));
        }
        let reason = match error.kind() {
            StorageFull | QuotaExceeded => "the drive is full".to_string(),
            ReadOnlyFilesystem if cfg!(target_os = "macos") => "the drive is read-only (macOS \
                can only read NTFS drives; use one formatted as APFS, Mac OS Extended or exFAT)"
                .to_string(),
            ReadOnlyFilesystem => "the drive is read-only".to_string(),
            PermissionDenied if cfg!(target_os = "macos") => format!(
                "{error}. If it's on an external drive, allow spotdl (or the terminal app you \
                 run it from) to use removable volumes, in System Settings → Privacy & \
                 Security → Files & Folders"
            ),
            PermissionDenied if cfg!(windows) => format!(
                "{error}. If Windows Security's Controlled folder access is on, allow \
                 spotdl-gui.exe and spotdl.exe through it (Virus & threat protection → \
                 Ransomware protection), or choose a folder it doesn't protect"
            ),
            PermissionDenied => error.to_string(),
            NotFound => "the folder disappeared (was the drive disconnected?)".to_string(),
            _ => return None,
        };
        Some(Self::new(folder, reason))
    }

    /// The same, for the first I/O error behind `error`.
    pub fn find(folder: &Path, error: &anyhow::Error) -> Option<Self> {
        error
            .chain()
            .find_map(|e| e.downcast_ref::<io::Error>())
            .and_then(|e| Self::from_io(folder, e))
    }
}

/// Windows' own words for a drive that's gone or can't be written to.
#[cfg(windows)]
fn windows_reason(error: &io::Error) -> Option<String> {
    use windows_sys::Win32::Foundation::{
        ERROR_DEV_NOT_EXIST, ERROR_INVALID_DRIVE, ERROR_NOT_READY, ERROR_WRITE_PROTECT,
    };
    let code = u32::try_from(error.raw_os_error()?).ok()?;
    match code {
        ERROR_NOT_READY | ERROR_INVALID_DRIVE | ERROR_DEV_NOT_EXIST => {
            Some("the drive isn't connected (or isn't ready)".into())
        }
        ERROR_WRITE_PROTECT => Some("the drive is write-protected".into()),
        _ => None,
    }
}

/// Makes sure a track can be saved in the download folder `output`, with `needed` bytes
/// to spare, before an audio key is spent on it: creates the track's folder and its
/// empty `part` file (see [`PartFile`]; the download is written into it later), and
/// checks the free space. A problem with the folder as a whole is an [`Unwritable`]
/// error; one with this track alone (say, a name the drive won't take) is a plain error.
pub fn check_destination(output: &Path, part: &Path, needed: u64) -> Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(problem) = missing_drive(output) {
        return Err(Unwritable::new(output, problem).into());
    }
    let failed = |e: io::Error, what: &Path| match Unwritable::from_io(output, &e) {
        Some(unwritable) => anyhow::Error::new(unwritable),
        None => anyhow::Error::new(e).context(format!("creating {}", what.display())),
    };
    let dir = part.parent().unwrap_or(output);
    std::fs::create_dir_all(dir).map_err(|e| failed(e, dir))?;
    // (This empties one left behind by a crash.) It's kept, not removed and created
    // again for the download: on Windows, a virus scanner may still have the file open
    // just after seeing it created, and creating it anew would fail.
    std::fs::File::create(part).map_err(|e| failed(e, part))?;

    if let Some(free) = free_space(dir)
        && free < needed
    {
        let reason = format!(
            "there's not enough room on the drive (only {} left)",
            indicatif::HumanBytes(free)
        );
        return Err(Unwritable::new(output, reason).into());
    }
    Ok(())
}

/// Bytes free for this user on the drive holding `dir`, if the filesystem says.
#[cfg(target_os = "macos")]
fn free_space(dir: &Path) -> Option<u64> {
    // statfs, not statvfs: macOS's statvfs has 32-bit block counts, which wrap on big drives
    let s = rustix::fs::statfs(dir).ok()?;
    // some network filesystems report no size at all; that's not the same as full
    (s.f_blocks > 0).then(|| s.f_bavail.saturating_mul(u64::from(s.f_bsize)))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn free_space(dir: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(dir).ok()?;
    (s.f_blocks > 0).then(|| s.f_bavail.saturating_mul(s.f_frsize))
}

#[cfg(windows)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: `path` is a NUL-terminated wide string that outlives the call, the two
    // out-pointers point at live u64s, and the count that isn't needed may be null.
    let ok =
        unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0 && total > 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// Why `output` can't be written to, if it's on an external drive that isn't connected.
///
/// macOS mounts drives at `/Volumes/<name>`. Without the drive, writing there would at
/// best fail with a puzzling error. At worst (if `/Volumes/<name>` had been created as
/// a plain folder) downloads would quietly fill the startup disk instead, and the drive
/// would be mounted as "<name> 1" from then on, away from where spotdl and Plex look.
#[cfg(target_os = "macos")]
fn missing_drive(output: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let output = std::path::absolute(output).ok()?;
    let name = drive_name(&output)?;
    let mount_point = Path::new("/Volumes").join(name);
    let name = name.to_string_lossy();
    match std::fs::metadata(&mount_point) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Some(format!("the drive “{name}” isn't connected"))
        }
        // the startup disk is linked into /Volumes (e.g. "Macintosh HD"); it's connected
        Ok(_) if std::fs::canonicalize(&mount_point).is_ok_and(|p| p == Path::new("/")) => None,
        // a drive is a filesystem of its own; this folder is on the same one as /Volumes
        Ok(found) if std::fs::metadata("/Volumes").is_ok_and(|v| v.dev() == found.dev()) => {
            Some(format!(
                "the drive “{name}” isn't connected ({} is an ordinary folder on the startup \
                 disk; remove it if it's empty)",
                mount_point.display()
            ))
        }
        // connected; or unreadable, in which case writing says why
        _ => None,
    }
}

/// The `<name>` of an absolute path under `/Volumes/<name>`.
#[cfg(any(target_os = "macos", test))]
fn drive_name(path: &Path) -> Option<&std::ffi::OsStr> {
    use std::path::Component;
    let mut parts = path.components();
    match (parts.next()?, parts.next()?, parts.next()?) {
        (Component::RootDir, Component::Normal(volumes), Component::Normal(name))
            if volumes == "Volumes" =>
        {
            Some(name)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// Writing files

/// Whether a finished download is already at `path`. An empty file doesn't count: a
/// crash at the wrong moment can leave one behind, and it would otherwise be skipped
/// forever.
pub fn already_downloaded(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// A file being written next to its final location. It's deleted if dropped before
/// `finish`, so interrupted downloads never leave half-written tracks behind.
pub struct PartFile {
    path: PathBuf,
    done: bool,
}

impl PartFile {
    pub fn new(final_path: &Path) -> Self {
        Self {
            path: with_extension(final_path, "part"),
            done: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flushes the file to disk, then moves it into place, so even a power cut can't
    /// leave an empty or partial file under the final name. Returns the file size.
    pub fn finish(mut self, final_path: &Path) -> Result<u64> {
        // write access, because Windows refuses to flush a read-only handle
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .context("opening the finished download")?;
        flush_to_disk(&file).context("flushing the download to disk")?;
        let size = file.metadata().map(|m| m.len()).unwrap_or_default();
        drop(file);

        rename_patiently(&self.path, final_path)
            .with_context(|| format!("moving download to {}", final_path.display()))?;
        self.done = true;

        // make the rename itself durable too (best effort; not every filesystem can)
        #[cfg(unix)]
        {
            if let Some(dir) = final_path.parent().filter(|d| !d.as_os_str().is_empty()) {
                _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
            }
        }
        Ok(size)
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.done {
            _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Renames a file. On Windows, renaming fails while another program has the file open,
/// and virus scanners and the search indexer often open new files for a moment, so it
/// tries again for a few seconds before giving up.
fn rename_patiently(from: &Path, to: &Path) -> io::Result<()> {
    let mut wait = std::time::Duration::from_millis(50);
    for _ in 0..8 {
        match std::fs::rename(from, to) {
            Err(e) if cfg!(windows) && in_use(&e) => {
                log::debug!("{} is in use ({e}); trying again", from.display());
                std::thread::sleep(wait);
                wait = (wait * 2).min(std::time::Duration::from_secs(1));
            }
            result => return result,
        }
    }
    std::fs::rename(from, to)
}

/// Whether a Windows error means another program has the file open.
fn in_use(error: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
    error.kind() == io::ErrorKind::PermissionDenied || matches!(error.raw_os_error(), Some(32 | 33))
}

/// Flushes a file all the way to the disk. On macOS, std's `sync_all` is a "full" fsync
/// (`F_FULLFSYNC`), which some volumes can't do (network shares, for one): they get a
/// plain fsync, the most they offer, rather than failing every download.
fn flush_to_disk(file: &std::fs::File) -> io::Result<()> {
    let result = file.sync_all();
    #[cfg(target_os = "macos")]
    if let Err(e) = &result {
        use rustix::io::Errno;
        let unsupported = [Errno::NOTSUP, Errno::OPNOTSUPP, Errno::NOTTY, Errno::INVAL];
        if Errno::from_io_error(e).is_some_and(|errno| unsupported.contains(&errno)) {
            log::debug!("this volume can't do a full fsync ({e}); using a plain one");
            return rustix::fs::fsync(file).map_err(io::Error::from);
        }
    }
    result
}

/// The ffmpeg to run. Package managers' folders are searched as well as the PATH: on
/// macOS, apps started from the Finder or the Dock get a bare PATH without Homebrew's or
/// MacPorts' folders; on Windows, a running app doesn't see PATH changes made after it
/// started, such as installing ffmpeg with winget.
pub fn ffmpeg() -> PathBuf {
    let usual: Vec<PathBuf> = if cfg!(target_os = "macos") {
        ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"]
            .map(PathBuf::from)
            .into()
    } else if cfg!(windows) {
        let env = |name| std::env::var_os(name).map(PathBuf::from);
        [
            env("LOCALAPPDATA").map(|d| d.join("Microsoft").join("WinGet").join("Links")),
            env("ProgramFiles").map(|d| d.join("WinGet").join("Links")),
            env("USERPROFILE").map(|d| d.join("scoop").join("shims")),
            env("ProgramData").map(|d| d.join("chocolatey").join("bin")),
        ]
        .into_iter()
        .flatten()
        .collect()
    } else {
        return PathBuf::from("ffmpeg"); // the PATH is all there is
    };
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(usual)
        .map(|dir| dir.join(name))
        .find(|p| is_program(p))
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

fn is_program(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// ffmpeg, ready to run. On Windows, without the console window a command line program
/// would otherwise flash up each time it's started from the desktop app.
fn ffmpeg_command() -> tokio::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut command = tokio::process::Command::new(ffmpeg());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// How to get ffmpeg, for messages.
pub const FFMPEG_HINT: &str = if cfg!(target_os = "macos") {
    "Install it with Homebrew (`brew install ffmpeg`)"
} else if cfg!(windows) {
    "Install it with winget (`winget install Gyan.FFmpeg`)"
} else {
    "Install ffmpeg"
};

pub async fn ffmpeg_available() -> bool {
    ffmpeg_command()
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Converts an Ogg Vorbis stream into `format` at `dest`.
pub async fn transcode(ogg: &[u8], format: Format, dest: &Path) -> Result<()> {
    let codec: &[&str] = match format {
        // the decoder outputs float samples; keep 24 bits instead of truncating to 16
        Format::Flac => &[
            "-c:a",
            "flac",
            "-sample_fmt",
            "s32",
            "-bits_per_raw_sample",
            "24",
            "-f",
            "flac",
        ],
        Format::Mp3 => &["-c:a", "libmp3lame", "-b:a", "320k", "-f", "mp3"],
        Format::Opus => &["-c:a", "libopus", "-b:a", "192k", "-f", "opus"],
        Format::Ogg => unreachable!("ogg is saved without conversion"),
    };

    let mut child = ffmpeg_command()
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "ogg",
            "-i",
            "pipe:0",
        ])
        .args(["-map", "0:a", "-map_metadata", "-1"])
        .args(codec)
        .arg(dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("couldn't start ffmpeg")?;

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let feed = async move {
        let result = stdin.write_all(ogg).await;
        drop(stdin); // EOF tells ffmpeg to finish the file
        result
    };
    let (fed, output) = tokio::join!(feed, child.wait_with_output());
    let output = output.context("waiting for ffmpeg")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg failed ({}): {}", output.status, stderr.trim());
    }
    fed.context("sending audio to ffmpeg")?;
    Ok(())
}

pub struct TagData {
    title: String,
    artist: String,
    artists: Vec<String>,
    album: String,
    album_artist: String,
    track: u32,
    track_total: u32,
    disc: u32,
    disc_total: u32,
    date: Option<String>,
    isrc: Option<String>,
    label: Option<String>,
    copyright: Option<String>,
    gain: Option<Gain>,
    cover: Option<Arc<Vec<u8>>>,
}

impl TagData {
    pub fn new(t: &TrackInfo, gain: Option<Gain>, cover: Option<Arc<Vec<u8>>>) -> Self {
        Self {
            title: t.title.clone(),
            artist: t.artist(),
            artists: t.artists.clone(),
            album: t.album.name.clone(),
            album_artist: if t.album.artists.is_empty() {
                t.artist()
            } else {
                t.album.artists.join(", ")
            },
            track: t.number,
            track_total: t.album.tracks_on_disc(t.disc),
            disc: t.disc,
            disc_total: t.album.disc_count(),
            date: t.album.date.map(|d| d.to_string()),
            isrc: t.isrc.clone(),
            label: t.album.label.clone(),
            copyright: t.album.copyright.clone(),
            gain,
            cover,
        }
    }
}

/// Replaces the file's tags with ours. Works for every output format (Vorbis comments
/// for ogg/opus/flac, ID3v2 for mp3) and embeds the cover art.
pub fn write_tags(path: &Path, data: &TagData) -> Result<()> {
    let mut file = Probe::open(path)?
        .guess_file_type()?
        .read()
        .context("reading audio file")?;
    let mut tag = Tag::new(file.primary_tag_type());

    tag.set_title(data.title.clone());
    tag.set_artist(data.artist.clone());
    tag.set_album(data.album.clone());
    tag.insert_text(ItemKey::AlbumArtist, data.album_artist.clone());
    if data.artists.len() > 1 {
        for a in &data.artists {
            tag.push(TagItem::new(
                ItemKey::TrackArtists,
                ItemValue::Text(a.clone()),
            ));
        }
    }
    tag.set_track(data.track);
    tag.set_track_total(data.track_total);
    tag.set_disk(data.disc);
    tag.set_disk_total(data.disc_total);
    for (key, value) in [
        (ItemKey::RecordingDate, &data.date),
        (ItemKey::Isrc, &data.isrc),
        (ItemKey::Label, &data.label),
        (ItemKey::CopyrightMessage, &data.copyright),
    ] {
        if let Some(value) = value {
            tag.insert_text(key, value.clone());
        }
    }
    if let Some(g) = data.gain {
        tag.insert_text(
            ItemKey::ReplayGainTrackGain,
            format!("{:.2} dB", g.track_gain_db),
        );
        tag.insert_text(ItemKey::ReplayGainTrackPeak, format!("{:.6}", g.track_peak));
        tag.insert_text(
            ItemKey::ReplayGainAlbumGain,
            format!("{:.2} dB", g.album_gain_db),
        );
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, format!("{:.6}", g.album_peak));
    }
    if let Some(cover) = &data.cover {
        tag.push_picture(
            Picture::unchecked(cover.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
    }

    file.insert_tag(tag);
    file.save_to_path(path, WriteOptions::default().use_id3v23(true))
        .context("writing tags")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Playlist files

pub struct M3uEntry {
    pub path: PathBuf,
    pub seconds: u32,
    pub label: String,
}

/// Writes an extended M3U playlist with paths relative to the playlist file's directory.
pub fn write_m3u(path: &Path, entries: &[M3uEntry]) -> Result<()> {
    let base = path.parent().unwrap_or(Path::new("."));
    let mut out = String::from("#EXTM3U\n");
    for e in entries {
        let rel = e.path.strip_prefix(base).unwrap_or(&e.path);
        let rel: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        out.push_str(&format!(
            "#EXTINF:{},{}\n{}\n",
            e.seconds,
            e.label,
            rel.join("/")
        ));
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{AlbumInfo, ReleaseDate};
    use librespot_core::SpotifyId;

    fn track(discs: Vec<(u32, u32)>, disc: u32, number: u32) -> TrackInfo {
        let id = SpotifyId::from_base62("3uNaLrScYulpXLRmQquP9I").unwrap();
        TrackInfo {
            id,
            title: "Who/What: A Song?".into(),
            artists: vec!["Nujabes".into(), "Shing02".into()],
            artist_ids: vec![],
            album: Arc::new(AlbumInfo {
                id,
                name: "...Album Vol. 2".into(),
                kind: "Album",
                artists: vec!["Nujabes".into()],
                date: Some(ReleaseDate {
                    year: 2004,
                    month: Some(6),
                    day: Some(23),
                }),
                label: None,
                copyright: None,
                discs,
                tracks: vec![],
                cover: None,
                thumbnail: None,
            }),
            number,
            disc,
            duration_ms: 1000,
            isrc: None,
            playable: Err("test".into()),
        }
    }

    #[test]
    fn renders_default_template() {
        let t = Template::parse(crate::cli::DEFAULT_TEMPLATE).unwrap();
        let path = t.render(&track(vec![(1, 17)], 1, 3), None);
        assert_eq!(
            path,
            PathBuf::from("Nujabes/Album Vol. 2/03 - Who-What - A Song")
        );
        assert_eq!(
            with_extension(&path, "flac"),
            PathBuf::from("Nujabes/Album Vol. 2/03 - Who-What - A Song.flac")
        );
    }

    #[test]
    fn multi_disc_and_playlists() {
        let t = Template::parse("{track} {title}").unwrap();
        assert_eq!(
            t.render(&track(vec![(1, 10), (2, 12)], 2, 5), None),
            PathBuf::from("2-05 Who-What - A Song")
        );

        let t = Template::parse("{playlist}/{playlist_index} {artist} - {year}").unwrap();
        let pos = PlaylistPos {
            name: "Road Trip",
            index: 7,
            len: 150,
        };
        assert_eq!(
            t.render(&track(vec![(1, 1)], 1, 1), Some(pos)),
            PathBuf::from("Road Trip/007 Nujabes, Shing02 - 2004")
        );
    }

    #[test]
    fn rejects_bad_templates() {
        assert!(Template::parse("{nope}").is_err());
        assert!(Template::parse("{title").is_err());
        assert!(Template::parse("//").is_err());
    }

    /// Converts a generated Vorbis stream to every format and checks the tags and cover
    /// survive. Skipped when ffmpeg isn't installed.
    #[tokio::test]
    async fn converts_and_tags_every_format() {
        if !ffmpeg_available().await {
            eprintln!("ffmpeg not found; skipping");
            return;
        }
        let tone = |args: &[&str]| {
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                ])
                .args(args)
                .output()
                .unwrap()
                .stdout
        };
        let ogg = tone(&["-c:a", "libvorbis", "-f", "ogg", "-"]);
        let cover = tone(&["-frames:v", "1", "-f", "mjpeg", "-"]);
        assert!(ogg.starts_with(b"OggS"));

        let dir = std::env::temp_dir().join(format!("spotdl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data = TagData::new(&track(vec![(1, 17)], 1, 3), None, Some(Arc::new(cover)));

        for format in [Format::Ogg, Format::Flac, Format::Mp3, Format::Opus] {
            let path = dir.join(format!("test.{}.part", format.extension()));
            match format {
                Format::Ogg => std::fs::write(&path, &ogg).unwrap(),
                f => transcode(&ogg, f, &path).await.unwrap(),
            }
            write_tags(&path, &data).unwrap();

            let file = Probe::open(&path)
                .unwrap()
                .guess_file_type()
                .unwrap()
                .read()
                .unwrap();
            let tag = file.primary_tag().expect("tag written");
            assert_eq!(
                tag.title().as_deref(),
                Some("Who/What: A Song?"),
                "{format:?}"
            );
            assert_eq!(tag.track(), Some(3), "{format:?}");
            assert_eq!(
                tag.get_string(ItemKey::RecordingDate),
                Some("2004-06-23"),
                "{format:?}"
            );
            assert_eq!(tag.pictures().len(), 1, "{format:?} cover");
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn part_files_land_complete_or_not_at_all() {
        let dir = std::env::temp_dir().join(format!("spotdl-part-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("song.ogg");

        // dropped before finishing: nothing is left behind
        let part = PartFile::new(&dest);
        std::fs::write(part.path(), b"half").unwrap();
        let part_path = part.path().to_path_buf();
        drop(part);
        assert!(!part_path.exists());
        assert!(!already_downloaded(&dest));

        // finished: moved into place with its size reported
        let part = PartFile::new(&dest);
        std::fs::write(part.path(), b"complete").unwrap();
        assert_eq!(part.finish(&dest).unwrap(), 8);
        assert!(!part_path.exists());
        assert!(already_downloaded(&dest));

        // an empty file (e.g. from a crash) doesn't count as downloaded
        std::fs::write(&dest, b"").unwrap();
        assert!(!already_downloaded(&dest));
        assert!(!already_downloaded(&dir));

        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checks_the_destination_before_downloading() {
        let dir = std::env::temp_dir().join(format!("spotdl-check-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("Artist/Album/01 - Song.ogg");
        let part = PartFile::new(&file);

        check_destination(&dir, part.path(), 1 << 20).unwrap();
        assert!(
            file.parent().unwrap().is_dir(),
            "creates the track's folder"
        );
        assert_eq!(
            std::fs::read(part.path()).unwrap(),
            b"",
            "an empty part file"
        );
        assert!(!file.exists());

        // a `.part` left behind by a crash is emptied
        std::fs::write(part.path(), b"stale").unwrap();
        check_destination(&dir, part.path(), 0).unwrap();
        assert_eq!(std::fs::read(part.path()).unwrap(), b"");

        // no room for the track: nothing else fits either
        #[cfg(any(unix, windows))]
        {
            let e = check_destination(&dir, part.path(), u64::MAX).unwrap_err();
            let problem = e
                .downcast_ref::<Unwritable>()
                .expect("about the whole folder");
            assert!(problem.to_string().contains("not enough room"), "{problem}");
        }
        // a track that isn't downloaded after all leaves nothing behind
        let part_path = part.path().to_path_buf();
        drop(part);
        assert!(!part_path.exists());

        // a name the filesystem won't take is about that one track
        let long = PartFile::new(&dir.join(format!("{}.ogg", "x".repeat(300))));
        let e = check_destination(&dir, long.path(), 0).unwrap_err();
        assert!(e.downcast_ref::<Unwritable>().is_none(), "{e:#}");

        _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_is_unwritable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("spotdl-ro-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // root may write anywhere, so there's nothing to check then
        if std::fs::write(dir.join("probe"), b"").is_err() {
            let part = PartFile::new(&dir.join("A/B/01 - x.ogg"));
            let e = check_destination(&dir, part.path(), 0).unwrap_err();
            let problem = e
                .downcast_ref::<Unwritable>()
                .expect("about the whole folder");
            assert!(
                problem.to_string().starts_with("Can't save to "),
                "{problem}"
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tells_folder_problems_from_track_problems() {
        let folder = Path::new("/music");
        let full =
            anyhow::Error::new(io::Error::from(io::ErrorKind::StorageFull)).context("writing file");
        let problem = Unwritable::find(folder, &full).expect("a full drive");
        assert_eq!(
            problem.to_string(),
            "Can't save to /music: the drive is full"
        );
        let read_only = anyhow::Error::new(io::Error::from(io::ErrorKind::ReadOnlyFilesystem));
        assert!(Unwritable::find(folder, &read_only).is_some());

        let bad_name = anyhow::Error::new(io::Error::from(io::ErrorKind::InvalidFilename));
        assert!(Unwritable::find(folder, &bad_name).is_none());
        assert!(Unwritable::find(folder, &anyhow!("ffmpeg failed")).is_none());
    }

    #[test]
    fn finds_drive_names() {
        use std::ffi::OsStr;
        fn name(p: &str) -> Option<&OsStr> {
            drive_name(Path::new(p))
        }
        assert_eq!(name("/Volumes/Music/Spotify"), Some(OsStr::new("Music")));
        assert_eq!(name("/Volumes/My Drive"), Some(OsStr::new("My Drive")));
        assert_eq!(name("/Volumes"), None);
        assert_eq!(name("/Users/me/Music"), None);
        assert_eq!(name("Volumes/Music"), None);
    }

    /// Runs on macOS only, where it checks against the real /Volumes.
    #[cfg(target_os = "macos")]
    #[test]
    fn notices_a_drive_that_isnt_connected() {
        let drive = Path::new("/Volumes/spotdl-test-no-such-drive");
        let output = drive.join("Music");
        let part = PartFile::new(&output.join("A/B/01 - x.ogg"));
        let e = check_destination(&output, part.path(), 0).unwrap_err();
        let problem = e
            .downcast_ref::<Unwritable>()
            .expect("about the whole folder");
        assert!(problem.to_string().contains("isn't connected"), "{problem}");
        assert!(!drive.exists(), "nothing was created in its place");

        assert_eq!(missing_drive(Path::new("/Users")), None);
        // the startup disk is linked into /Volumes too (e.g. "Macintosh HD"), and counts
        let startup = std::fs::read_dir("/Volumes")
            .unwrap()
            .flatten()
            .find(|e| std::fs::canonicalize(e.path()).is_ok_and(|p| p == Path::new("/")));
        if let Some(startup) = startup {
            assert_eq!(missing_drive(&startup.path().join("Users")), None);
        }
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("AC/DC"), "AC-DC");
        assert_eq!(sanitize("Trailing dot."), "Trailing dot");
        assert_eq!(sanitize("CON"), "CON_");
        assert_eq!(sanitize("what?*<>"), "what");
        assert!(sanitize(&"é".repeat(200)).len() <= 180);
    }
}
