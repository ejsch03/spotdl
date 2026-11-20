//! spotdl: download songs, albums, playlists and artists from Spotify.
//!
//! The `spotdl` command line and the `spotdl-gui` desktop app are both thin front ends
//! over [`engine::Downloader`], which owns everything that touches the account (pacing,
//! retries, stopping when Spotify pushes back), so both behave exactly the same.

pub mod app;
pub mod audio;
pub mod auth;
pub mod awake;
pub mod cli;
pub mod downloaded;
pub mod engine;
pub mod input;
pub mod login;
pub mod meta;
pub mod output;
pub mod plex;
pub mod search;
pub mod ui;

/// The async runtime both front ends run on. Its work is network-bound and tracks
/// download one at a time, so a couple of workers is plenty; CPU-heavy steps (tagging,
/// decoding thumbnails) go to the blocking pool anyway. Kept small on purpose: every
/// worker gets its own malloc arena, and each arena holds on to the peak of the
/// multi-MB track buffers that passed through it, so one worker per core let memory
/// climb by a track's worth each time a download landed on a fresh worker.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
}

/// Makes glibc hand every allocation of 1 MiB or more straight to the OS and return it
/// the moment it's freed. Call first thing in `main`, before any thread starts.
///
/// Each download holds the whole track in memory (several MB) once or twice. glibc
/// normally raises its mmap threshold after the first such buffer is freed, then serves
/// later ones from its per-thread arenas, where tracks of varying sizes fragment the
/// free space: measured over 63 real tracks, memory crept up about 1 MB per track with
/// no sign of levelling off. With a fixed threshold, track buffers never enter the
/// arenas, and a simulated run of 30 tracks grew by 0 MB. The cost is a few mmap calls
/// per track. Other platforms' allocators don't work this way, so this is glibc-only.
pub fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        const MMAP_THRESHOLD: libc::c_int = 1 << 20;
        // SAFETY: mallopt only changes allocator parameters, and nothing else is running
        // yet. It can only fail for values above 32 MiB, and if it did, glibc would just
        // keep its default behaviour.
        unsafe {
            libc::mallopt(libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD);
        }
    }
}
