# spotdl

Download songs, albums, playlists, artists and your library from Spotify into a tidy,
fully tagged music folder.

```console
$ spotdl https://open.spotify.com/album/3uNaLrScYulpXLRmQquP9I
Album · samurai champloo music record departure — Nujabes, fat jon · 2004 · 17 tracks
✓ [ 1/17] battlecry — Nujabes, Shing02  9.64 MiB · 320 kbps source
  [ 2/17] the space between two world — Nujabes  ███████████▌        5.1 MiB/9.8 MiB
```

- Paste any Spotify link, URI or ID, or just type what you're looking for.
- Fetches Spotify's original 320 kbps files directly, with no real-time playback and no re-recording.
- Saves Spotify's original Ogg Vorbis file untouched (default), or converts it to FLAC, MP3
  or Opus.
- Tags title, artists, album, album artist, track/disc numbers, release date, ISRC,
  label, copyright and ReplayGain, and embeds full-resolution cover art.
- Skips files you already have, so re-running a playlist only fetches what's new.

## Setup

You need a Spotify **Premium** account, a Rust toolchain, and [ffmpeg](https://ffmpeg.org)
on your `PATH` if you want FLAC, MP3 or Opus (the default, Ogg, doesn't need it).

On Linux, install it with its app menu entry and icon:

```sh
./install.sh        # builds, then installs into ~/.local (./install.sh --uninstall removes it)
spotdl login        # or log in from the app; opens your browser once and is remembered
```

This puts the `spotdl` command and the `spotdl-gui` desktop app in `~/.local/bin`, adds
**spotdl** to your app menu, and installs its icon into the icon theme, so the window, the
taskbar and the tray all show it. `PREFIX=/usr/local sudo -E ./install.sh` installs it for
everyone instead (after `cargo build --release --locked` as yourself). The desktop app needs
fontconfig (`libfontconfig1-dev` on Debian/Ubuntu, `fontconfig` on Arch).

On macOS, the same script installs the app as `~/Applications/spotdl.app`, so it's in
Launchpad, Spotlight and the Dock with its icon, and the commands into `~/.local/bin`. It
needs Rust and Apple's command line tools (`xcode-select --install`). For FLAC, MP3 or
Opus, `brew install ffmpeg`; the app finds Homebrew's ffmpeg even when it's started from the
Dock. See [On a Mac](#on-a-mac) for using an external drive.

On Windows, install Rust from <https://rustup.rs> (it offers to set up the Visual Studio
C++ build tools it needs), then, in this folder:

```powershell
powershell -ExecutionPolicy Bypass -File install.ps1   # add -Uninstall to remove it again
spotdl login
```

This puts both programs in `%LOCALAPPDATA%\Programs\spotdl`, adds that folder to your PATH,
and adds **spotdl** to the Start menu with its icon. For FLAC, MP3 or Opus,
`winget install Gyan.FFmpeg`; the app finds it without a restart. See
[On Windows](#on-windows).

Elsewhere, or for the command line alone (e.g. on a server):

```sh
cargo install --path .                        # both programs, without menu entry or icon
cargo install --path . --no-default-features  # just the command line
```

The login is stored in your user config directory (`~/.config/spotdl` on Linux,
`~/Library/Application Support/spotdl` on macOS, `%APPDATA%\spotdl` on Windows). If you
run `spotdl` before logging in, it walks you through it.

Logging in on a machine without a browser (say, over SSH) needs no setup: open the link
`spotdl login` prints in a browser on any computer and log in there. The login ends on a
page that doesn't load (it's meant for the machine running spotdl); copy that page's
address and paste it into spotdl. Forwarding the port works too:
`ssh -L 8898:127.0.0.1:8898 your-server`.

## Usage

```sh
spotdl <INPUT>...                 # download
spotdl search <QUERY>             # list matching tracks with their links
spotdl login | logout
```

| Input                                           | Downloads                                  |
|-------------------------------------------------|--------------------------------------------|
| `https://open.spotify.com/track/…` (or `spotify.link/…`) | that track                        |
| `https://open.spotify.com/album/…`              | the whole album                            |
| `https://open.spotify.com/playlist/…`           | the playlist                               |
| `https://open.spotify.com/artist/…`             | the artist's albums and singles (see `--releases`) |
| `spotify:album:<id>`, `album:<id>`, `<id>`      | URIs, shorthands and bare IDs work too     |
| `liked`                                         | your Liked Songs                           |
| `saved-albums`                                  | every album in your library                |
| anything else, e.g. `daft punk one more time`   | the best-matching track (no quotes needed) |

Mix as many inputs as you like, or put them in a file (one per line, `#` for comments):

```sh
spotdl -i albums.txt
spotdl liked -o ~/Music
spotdl spotify:artist:4tZwfgrHOc3mvqYlEYSvVi --releases album,single,compilation
spotdl never gonna give you up -f mp3
spotdl -n https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M   # dry run: show paths only
```

Search downloads the top result. Run `spotdl search …` first if you want to check it.

### Formats

| `-f`            | Result                                                                      |
|-----------------|-----------------------------------------------------------------------------|
| `ogg` (default) | Spotify's original 320 kbps Ogg Vorbis file, byte-for-byte. Best quality, smallest, fastest, no ffmpeg |
| `flac`          | 24-bit FLAC decoded from the original. Sounds identical to `ogg` at about 4x the size; for players that can't play Ogg |
| `mp3`           | 320 kbps MP3 (ID3v2.3 tags), for maximum device compatibility; re-encoded, so very slightly lossier |
| `opus`          | 192 kbps Opus; re-encoded, so very slightly lossier                          |

Spotify only serves lossy Ogg Vorbis, so no format can sound better than `ogg`: FLAC is a
lossless copy of that file, not of the original master. Vorbis at 320 kbps is
indistinguishable from CD quality for almost everyone, at about 2.4 MB per minute.

`-b 160` / `-b 96` request lower source quality (for non-Premium accounts).

### File names

Files are named with templates relative to `-o/--output` (default: the current directory):

| Option                | Used for                    | Default                                    |
|-----------------------|-----------------------------|--------------------------------------------|
| `-t, --template`      | tracks, albums, artists     | `{album_artist}/{album}/{track} - {title}` |
| `--playlist-template` | playlists and Liked Songs   | the same as `--template`                   |

Placeholders: `{title}` `{artist}` `{album}` `{album_artist}` `{track}` `{track_number}`
`{disc}` `{year}` `{date}` `{playlist}` `{playlist_index}` `{isrc}` `{id}`. `{track}` is
the zero-padded track number, prefixed with the disc on multi-disc albums (`2-05`).
Characters that aren't valid in file names on Windows, macOS or Linux are replaced, so a
library can be copied to any drive.

Playlists (and Liked Songs) are filed under their albums like everything else, so every
song is stored once: a song that's in several playlists, or that you already have from an
album, is skipped instead of downloaded again, which also saves an audio key request. Each
playlist is written as `<playlist>.m3u8` in the output directory, listing its songs in order
with paths relative to it, so it keeps working if the drive is mounted somewhere else. Most
players and music servers (VLC, foobar2000, MusicBee, Plex, Jellyfin, Navidrome, ...) read
these.

For a folder per playlist instead, use `--playlist-template '{playlist}/{artist} - {title}'`.
`--no-m3u` skips the `.m3u8` files.

### Defaults via environment

Set `SPOTDL_OUTPUT`, `SPOTDL_FORMAT`, `SPOTDL_BITRATE`, `SPOTDL_TEMPLATE`,
`SPOTDL_PLAYLIST_TEMPLATE`, `SPOTDL_DELAY` or `SPOTDL_CONFIG_DIR` to change the defaults,
for example `export SPOTDL_OUTPUT=~/Music SPOTDL_FORMAT=mp3`.

## Desktop app

`spotdl-gui` opens a window where you can search for songs, albums, artists and playlists
and download them with one click. **Your playlists** lists the playlists in your library
(yours and the ones you follow). Paste a Spotify link into the search box to queue it
directly; `liked` and `saved-albums` work there too. Downloads run one at a
time in the queue on the right, with the same pacing, retries and safety stop as the
command line (see [Rate limits](#rate-limits)). Cancelling one takes it off the list (a
half-downloaded track is deleted). **Queue ▾** has **Pause all**, **Resume all** and
**Cancel all**: pausing holds off before the next track (one already downloading finishes
first, so its audio key isn't wasted) and stops the countdown.

The Downloads list is saved in `queue.json` in the config directory whenever it changes, so
it's the same after a restart: finished jobs stay listed until cleared, what was queued or
downloading carries on once you're logged in (tracks already there are skipped, so an
interrupted album picks up where it stopped), and paused stays paused.

Closing the window hides it in the system tray (the notification area on Windows) and
downloads carry on; click the tray icon to bring the window back, or use its menu to quit.
The icon's tooltip shows what's downloading. On Linux this needs a tray that speaks
StatusNotifierItem (KDE does; GNOME needs the AppIndicator extension). Without one, and on
macOS for now, closing the window quits (minimize it instead to keep downloading; see
[On a Mac](#on-a-mac)).

The window is drawn with the GPU (OpenGL). Where that isn't available or isn't up to it,
as in many virtual machines and remote desktop sessions, and on some Windows on ARM
computers, spotdl starts again by itself, drawing without the GPU.

Only one copy runs at a time (per config directory). Starting it again, from the app menu or
with `spotdl-gui <links>`, brings the running window to the front and queues any links given,
so two copies never request audio keys at once.

It logs in the same way and shares the saved login with `spotdl`. While it waits for the
browser, it can open the login page again, or cancel; for a browser on another computer,
it shows the link to open there and takes the address the login ended on. The command line's
download options and `SPOTDL_*` variables set its defaults (format, file name templates,
`--delay`, ...), and any links passed to it are queued once it's logged in, e.g.
`spotdl-gui https://open.spotify.com/album/…`. Files go to your Music folder unless you
pick another folder in the window (**Choose…**) or set `SPOTDL_OUTPUT`. On Linux the folder
picker is your desktop's own dialog, through the XDG desktop portal (standard on KDE and
GNOME); without a portal it falls back to `zenity`, and you can always type a path instead.

The folder and format you choose in the window are remembered in `gui.json` in the config
directory, e.g. `{ "output": "/home/me/Music", "format": "ogg" }`. Passing `-o`/`-f` or
setting `SPOTDL_OUTPUT`/`SPOTDL_FORMAT` overrides them for that run.

Songs, albums and playlists that are already in the download folder, in the chosen format,
show a greyed-out **Downloaded** button, since downloading them again would do nothing. (A
playlist counts once all its songs and its `.m3u8` file are there.)
Telling takes their track details (metadata, never audio keys): usually one or two lookups
per result, kept for the session, so switching folder or format only re-checks the disk.
Artists, and playlists of more than 300 tracks, aren't checked.

### Searching albums, artists and playlists

Out of the box, search finds songs the same way `spotdl search` does and lists the albums
and artists of the matching songs alongside them. To search albums, artists and playlists
directly, give spotdl your own Spotify developer app. Spotify's Web API refuses the login
session's own client, but it serves your app. It's free and takes a couple of minutes:

1. In the app, open **Settings** and choose **Open the dashboard**
   (<https://developer.spotify.com/dashboard>).
2. Create an app there: any name and description; for the redirect URI enter
   `http://127.0.0.1:8888/callback` (it isn't used); tick "Web API".
3. Copy the app's Client ID and Client secret into Settings and choose **Save**.

spotdl checks them with Spotify before saving, and says what's wrong if Spotify doesn't
accept them; once saved, search covers everything right away. **Remove** forgets them.

They're kept in `developer-app.json` in the config directory, readable by you alone, so
you can also write that file yourself (`{ "client_id": "…", "client_secret": "…" }`) or
set `SPOTDL_CLIENT_ID` and `SPOTDL_CLIENT_SECRET`, which take its place. The app is only
used to search, with its own token (no login). If the Web API fails, search falls back to
songs and says why.

## Rate limits

Spotify refuses audio keys (the per-track decryption keys) when they're requested too
quickly, so spotdl waits at least `--delay` seconds (default 50) between key requests
and shows a countdown while it waits. Already-downloaded tracks don't count. The gap holds
across runs too, and between the app and the command line: spotdl notes when it last asked
for a key (`last-key-request` in the config directory), so quitting and starting again
doesn't skip the wait. A lower `--delay` is faster but risks more refusals.

Each track normally costs exactly one key request: if the download itself fails, the
retry reuses the key and skips the wait. And before asking for a key, spotdl makes sure
the track can be saved: its folder can be written to, and the drive has room for it. When
the download folder can't take files (its drive isn't connected, is full or read-only),
the command line stops and the app pauses its queue until you choose **Resume all**,
rather than spending keys on tracks it couldn't save. When Spotify refuses a key, the track is retried
with growing back-off (`--retries`, default 5). If Spotify refuses or ignores 5 key
requests in a row, spotdl stops the whole run instead of asking again for every
remaining track, since that usually means the account is being rate-limited. Run it
again later; tracks you already have are skipped.

Interrupting with Ctrl-C is safe: tracks are written to a `.part` file first, flushed to
disk, and only then renamed into place, so a crash or power cut can't leave a broken file
that later runs would skip.

## On a Mac

Everything works the same on macOS. A few things are worth knowing, especially with the
music on an external drive:

- **Drive access.** The first time spotdl writes to an external drive, macOS asks whether
  it may access removable volumes: allow it. (For the command line, your terminal app is
  the one asking.) If you said no, turn it on in System Settings → Privacy & Security →
  Files & Folders. macOS may ask again after you reinstall the app.
- **Drive format.** macOS can read NTFS drives (common on drives that came from a Windows
  PC) but not write to them. Use a drive formatted as APFS, Mac OS Extended or exFAT.
- **When the drive isn't connected**, spotdl says so and stops (the app pauses its queue)
  instead of writing anywhere else: macOS mounts drives at `/Volumes/<name>`, and spotdl
  never creates that folder itself, since that would quietly fill the startup disk.
- **Sleep.** While it's downloading, spotdl keeps the Mac from falling asleep when idle
  (the display can still turn off) and out of App Nap, so it keeps its pace in the
  background. Once the queue is done or paused, the Mac can sleep as usual. Closing a
  laptop's lid still puts it to sleep.
- **Closing the window** quits the app; the Downloads list picks up where it left off next
  time. To keep downloading in the background, minimize the window (⌘M) or hide the app
  (⌘H), and click its Dock icon to come back.
- **Environment variables** set in your shell profile (`SPOTDL_*`) don't reach the app when
  it's started from the Dock or Launchpad. The window remembers its folder and format
  anyway, and the developer app is best set up in Settings.
- **Over SSH**, macOS may refuse remote sessions access to external drives. Turning on
  "Allow full disk access for remote users" (System Settings → General → Sharing → the ⓘ
  next to Remote Login) fixes that.

## On Windows

Everything works the same on Windows. A few things are worth knowing:

- **Closing the window** hides it in the notification area, and downloads carry on; click
  spotdl's icon to bring the window back, or right-click it to quit. Windows may tuck new
  icons away under the **^** arrow at first; drag spotdl's onto the taskbar to keep it in
  sight.
- **Windows on ARM** works the same: on an ARM computer, rustup sets up the ARM toolchain,
  and `install.ps1` builds for it.
- **Sleep.** While it's downloading, spotdl keeps Windows from going to sleep when idle (the
  screen can still turn off). Once the queue is done or paused, it can sleep as usual.
- **When the drive isn't connected** (say, an external `E:` drive), or it's full or
  write-protected, spotdl says so and stops (the app pauses its queue), without spending
  audio keys on tracks it couldn't save.
- **Controlled folder access.** If Windows Security's ransomware protection is on, it keeps
  unknown programs from writing to folders like Music, and spotdl says it can't save there.
  Allow `spotdl-gui.exe` and `spotdl.exe` (Windows Security → Virus & threat protection →
  Ransomware protection → Allow an app through Controlled folder access), or choose a
  folder it doesn't protect.
- **OneDrive.** If your Music folder is synced with OneDrive, everything downloaded there is
  uploaded too; choose another folder if you don't want that.
- **`~` means your user folder** on the command line too, in PowerShell and cmd:
  `spotdl liked -o ~\Music`.

## Upgrading from the old server version

The OpenSubsonic server is gone and `spotdl.json` is no longer used (no Spotify developer
app is needed anymore). To keep your existing login, move `credentials.json` into the
config directory (`mkdir -p ~/.config/spotdl && mv credentials.json ~/.config/spotdl/`),
or just run `spotdl login` again.
