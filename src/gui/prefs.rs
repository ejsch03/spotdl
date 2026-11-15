//! The desktop app's settings file, `gui.json` in the config directory. It keeps what's
//! chosen in the window (the download folder and the format) for the next time, e.g.
//!
//! ```json
//! { "output": "/home/me/Music", "format": "ogg" }
//! ```
//!
//! Other keys in the file are left alone, so it's safe to edit by hand.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::{Map, Value};

use spotdl::cli::Format;

pub const FILE: &str = "gui.json";

pub struct Prefs {
    path: PathBuf,
    values: Map<String, Value>,
}

impl Prefs {
    /// Reads the settings in `dir`. A missing file means none were saved yet; a broken
    /// one is ignored (and replaced on the next save) rather than stopping the app.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let values = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(Value::Object(values)) => values,
                _ => {
                    log::warn!("ignoring {}: it isn't a JSON object", path.display());
                    Map::new()
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("couldn't read {}: {e}", path.display());
                }
                Map::new()
            }
        };
        Self { path, values }
    }

    /// The download folder as it was typed (`~` isn't expanded).
    pub fn output(&self) -> Option<&str> {
        self.values
            .get("output")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
    }

    pub fn format(&self) -> Option<Format> {
        Format::from_str(self.values.get("format")?.as_str()?, true).ok()
    }

    /// Remembers the download folder; an empty one means the default again.
    pub fn set_output(&mut self, dir: &str) {
        match dir.trim() {
            "" => _ = self.values.remove("output"),
            dir => _ = self.values.insert("output".into(), dir.into()),
        }
    }

    pub fn set_format(&mut self, format: Format) {
        if let Some(name) = format.to_possible_value() {
            self.values
                .insert("format".into(), name.get_name().to_string().into());
        }
    }

    /// Writes the file, replacing the old one in one step so it's never half-written.
    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            spotdl::auth::create_private_dir(dir)?;
        }
        let mut json = serde_json::to_vec_pretty(&self.values)?;
        json.push(b'\n');
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, json).with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("saving {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_folder_and_format() {
        let dir = std::env::temp_dir().join(format!("spotdl-prefs-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);

        let mut prefs = Prefs::load(&dir);
        assert_eq!(
            (prefs.output(), prefs.format()),
            (None, None),
            "nothing saved yet"
        );
        prefs.set_output("  ~/Music/Spotify ");
        prefs.set_format(Format::Opus);
        prefs.save().unwrap();

        let prefs = Prefs::load(&dir);
        assert_eq!(prefs.output(), Some("~/Music/Spotify"));
        assert_eq!(prefs.format(), Some(Format::Opus));

        // keys it doesn't know survive; bad values are ignored
        std::fs::write(
            dir.join(FILE),
            r#"{ "output": "", "format": "wav", "theme": "dark" }"#,
        )
        .unwrap();
        let mut prefs = Prefs::load(&dir);
        assert_eq!((prefs.output(), prefs.format()), (None, None));
        prefs.set_format(Format::Flac);
        prefs.save().unwrap();
        let saved = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(saved.contains(r#""theme": "dark""#) && saved.contains(r#""format": "flac""#));

        // a broken file doesn't stop the app
        std::fs::write(dir.join(FILE), "{ not json").unwrap();
        assert_eq!(Prefs::load(&dir).output(), None);
        _ = std::fs::remove_dir_all(&dir);
    }
}
