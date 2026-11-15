//! The Downloads list, kept in `queue.json` in the config directory so it's the same
//! after a restart: what's queued (and what was downloading) carries on, finished jobs
//! stay listed until cleared, and paused stays paused.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use spotdl::cli::Format;

pub const FILE: &str = "queue.json";

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedQueue {
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub jobs: Vec<SavedJob>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedJob {
    pub title: String,
    /// What was asked for: a link, URI, `liked`... (parsed again when restored).
    pub input: String,
    pub output: PathBuf,
    /// The format's name, as `-f` takes it.
    pub format: String,
    /// How it ended; `None` while it's still to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<Finished>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Finished {
    pub failed: bool,
    pub status: String,
    #[serde(default)]
    pub detail: String,
}

impl SavedJob {
    pub fn format(&self) -> Option<Format> {
        Format::from_str(&self.format, true).ok()
    }
}

pub fn format_name(format: Format) -> String {
    format
        .to_possible_value()
        .map(|v| v.get_name().to_string())
        .unwrap_or_default()
}

impl SavedQueue {
    /// Reads the saved list from `dir`. A missing file means an empty list; a broken one
    /// is set aside as `queue.json.broken` (so it isn't lost) and the list starts empty.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("couldn't read {}: {e}", path.display());
                }
                return Self::default();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(queue) => queue,
            Err(e) => {
                log::warn!("ignoring {}: {e}", path.display());
                _ = std::fs::rename(&path, path.with_extension("json.broken"));
                Self::default()
            }
        }
    }

    /// Writes the list, replacing the old file in one step so it's never half-written.
    pub fn save(&self, dir: &Path) -> Result<()> {
        spotdl::auth::create_private_dir(dir)?;
        let path = dir.join(FILE);
        let temp = path.with_extension("json.tmp");
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        std::fs::write(&temp, json).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("saving {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_list_across_restarts() {
        let dir = std::env::temp_dir().join(format!("spotdl-queue-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            SavedQueue::load(&dir),
            SavedQueue::default(),
            "nothing saved yet"
        );

        let queue = SavedQueue {
            paused: true,
            jobs: vec![
                SavedJob {
                    title: "Album · Modal Soul".into(),
                    input: "https://open.spotify.com/album/3uNaLrScYulpXLRmQquP9I".into(),
                    output: "/music".into(),
                    format: format_name(Format::Ogg),
                    finished: None,
                },
                SavedJob {
                    title: "Liked Songs".into(),
                    input: "liked".into(),
                    output: "/music".into(),
                    format: format_name(Format::Flac),
                    finished: Some(Finished {
                        failed: false,
                        status: "12 tracks downloaded".into(),
                        detail: String::new(),
                    }),
                },
            ],
        };
        queue.save(&dir).unwrap();
        let loaded = SavedQueue::load(&dir);
        assert_eq!(loaded, queue);
        assert_eq!(loaded.jobs[1].format(), Some(Format::Flac));

        // a broken file is set aside, not lost
        std::fs::write(dir.join(FILE), "{ not json").unwrap();
        assert_eq!(SavedQueue::load(&dir), SavedQueue::default());
        assert!(dir.join("queue.json.broken").exists());
        _ = std::fs::remove_dir_all(&dir);
    }
}
