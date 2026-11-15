//! One running app per config directory (so per account). A second launch hands any links
//! it was given to the running app, which brings its window back and queues them; then
//! the second launch exits. Besides being what people expect, this keeps two downloaders
//! from requesting audio keys at the same time.
//!
//! The running app holds a lock on `gui.lock` in the config directory; the lock goes away
//! with the process, however it ends, so a crash never blocks the next launch. Later
//! launches reach it through a Unix socket, `gui.sock`, or else through a local TCP port:
//! on Windows, which has no Unix sockets in std, and where the socket's path would be too
//! long (they're limited to about 100 bytes). The port and a random secret are written to
//! `gui.port`, and only a launch that sends the secret is listened to. Either way, only
//! the config directory's owner can get in.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

pub enum Claim {
    /// No other app is running: this one is it. Later launches reach it through the
    /// listener (if listening worked).
    First(Option<Listener>),
    /// The running app was told to show its window, and given the links.
    Forwarded,
}

/// The first line of a message, so nothing else that connects is mistaken for a launch.
const GREETING: &str = "spotdl-gui show";

pub struct Listener {
    /// Held for as long as the app runs.
    _lock: std::fs::File,
    server: Option<Server>,
}

pub fn claim(dir: &Path, inputs: &[String]) -> Result<Claim> {
    use std::fs::{OpenOptions, TryLockError};

    let lock = spotdl::auth::create_private_dir(dir).and_then(|()| {
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("gui.lock"))?)
    });
    let lock = match lock {
        Ok(lock) => lock,
        Err(e) => {
            log::warn!("couldn't check for a running spotdl: {e:#}");
            return Ok(Claim::First(None));
        }
    };
    match lock.try_lock() {
        Ok(()) => {
            let server = listen(dir)
                .inspect_err(|e| log::warn!("later launches won't find this one: {e:#}"))
                .ok();
            Ok(Claim::First(Some(Listener {
                _lock: lock,
                server,
            })))
        }
        Err(TryLockError::WouldBlock) => {
            forward(dir, inputs)?;
            Ok(Claim::Forwarded)
        }
        Err(TryLockError::Error(e)) => {
            log::warn!("couldn't check for a running spotdl: {e}");
            Ok(Claim::First(None))
        }
    }
}

