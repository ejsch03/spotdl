//! Logging in to Spotify and keeping a connected session.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use librespot_core::{Session, SessionConfig, authentication::Credentials, cache::Cache};
use tokio::sync::mpsc;

use crate::login::BrowserLogin;

const CREDENTIALS_FILE: &str = "credentials.json";

#[derive(Debug, Clone)]
pub struct Auth {
    dir: PathBuf,
}

impl Auth {
    pub fn new(config_dir: Option<&Path>) -> Result<Self> {
        let dir = match config_dir {
            Some(dir) => dir.to_path_buf(),
            None => dirs::config_dir()
                .ok_or_else(|| anyhow!("couldn't find your config directory; pass --config-dir"))?
                .join("spotdl"),
        };
        Ok(Self { dir })
    }

    /// Where spotdl keeps its login and settings.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }

    fn cache(&self) -> Result<Cache> {
        create_private_dir(&self.dir)?;
        Cache::new(Some(&self.dir), None, None, None).context("opening the credentials cache")
    }

    pub fn is_logged_in(&self) -> Result<bool> {
        Ok(self.cache()?.credentials().is_some())
    }

    /// Connects with the saved login.
    pub async fn connect(&self) -> Result<Session> {
        let creds = self
            .cache()?
            .credentials()
            .ok_or_else(|| anyhow!("not logged in; run `spotdl login` first"))?;
        self.connect_with(creds)
            .await
            .context("Spotify rejected the saved login; run `spotdl login` to log in again")
    }

    /// Finishes a browser login (see [`BrowserLogin::finish`]), then connects and saves
    /// the login for next time.
    pub async fn finish_login(
        &self,
        login: BrowserLogin,
        pasted: mpsc::UnboundedReceiver<String>,
    ) -> Result<Session> {
        let token = login.finish(pasted).await?;
        self.connect_with(Credentials::with_access_token(token))
            .await
            .context("logging in to Spotify")
    }

    pub fn logout(&self) -> Result<bool> {
        match std::fs::remove_file(self.credentials_path()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).context("removing saved credentials"),
        }
    }

    async fn connect_with(&self, creds: Credentials) -> Result<Session> {
        let session = Session::new(SessionConfig::default(), Some(self.cache()?));
        // `true` (re)saves reusable credentials, so a login survives token rotation
        session.connect(creds, true).await?;
        restrict_permissions(&self.credentials_path());

        // The country and account type arrive in separate packets just after connecting;
        // track availability checks need the country, so give them a moment.
        for _ in 0..100 {
            if !session.country().is_empty() && session.get_user_attribute("type").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok(session)
    }
}

/// The logged-in session, shared by everything that talks to Spotify. When the
/// connection drops it's re-established once, for everyone, rather than each user of
/// the session opening its own.
pub struct Connection {
    auth: Auth,
    current: std::sync::Mutex<Session>,
    reconnecting: tokio::sync::Mutex<()>,
}

impl Connection {
    pub fn new(auth: Auth, session: Session) -> Self {
        Self {
            auth,
            current: std::sync::Mutex::new(session),
            reconnecting: tokio::sync::Mutex::new(()),
        }
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// The session as it is now, without reconnecting.
    pub fn current(&self) -> Session {
        self.current.lock().unwrap().clone()
    }

    /// A working session, reconnecting first if the connection dropped.
    pub async fn session(&self) -> Result<Session> {
        let session = self.current();
        if !session.is_invalid() {
            return Ok(session);
        }
        let _one_at_a_time = self.reconnecting.lock().await;
        // someone else may have reconnected while we waited
        let session = self.current();
        if !session.is_invalid() {
            return Ok(session);
        }
        log::info!("lost the connection to Spotify; reconnecting");
        let fresh = self.auth.connect().await?;
        *self.current.lock().unwrap() = fresh.clone();
        Ok(fresh)
    }
}

pub fn describe(session: &Session) -> String {
    let product = session
        .get_user_attribute("type")
        .unwrap_or_else(|| "unknown plan".into());
    format!("{} ({product}, {})", session.username(), session.country())
}

/// Creates spotdl's config directory, readable by its owner only (it holds the login).
pub fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Writes a file only its owner can read (it holds secrets), replacing it in one step.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let temp = path.with_extension("tmp");
    _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // private from the start, not just once written
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let write = || -> std::io::Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    };
    write().with_context(|| format!("saving {}", path.display()))
}

/// Makes a file readable by its owner only (it holds secrets).
pub(crate) fn restrict_permissions(_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _ = std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o600));
    }
}
