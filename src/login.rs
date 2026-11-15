//! Logging in through the browser: Spotify's OAuth authorization code flow, with PKCE.
//!
//! spotdl opens Spotify's login page in the browser. Once the user agrees, Spotify sends
//! the browser on to `http://127.0.0.1:8898/login?code=…`, where spotdl is listening,
//! and trades that code for an access token. When the browser runs on another computer
//! (say, spotdl runs over SSH), that last page can't load there; pasting its address
//! into spotdl works just as well, so no port forwarding is needed.
//!
//! The protocol itself (PKCE, the state check, the token request) is the `oauth2`
//! crate's, the same one librespot uses. Only the waiting is done here, so that it can
//! be cancelled, isn't thrown off by stray connections, and takes a pasted address too.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Spotify's own desktop client ID, which has [`REDIRECT_URI`] registered for this flow
/// (the same one librespot uses).
const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
pub const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";
const LISTEN_ON: &str = "127.0.0.1:8898";
/// What the session may do: play (that is, fetch) audio.
const SCOPE: &str = "streaming";

/// Where to set up a developer app, for the desktop app's full search.
pub const DEVELOPER_DASHBOARD: &str = "https://developer.spotify.com/dashboard";

type Client = BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

fn client() -> Result<Client> {
    Ok(BasicClient::new(ClientId::new(CLIENT_ID.into()))
        .set_auth_uri(AuthUrl::new(
            "https://accounts.spotify.com/authorize".into(),
        )?)
        .set_token_uri(TokenUrl::new(
            "https://accounts.spotify.com/api/token".into(),
        )?)
        .set_redirect_uri(RedirectUrl::new(REDIRECT_URI.into())?))
}

/// Opens `url` in the default browser. Returns false (and logs why) if that failed.
pub fn open_in_browser(url: &str) -> bool {
    match open::that_detached(url) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("couldn't open the browser: {e}");
            false
        }
    }
}

/// A browser login under way.
pub struct BrowserLogin {
    url: String,
    state: String,
    verifier: PkceCodeVerifier,
    listener: TcpListener,
}

impl BrowserLogin {
    /// Starts logging in. Takes the port Spotify sends the browser back to right away,
    /// so a port that's in use is reported before the browser opens.
    pub async fn start() -> Result<Self> {
        Self::start_on(LISTEN_ON).await
    }

    async fn start_on(address: &str) -> Result<Self> {
        let listener = TcpListener::bind(address).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                anyhow!(
                    "something else on this computer is using port {} (another spotdl \
                     logging in?), and Spotify sends the browser back there. Close it, then \
                     try again",
                    address.rsplit(':').next().unwrap_or(address)
                )
            } else {
                anyhow!(e).context(format!("listening on {address}"))
            }
        })?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state) = client()?
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(SCOPE.into()))
            .set_pkce_challenge(challenge)
            .url();
        Ok(Self {
            url: url.into(),
            state: state.secret().clone(),
            verifier,
            listener,
        })
    }

    /// Spotify's login page, to open in a browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for the browser to come back from Spotify, or for the address of the page
    /// it ended on to arrive through `pasted`, and returns an access token. Cancel it by
    /// dropping it; that also frees the port.
    pub async fn finish(self, pasted: mpsc::UnboundedReceiver<String>) -> Result<String> {
        let code = wait_for_code(self.listener, self.state, pasted).await?;
        let http = reqwest::Client::builder()
            // the token endpoint answers directly; following redirects isn't wanted here
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()?;
        let token = client()?
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(self.verifier)
            .request_async(&http)
            .await
            .map_err(|e| anyhow!("Spotify didn't complete the login ({e})"))?;
        Ok(token.access_token().secret().clone())
    }
}

/// Whether `text` could be the address the login ended on (to tell people right away
/// when they've pasted something else).
pub fn is_reply(text: &str) -> bool {
    parse_reply(text, None).is_some()
}

/// What came back from Spotify, through the browser or a pasted address.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Code(String),
    /// Spotify sent an error instead, e.g. `access_denied` when the user declined.
    Refused(String),
    /// It belongs to another login attempt: its `state` isn't this one's.
    Stale,
}

/// Reads Spotify's answer from an address (or just its path and query). With `state`,
/// a different `state` makes it [`Reply::Stale`]; a missing one is accepted only if
/// `strict` is false (for pasted addresses, which may have been cut short).
fn parse_reply(text: &str, state: Option<(&str, bool)>) -> Option<Reply> {
    let text = text
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '<' | '>'));
    let url = if text.starts_with('/') {
        reqwest::Url::parse(&format!("http://127.0.0.1{text}")).ok()?
    } else {
        reqwest::Url::parse(text).ok()?
    };
    let (mut code, mut error, mut sent_state) = (None, None, None);
    for (key, value) in url.query_pairs() {
        match &*key {
            "code" => code = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "state" => sent_state = Some(value.into_owned()),
            _ => {}
        }
    }
    let reply = match (code, error) {
        (_, Some(error)) => Reply::Refused(error),
        (Some(code), None) if !code.is_empty() => Reply::Code(code),
        _ => return None,
    };
    if let Some((expected, strict)) = state {
        match sent_state {
            Some(sent) if sent != expected => return Some(Reply::Stale),
            None if strict => return Some(Reply::Stale),
            _ => {}
        }
    }
    Some(reply)
}

