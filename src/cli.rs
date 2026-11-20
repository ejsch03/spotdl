use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::plex;

pub const DEFAULT_TEMPLATE: &str = "{album_artist}/{album}/{track} - {title}";

fn after_help() -> String {
    let h = clap::builder::styling::Style::new().bold().underline();
    format!(
        "\
{h}Inputs:{h:#}
  https://open.spotify.com/...  Track, album, playlist or artist links (spotify.link too)
  spotify:album:<id>            Spotify URIs; the shorthands album:<id>, track:<id>, ... work too
  <id>                          A bare 22-character Spotify ID
  liked                         Your Liked Songs
  saved-albums                  Every album saved in your library
  anything else                 Searched for; the best-matching track is downloaded

{h}Examples:{h:#}
  spotdl https://open.spotify.com/album/4m2880jivSbbyEGAKfITCa
  spotdl never gonna give you up -f mp3
  spotdl liked -o ~/Music
  spotdl -i albums.txt --dry-run
  spotdl spotify:artist:4tZwfgrHOc3mvqYlEYSvVi --releases album,single,compilation

Run `spotdl login` once before downloading. `spotdl --help` lists path template placeholders."
    )
}

const TEMPLATE_HELP: &str = "\
Placeholders: {title} {artist} {album} {album_artist} {track} {track_number} {disc} \
{year} {date} {playlist} {playlist_index} {isrc} {id}. Use `/` to create folders; the \
file extension is added automatically. {track} is the zero-padded track number, prefixed \
with the disc number on multi-disc albums (e.g. `2-05`).";

#[derive(Debug, Parser)]
#[command(
    name = "spotdl",
    version,
    about = "Download songs, albums, playlists and artists from Spotify",
    after_help = after_help(),
    arg_required_else_help = true,
    max_term_width = 100
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub download: DownloadArgs,

    #[command(flatten)]
    pub global: GlobalArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Log in to Spotify in your browser and remember the account
    Login,
    /// Forget the saved Spotify login
    Logout,
    /// Search for tracks and print their links
    Search {
        /// What to search for
        #[arg(required = true, value_name = "QUERY")]
        query: Vec<String>,
        /// Number of results to show
        #[arg(short = 'l', long, default_value_t = 10)]
        limit: usize,
    },
}

#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Where the Spotify login is stored [default: your user config dir]
    #[arg(long, global = true, value_name = "DIR", env = "SPOTDL_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,

    /// Print diagnostic logs (-v for info, -vv for debug)
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Args)]
pub struct DownloadArgs {
    /// Links, URIs, IDs, `liked`, `saved-albums`, or a search query (see below)
    #[arg(value_name = "INPUT")]
    pub inputs: Vec<String>,

    /// Read inputs from a file, one per line (`-` reads stdin; lines starting with # are ignored)
    #[arg(short, long, value_name = "FILE")]
    pub input_file: Option<PathBuf>,