/// Tells the running app to show itself and queue `inputs`.
fn forward(dir: &Path, inputs: &[String]) -> Result<()> {
    // it may still be starting up, between taking the lock and listening
    let mut last_error = None;
    for _ in 0..30 {
        match connect(dir) {
            Ok((mut stream, greeting)) => {
                let mut message = format!("{greeting}\n");
                for input in inputs {
                    message.push_str(input);
                    message.push('\n');
                }
                stream.write_all(message.as_bytes())?;
                return Ok(());
            }
            Err(e) => last_error = Some(e),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!(
        "spotdl is already running but didn't answer ({}); it may be stuck, so it wasn't \
         started twice",
        last_error.map(|e| e.to_string()).unwrap_or_default()
    )
}

/// Being the running app. Hold on to it for as long as the app runs; dropping it lets
/// another launch take over.
pub struct Running {
    _lock: std::fs::File,
}

impl Listener {
    /// Calls `on_launch` with the links of every later launch, on a background thread.
    pub fn serve(self, on_launch: impl Fn(Vec<String>) + Send + 'static) -> Running {
        if let Some(server) = self.server {
            let spawned = std::thread::Builder::new()
                .name("launches".into())
                .spawn(move || server.serve(&on_launch));
            if let Err(e) = spawned {
                log::warn!("later launches won't find this one: {e}");
            }
        }
        Running { _lock: self._lock }
    }
}

/// Reads one launch's message, and passes on its links if it greets properly.
fn handle(stream: impl Read, greeting: &str, on_launch: &dyn Fn(Vec<String>)) {
    let mut message = String::new();
    // a launch sends a few links; ignore anything huge or garbled
    if stream.take(1 << 20).read_to_string(&mut message).is_err() {
        return;
    }
    let mut lines = message.lines();
    if lines.next() == Some(greeting) {
        on_launch(
            lines
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect(),
        );
    }
}

enum Server {
    #[cfg(unix)]
    Unix(unix::Server),
    Tcp(tcp::Server),
}

impl Server {
    fn serve(&self, on_launch: &dyn Fn(Vec<String>)) {
        match self {
            #[cfg(unix)]
            Self::Unix(server) => server.serve(on_launch),
            Self::Tcp(server) => server.serve(on_launch),
        }
    }
}

/// Listens for later launches: on a Unix socket where it can, else on a local port.
fn listen(dir: &Path) -> std::io::Result<Server> {
    #[cfg(unix)]
    match unix::listen(dir) {
        Ok(server) => {
            // a port from an earlier way of listening would only mislead later launches
            _ = std::fs::remove_file(dir.join(tcp::PORT_FILE));
            return Ok(Server::Unix(server));
        }
        Err(e) => log::debug!("no socket for later launches ({e}); using a local port"),
    }
    tcp::listen(dir).map(Server::Tcp)
}

/// Connects to the running app, however it listens, and says what to greet it with.
fn connect(dir: &Path) -> std::io::Result<(Box<dyn Write>, String)> {
    #[cfg(unix)]
    if let Ok((stream, greeting)) = unix::connect(dir) {
        return Ok((Box::new(stream), greeting));
    }
    let (stream, greeting) = tcp::connect(dir)?;
    Ok((Box::new(stream), greeting))
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    pub struct Server(UnixListener);

    /// Listens on `gui.sock`. Only the app holding the lock calls this, so a socket
    /// that's there already is from an app that's gone.
    pub fn listen(dir: &Path) -> io::Result<Server> {
        let path = dir.join("gui.sock");
        _ = std::fs::remove_file(&path);
        UnixListener::bind(path).map(Server)
    }

    /// Connects to the running app, and says what to greet it with.
    pub fn connect(dir: &Path) -> io::Result<(UnixStream, String)> {
        let stream = UnixStream::connect(dir.join("gui.sock"))?;
        Ok((stream, super::GREETING.to_string()))
    }

    impl Server {
        pub fn serve(&self, on_launch: &dyn Fn(Vec<String>)) {
            for stream in self.0.incoming() {
                let Ok(stream) = stream else { continue };
                _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                super::handle(stream, super::GREETING, on_launch);
            }
        }
    }
}

mod tcp {
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::Path;
    use std::time::Duration;

    pub const PORT_FILE: &str = "gui.port";

    pub struct Server {
        listener: TcpListener,
        greeting: String,
    }

    /// Listens on a free local port, and writes it to `gui.port` with a fresh secret.
    pub fn listen(dir: &Path) -> io::Result<Server> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        // any random string will do; this is a handy one already at hand
        let secret = oauth2::CsrfToken::new_random().secret().clone();
        spotdl::auth::write_private(
            &dir.join(PORT_FILE),
            format!("{port} {secret}\n").as_bytes(),
        )
        .map_err(io::Error::other)?;
        Ok(Server {
            listener,
            greeting: format!("{} {secret}", super::GREETING),
        })
    }

    /// Connects to the running app, and says what to greet it with.
    pub fn connect(dir: &Path) -> io::Result<(TcpStream, String)> {
        let text = std::fs::read_to_string(dir.join(PORT_FILE))?;
        let mut parts = text.split_whitespace();
        let (Some(port), Some(secret)) = (parts.next(), parts.next()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gui.port is garbled",
            ));
        };
        let port: u16 = port
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "gui.port is garbled"))?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
        Ok((stream, format!("{} {secret}", super::GREETING)))
    }

    impl Server {
        pub fn serve(&self, on_launch: &dyn Fn(Vec<String>)) {
            for stream in self.listener.incoming() {
                let Ok(stream) = stream else { continue };
                _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                super::handle(stream, &self.greeting, on_launch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spotdl-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_second_launch_reaches_the_first() {
        let dir = temp_dir("instance");
        let Ok(Claim::First(Some(listener))) = claim(&dir, &[]) else {
            panic!("the first launch should be the one running");
        };
        let (tx, rx) = mpsc::channel();
        let running = listener.serve(move |inputs| _ = tx.send(inputs));

        let links = vec![
            "https://open.spotify.com/album/x".to_string(),
            "liked".into(),
        ];
        assert!(matches!(claim(&dir, &links), Ok(Claim::Forwarded)));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), links);
        assert!(matches!(claim(&dir, &[]), Ok(Claim::Forwarded)));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Vec::<String>::new()
        );

        // whatever else connects is ignored
        let (mut other, _) = connect(&dir).unwrap();
        other.write_all(b"hello\nliked\n").unwrap();
        drop(other);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());

        // once it's gone (say, restarting), the next launch is the one running
        drop(running);
        assert!(matches!(claim(&dir, &[]), Ok(Claim::First(Some(_)))));
        _ = std::fs::remove_dir_all(&dir);
    }

    /// A config directory too deep for a socket's path: the local port takes over.
    #[test]
    fn a_long_config_directory_still_reaches_the_first() {
        let dir = temp_dir("instance-long").join("x".repeat(120));
        let Ok(Claim::First(Some(listener))) = claim(&dir, &[]) else {
            panic!("the first launch should be the one running");
        };
        let (tx, rx) = mpsc::channel();
        let _running = listener.serve(move |inputs| _ = tx.send(inputs));
        assert!(matches!(
            claim(&dir, &["liked".into()]),
            Ok(Claim::Forwarded)
        ));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            vec!["liked".to_string()]
        );
        _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Windows' way: a local port, and only launches that know the secret count.
    #[test]
    fn the_local_port_takes_only_launches_with_the_secret() {
        let dir = temp_dir("instance-tcp");
        std::fs::create_dir_all(&dir).unwrap();
        let server = tcp::listen(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || server.serve(&move |inputs| _ = tx.send(inputs)));

        let (mut stream, greeting) = tcp::connect(&dir).unwrap();
        stream
            .write_all(format!("{greeting}\nliked\n").as_bytes())
            .unwrap();
        drop(stream);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            vec!["liked".to_string()]
        );

        // the right port but no secret, or a wrong one: ignored
        for greeting in [GREETING.to_string(), format!("{GREETING} wrong")] {
            let (mut stream, _) = tcp::connect(&dir).unwrap();
            stream
                .write_all(format!("{greeting}\nliked\n").as_bytes())
                .unwrap();
        }
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("gui.port"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "others can read the secret");
        }
        _ = std::fs::remove_dir_all(&dir);
    }
}