/// Waits for the authorization code, from the browser or `pasted`.
async fn wait_for_code(
    listener: TcpListener,
    state: String,
    mut pasted: mpsc::UnboundedReceiver<String>,
) -> Result<String> {
    let state: Arc<str> = state.into();
    let (replies_tx, mut replies) = mpsc::unbounded_channel();
    let accepting = {
        let state = state.clone();
        async move {
            loop {
                match listener.accept().await {
                    // each connection on its own, so one that never sends anything (a
                    // browser opening spare connections) can't hold up the real one
                    Ok((stream, _)) => {
                        tokio::spawn(answer(stream, state.clone(), replies_tx.clone()));
                    }
                    Err(e) => {
                        log::debug!("couldn't accept a connection: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    };
    tokio::pin!(accepting);

    loop {
        let (reply, from_paste) = tokio::select! {
            () = &mut accepting => unreachable!("accepts forever"),
            Some(reply) = replies.recv() => (reply, false),
            Some(text) = pasted.recv() => match parse_reply(&text, Some((&state, false))) {
                Some(reply) => (reply, true),
                None => bail!(
                    "that isn't the address the login ended on (it has “code=” in it)"
                ),
            },
        };
        match reply {
            Reply::Code(code) => return Ok(code),
            Reply::Refused(error) if error == "access_denied" => {
                bail!("you didn't allow spotdl to log in")
            }
            Reply::Refused(error) => bail!("Spotify refused the login ({error})"),
            // an old browser tab; the right one may still come
            Reply::Stale if !from_paste => continue,
            Reply::Stale => bail!(
                "that address is from an earlier login attempt; use the page that opened \
                 last"
            ),
        }
    }
}

/// Answers one connection to the callback port, and passes on what Spotify sent.
async fn answer(mut stream: TcpStream, state: Arc<str>, replies: mpsc::UnboundedSender<Reply>) {
    let request = tokio::time::timeout(Duration::from_secs(10), read_request_target(&mut stream));
    let target = match request.await {
        Ok(Ok(target)) => target,
        Ok(Err(e)) => return log::debug!("unreadable request on the login port: {e:#}"),
        Err(_) => return, // never sent anything
    };
    let path = target.split('?').next().unwrap_or_default();
    let reply = (path == "/login")
        .then(|| parse_reply(&target, Some((&state, true))))
        .flatten();
    let (status, title, message) = match &reply {
        Some(Reply::Code(_)) => (
            "200 OK",
            "You're logged in",
            "spotdl is logged in to Spotify. You can close this tab and go back to spotdl.",
        ),
        Some(Reply::Refused(_)) => (
            "200 OK",
            "Not logged in",
            "spotdl wasn't allowed to log in. Go back to spotdl to try again.",
        ),
        Some(Reply::Stale) => (
            "400 Bad Request",
            "Old login page",
            "This page belongs to an earlier login attempt. Use the login page spotdl \
             opened last.",
        ),
        None => ("404 Not Found", "Not found", "Nothing here."),
    };
    let body = page(title, message);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Err(e) = stream.write_all(response.as_bytes()).await {
        log::debug!("couldn't answer the browser: {e}");
    }
    _ = stream.shutdown().await;
    if let Some(reply) = reply {
        _ = replies.send(reply);
    }
}

/// Reads an HTTP request's head and returns its target (`/login?code=…`). Reads the
/// whole head even though only the first line matters: closing a connection with
/// unread data in it resets it, and the browser would show an error instead of the page.
async fn read_request_target(stream: &mut TcpStream) -> Result<String> {
    let mut reader = BufReader::new(stream.take(16 * 1024));
    let mut first = String::new();
    reader.read_line(&mut first).await?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next(), parts.next());
    match (method, target) {
        (Some("GET"), Some(target)) => Ok(target.to_string()),
        _ => Err(anyhow!("not a GET request: {:?}", first.trim())).context("reading the request"),
    }
}

fn page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width\"><title>{title} · spotdl</title>\
         <body style=\"font: 17px/1.5 -apple-system, system-ui, sans-serif; text-align: center; \
         margin: 18vh 2em; color: #222; background: #fafafa\">\
         <h1 style=\"font-size: 26px\">{title}</h1><p>{message}</p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_replies() {
        let state = Some(("s1", true));
        assert_eq!(
            parse_reply("/login?code=abc&state=s1", state),
            Some(Reply::Code("abc".into()))
        );
        assert_eq!(
            parse_reply("/login?error=access_denied&state=s1", state),
            Some(Reply::Refused("access_denied".into()))
        );
        assert_eq!(
            parse_reply("/login?code=abc&state=s0", state),
            Some(Reply::Stale)
        );
        assert_eq!(parse_reply("/login?code=abc", state), Some(Reply::Stale));
        assert_eq!(parse_reply("/login?state=s1", state), None);
        assert_eq!(parse_reply("/favicon.ico", state), None);

        // pasted: a whole address, maybe quoted, percent-encoded, or cut short
        let pasted = Some(("s1", false));
        assert_eq!(
            parse_reply(
                "  \"http://127.0.0.1:8898/login?code=a%2Fb-c&state=s1\"\n",
                pasted
            ),
            Some(Reply::Code("a/b-c".into()))
        );
        assert_eq!(
            parse_reply("http://127.0.0.1:8898/login?code=abc", pasted),
            Some(Reply::Code("abc".into()))
        );
        assert_eq!(
            parse_reply("http://127.0.0.1:8898/login?code=abc&state=s0", pasted),
            Some(Reply::Stale)
        );
        assert!(is_reply("http://127.0.0.1:8898/login?code=abc&state=x"));
        assert!(!is_reply("abc"));
        assert!(!is_reply("https://open.spotify.com/album/x"));
    }

    async fn get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: */*\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    async fn login_on_any_port() -> (BrowserLogin, u16) {
        let login = BrowserLogin::start_on("127.0.0.1:0").await.unwrap();
        let port = login.listener.local_addr().unwrap().port();
        (login, port)
    }

    /// The parts that use the network run one after another: each takes a free port, and
    /// one checks that a port it gave up can be taken again, which a test running
    /// alongside could otherwise grab in between.
    #[tokio::test]
    async fn logs_in_through_the_browser() {
        waits_for_the_browser_past_stray_connections().await;
        takes_a_pasted_address_or_a_refusal().await;
        says_when_the_port_is_taken().await;
    }

    /// The browser's way back, with the kinds of stray connections browsers make.
    async fn waits_for_the_browser_past_stray_connections() {
        let (login, port) = login_on_any_port().await;
        assert!(
            login
                .url()
                .starts_with("https://accounts.spotify.com/authorize?")
        );
        assert!(login.url().contains("code_challenge_method=S256"));
        let state = login.state.clone();
        let (_paste, pasted) = mpsc::unbounded_channel();
        let waiting = tokio::spawn(wait_for_code(login.listener, state.clone(), pasted));

        // a spare connection that never sends anything doesn't hold things up
        let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        assert!(get(port, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        let stale = get(port, "/login?code=old&state=nope").await;
        assert!(stale.starts_with("HTTP/1.1 400"), "{stale}");

        let page = get(port, &format!("/login?code=c0de&state={state}")).await;
        assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
        assert!(page.contains("You can close this tab"));
        let code = tokio::time::timeout(Duration::from_secs(5), waiting).await;
        assert_eq!(code.unwrap().unwrap().unwrap(), "c0de");
    }

    async fn takes_a_pasted_address_or_a_refusal() {
        let (login, _) = login_on_any_port().await;
        let state = login.state.clone();
        let (paste, pasted) = mpsc::unbounded_channel();
        paste
            .send(format!(
                "http://127.0.0.1:8898/login?code=p4sted&state={state}"
            ))
            .unwrap();
        assert_eq!(
            wait_for_code(login.listener, state, pasted).await.unwrap(),
            "p4sted"
        );

        let (login, port) = login_on_any_port().await;
        let state = login.state.clone();
        let (_paste, pasted) = mpsc::unbounded_channel();
        let waiting = tokio::spawn(wait_for_code(login.listener, state.clone(), pasted));
        get(port, &format!("/login?error=access_denied&state={state}")).await;
        let error = waiting.await.unwrap().unwrap_err();
        assert_eq!(error.to_string(), "you didn't allow spotdl to log in");
    }

    async fn says_when_the_port_is_taken() {
        let (login, port) = login_on_any_port().await;
        let Err(e) = BrowserLogin::start_on(&format!("127.0.0.1:{port}")).await else {
            panic!("the port was free twice");
        };
        assert!(e.to_string().contains("another spotdl logging in"), "{e:#}");
        drop(login);
        // cancelling (dropping) the first login frees the port
        assert!(
            BrowserLogin::start_on(&format!("127.0.0.1:{port}"))
                .await
                .is_ok()
        );
    }
}