    /// Directory to save downloads in
    #[arg(
        short,
        long,
        value_name = "DIR",
        env = "SPOTDL_OUTPUT",
        default_value = "."
    )]
    pub output: PathBuf,

    /// Audio format to save as
    #[arg(short, long, value_enum, env = "SPOTDL_FORMAT", default_value_t = Format::Ogg)]
    pub format: Format,

    /// Source quality to request from Spotify (falls back to lower if unavailable)
    #[arg(short, long, value_enum, env = "SPOTDL_BITRATE", default_value_t = Bitrate::B320)]
    pub bitrate: Bitrate,

    /// File path template for single tracks, albums and artists
    #[arg(short, long, env = "SPOTDL_TEMPLATE", default_value = DEFAULT_TEMPLATE,
          long_help = format!("File path template for single tracks, albums and artists\n\n{TEMPLATE_HELP}"))]
    pub template: String,

    /// File path template for playlists and Liked Songs [default: same as --template]
    #[arg(long, env = "SPOTDL_PLAYLIST_TEMPLATE", value_name = "TEMPLATE",
          long_help = format!("File path template for playlists and Liked Songs [default: same as \
          --template]\n\nBy default playlist songs are filed under their albums like everything \
          else, so each song is stored once, and the playlists are .m3u8 files (or in Plex; see \
          --plex). For a folder per playlist instead, use \
          '{{playlist}}/{{artist}} - {{title}}'.\n\n{TEMPLATE_HELP}"))]
    pub playlist_template: Option<String>,

    /// Which releases to download for an artist (comma-separated)
    #[arg(long, value_enum, value_delimiter = ',', value_name = "KINDS",
          default_values_t = [Release::Album, Release::Single])]
    pub releases: Vec<Release>,

    /// Write an .m3u8 file for each playlist (and Liked Songs) even when Plex has it
    #[arg(
        long,
        conflicts_with = "no_m3u",
        long_help = "\
          Write an .m3u8 file for each playlist (and Liked Songs) into the output directory, \
          even when Plex has the playlist\n\n\
          By default the .m3u8 is written unless the playlist went to Plex (see --plex): \
          that's always without --plex, and with it when Plex isn't set up on this computer \
          or has no music library holding the output directory. (Plex doesn't read .m3u8 \
          files inside its libraries.)"
    )]
    pub m3u: bool,

    /// Never write .m3u8 files for playlists
    #[arg(long)]
    pub no_m3u: bool,

    /// Add playlists (and Liked Songs) to Plex instead of writing .m3u8 files
    #[arg(
        long,
        env = "SPOTDL_PLEX",
        // so SPOTDL_PLEX takes 1/0, yes/no or on/off, not only true/false
        value_parser = clap::builder::BoolishValueParser::new(),
        long_help = "\
          Add playlists (and Liked Songs) to Plex instead of writing .m3u8 files\n\n\
          Once a playlist is downloaded, it's added to Plex, in Spotify's order, in the music \
          library holding the output directory. Downloading the same playlist again (say, \
          for its new songs) updates it there; it's never added twice. This needs a token \
          (see --plex-token); without one, the playlist is written as an .m3u8 file after \
          all. If Plex can't be reached, the songs are downloaded anyway; run spotdl on the \
          same link again once it's back. SPOTDL_PLEX=1 turns this on for every run."
    )]
    pub plex: bool,

    /// The Plex Media Server to add playlists to (with --plex)
    #[arg(long, value_name = "URL", env = "SPOTDL_PLEX_URL", default_value = plex::DEFAULT_URL)]
    pub plex_url: String,

    /// Plex token, for --plex [default: the one Plex Media Server keeps on this computer]
    #[arg(
        long,
        value_name = "TOKEN",
        env = "SPOTDL_PLEX_TOKEN",
        hide_env_values = true,
        long_help = "\
          Plex token, for --plex [default: the one Plex Media Server keeps on this computer]\n\n\
          When Plex Media Server runs on this computer, its own token is used, so there's \
          nothing to set up: on macOS it's in its settings, on Linux in its Preferences.xml \
          (if spotdl may read it), on Windows in the registry. It's the server owner's, so \
          the playlists are too. The token is never shown or saved."
    )]
    pub plex_token: Option<plex::Token>,

    /// Download again even if the file already exists
    #[arg(long)]
    pub overwrite: bool,

    /// Show what would be downloaded and where, without downloading anything
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Minimum seconds between audio key requests (one per track). Spotify rate-limits
    /// them; lowering this risks failed tracks.
    #[arg(long, value_name = "SECS", env = "SPOTDL_DELAY", default_value_t = 50)]
    pub delay: u64,

    /// Attempts per track before giving up. The whole run stops early if Spotify refuses
    /// 5 audio keys in a row.
    #[arg(long, value_name = "N", default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
    pub retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Spotify's original Ogg Vorbis stream, saved without re-encoding (smallest, no ffmpeg needed)
    Ogg,
    /// FLAC decoded from the original: sounds the same as ogg, about 4x the size (needs ffmpeg)
    Flac,
    /// MP3 at 320 kbps (needs ffmpeg)
    Mp3,
    /// Opus at 192 kbps (needs ffmpeg)
    Opus,
}

impl Format {
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Ogg => "ogg",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
        }
    }

    pub const fn needs_ffmpeg(self) -> bool {
        !matches!(self, Self::Ogg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Bitrate {
    /// 320 kbps (Premium)
    #[value(name = "320")]
    B320,
    /// 160 kbps
    #[value(name = "160")]
    B160,
    /// 96 kbps
    #[value(name = "96")]
    B96,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Release {
    /// Studio albums
    Album,
    /// Singles and EPs
    Single,
    /// Compilations by the artist
    Compilation,
    /// Only the artist's own tracks on other artists' releases
    AppearsOn,
    /// The artist's 10 most popular tracks
    Top,
}
