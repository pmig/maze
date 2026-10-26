//! The loopback redirect listener (RFC 8252 §7.3): a one-shot HTTP server on
//! `127.0.0.1:<random port>` that receives the authorization response from the
//! browser.

use std::net::Ipv4Addr;
use std::time::Duration;

use mazemail_types::Sensitive;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::AuthError;

/// Largest request head accepted; browsers send far less.
const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// How long a connection may take to send its request head. Browsers open
/// speculative connections that never send anything; those just time out.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long writing a response page may take.
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The parameters of the authorization response (RFC 6749 §4.1.2).
#[derive(Debug, Default)]
pub(crate) struct CallbackParams {
    pub(crate) code: Option<Sensitive<String>>,
    pub(crate) state: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) error_description: Option<String>,
}

/// A bound listener waiting for the redirect. Dropping it closes the port.
#[derive(Debug)]
pub(crate) struct LoopbackListener {
    listener: TcpListener,
    redirect_uri: String,
}

impl LoopbackListener {
    /// Binds `127.0.0.1` (never all interfaces) on a port the OS picks.
    pub(crate) async fn bind() -> Result<Self, AuthError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(AuthError::Loopback)?;
        let port = listener.local_addr().map_err(AuthError::Loopback)?.port();
        Ok(Self {
            listener,
            // No trailing slash, like Thunderbird: providers may match redirect URIs
            // exactly, and the registered form is `http://127.0.0.1` (port ignored).
            redirect_uri: format!("http://127.0.0.1:{port}"),
        })
    }

    /// The `redirect_uri` to send in the authorization and token requests.
    pub(crate) fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits until `deadline` for the browser's redirect. Other requests (a favicon,
    /// a speculative connection) are answered or dropped without ending the wait.
    ///
    /// Returns the callback parameters and the connection to answer once the outcome
    /// is known. The listener closes when this returns: the redirect is single-use.
    pub(crate) async fn wait(
        self,
        deadline: Instant,
    ) -> Result<(CallbackParams, PendingResponse), AuthError> {
        tokio::time::timeout_at(deadline, self.accept_callback())
            .await
            .map_err(|_| AuthError::Timeout)?
    }

    async fn accept_callback(self) -> Result<(CallbackParams, PendingResponse), AuthError> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        if peer.ip().is_loopback() {
                            connections.spawn(read_request(stream));
                        }
                    }
                    Err(err) if is_transient_accept_error(&err) => {
                        tracing::debug!(%err, "loopback accept failed, continuing");
                    }
                    Err(err) => return Err(AuthError::Loopback(err)),
                },
                Some(joined) = connections.join_next(), if !connections.is_empty() => {
                    let Ok(Some((stream, target))) = joined else { continue };
                    match parse_callback(&target) {
                        Some(params) => return Ok((params, PendingResponse { stream })),
                        None => {
                            // Not the redirect (e.g. /favicon.ico); answer and keep waiting.
                            write_response(stream, "404 Not Found", NOT_FOUND_PAGE).await;
                        }
                    }
                }
            }
        }
    }
}

fn is_transient_accept_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    )
}

/// Reads one request head and returns the stream and the request target of a `GET`.
async fn read_request(mut stream: TcpStream) -> Option<(TcpStream, String)> {
    let head = tokio::time::timeout(REQUEST_READ_TIMEOUT, async {
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..read]);
            // Read the whole head (a GET has no body): closing with unread data would
            // reset the connection and the browser might not show the page.
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(buf);
            }
            if buf.len() > MAX_REQUEST_BYTES {
                return None;
            }
        }
    })
    .await
    .ok()??;

    let line_end = head.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&head[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }
    if method != "GET" {
        write_response(stream, "405 Method Not Allowed", NOT_FOUND_PAGE).await;
        return None;
    }
    Some((stream, target.to_owned()))
}

/// The callback parameters if `target` is the redirect: path `/` with at least
/// `code`, `state` or `error` in the query.
fn parse_callback(target: &str) -> Option<CallbackParams> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/" {
        return None;
    }
    let mut params = CallbackParams::default();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        // The first occurrence wins; RFC 6749 forbids repeating parameters.
        match key.as_ref() {
            "code" if params.code.is_none() => {
                params.code = Some(Sensitive::new(value.into_owned()));
            }
            "state" if params.state.is_none() => params.state = Some(value.into_owned()),
            "error" if params.error.is_none() => params.error = Some(value.into_owned()),
            "error_description" if params.error_description.is_none() => {
                params.error_description = Some(value.into_owned());
            }
            _ => {}
        }
    }
    (params.code.is_some() || params.state.is_some() || params.error.is_some()).then_some(params)
}

/// The browser's redirect connection, kept open until the sign-in outcome is known so
/// the page it shows is accurate.
#[derive(Debug)]
pub(crate) struct PendingResponse {
    stream: TcpStream,
}

impl PendingResponse {
    /// Shows the success page.
    pub(crate) async fn succeed(self) {
        write_response(self.stream, "200 OK", &page(SUCCESS_TITLE, SUCCESS_TEXT)).await;
    }

    /// Shows the failure page with a short, fixed explanation for `error`. Provider
    /// text is never echoed into the page.
    pub(crate) async fn fail(self, error: &AuthError) {
        let reason = failure_reason(error);
        let text = format!("{reason} Return to Maze for details. You can close this tab.");
        write_response(self.stream, "200 OK", &page(FAILURE_TITLE, &text)).await;
    }
}

fn failure_reason(error: &AuthError) -> &'static str {
    match error {
        AuthError::StateMismatch => {
            "The response didn't match the sign-in Maze started, so it was ignored."
        }
        AuthError::AccessDenied(_) => "The sign-in was cancelled or permission was declined.",
        AuthError::AdminConsentRequired { .. } => {
            "Your organization's administrator has to approve Maze first."
        }
        AuthError::InvalidClient { .. } => "The provider rejected Maze's OAuth client settings.",
        AuthError::SingleTenantApp => {
            "The app registration only allows its own organization: set auth.microsoft.tenant \
             in Maze's settings to your tenant ID or domain."
        }
        AuthError::UnknownTenant { .. } => {
            "Microsoft doesn't know the organization (tenant) Maze asked for."
        }
        _ => "Something went wrong while signing in.",
    }
}

const SUCCESS_TITLE: &str = "You're signed in";
const SUCCESS_TEXT: &str = "You're signed in to Maze. You can close this tab.";
const FAILURE_TITLE: &str = "Maze couldn't sign you in";
const NOT_FOUND_PAGE: &str = "<!doctype html><title>Not found</title>";

/// A minimal self-contained page; `text` must be trusted (no escaping).
fn page(title: &str, text: &str) -> String {
    format!(
        "<!doctype html>\
<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Maze: {title}</title>\
<style>\
:root{{color-scheme:light dark}}\
body{{font:16px/1.5 -apple-system,BlinkMacSystemFont,\"Segoe UI\",system-ui,sans-serif;\
margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
background:Canvas;color:CanvasText}}\
main{{max-width:28rem;padding:2rem;text-align:center}}\
h1{{font-size:1.5rem;margin:0 0 .5rem}}\
p{{margin:0;opacity:.8}}\
</style></head>\
<body><main><h1>{title}</h1><p>{text}</p></main></body></html>"
    )
}

/// Writes a complete response and closes the connection. Failures only mean the
/// browser went away, which doesn't change the sign-in outcome.
async fn write_response(mut stream: TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\n\
Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
Cache-Control: no-store\r\n\
Referrer-Policy: no-referrer\r\n\
X-Content-Type-Options: nosniff\r\n\
Connection: close\r\n\r\n{body}",
        body.len()
    );
    let written = tokio::time::timeout(RESPONSE_WRITE_TIMEOUT, async {
        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        stream.shutdown().await
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::debug!(%err, "writing the sign-in page failed"),
        Err(_) => tracing::debug!("writing the sign-in page timed out"),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_the_callback() {
        let params = parse_callback("/?code=abc%2Fdef&state=xyz&scope=a+b").unwrap();
        assert_eq!(params.code.unwrap().expose(), "abc/def");
        assert_eq!(params.state.as_deref(), Some("xyz"));
        assert!(params.error.is_none());

        let params =
            parse_callback("/?error=access_denied&error_description=no+thanks&state=s").unwrap();
        assert_eq!(params.error.as_deref(), Some("access_denied"));
        assert_eq!(params.error_description.as_deref(), Some("no thanks"));
    }

    #[test]
    fn first_parameter_wins() {
        let params = parse_callback("/?state=one&state=two&code=c").unwrap();
        assert_eq!(params.state.as_deref(), Some("one"));
    }

    #[test]
    fn ignores_other_requests() {
        assert!(parse_callback("/favicon.ico").is_none());
        assert!(parse_callback("/").is_none());
        assert!(parse_callback("/?foo=bar").is_none());
        assert!(parse_callback("/other?code=abc&state=x").is_none());
    }

    #[tokio::test]
    async fn redirect_uri_is_loopback_without_trailing_slash() {
        let listener = LoopbackListener::bind().await.unwrap();
        let uri = listener.redirect_uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(!uri.ends_with('/'));
        assert!(listener.listener.local_addr().unwrap().ip().is_loopback());
    }

    #[tokio::test]
    async fn answers_stray_requests_and_waits_for_the_redirect() {
        let listener = LoopbackListener::bind().await.unwrap();
        let addr = listener.listener.local_addr().unwrap();
        let wait = tokio::spawn(listener.wait(Instant::now() + Duration::from_secs(10)));

        // A speculative connection that never sends anything.
        let _idle = TcpStream::connect(addr).await.unwrap();

        // A favicon request gets a 404 and doesn't end the wait.
        let mut favicon = TcpStream::connect(addr).await.unwrap();
        favicon
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        favicon.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");

        let mut redirect = TcpStream::connect(addr).await.unwrap();
        redirect
            .write_all(b"GET /?code=c&state=s HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let (params, pending) = wait.await.unwrap().unwrap();
        assert_eq!(params.code.unwrap().expose(), "c");
        pending.succeed().await;
        let mut response = String::new();
        redirect.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("You're signed in to Maze. You can close this tab."));
        assert!(response.contains("Cache-Control: no-store"));

        // Single-use: the port is closed now.
        assert!(TcpStream::connect(addr).await.is_err());
    }

    #[tokio::test]
    async fn times_out() {
        let listener = LoopbackListener::bind().await.unwrap();
        let result = listener
            .wait(Instant::now() + Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(AuthError::Timeout)));
    }

    #[tokio::test]
    async fn failure_page_does_not_echo_provider_text() {
        let listener = LoopbackListener::bind().await.unwrap();
        let addr = listener.listener.local_addr().unwrap();
        let wait = tokio::spawn(listener.wait(Instant::now() + Duration::from_secs(10)));
        let mut redirect = TcpStream::connect(addr).await.unwrap();
        redirect
            .write_all(b"GET /?state=s&error=x HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let (_, pending) = wait.await.unwrap().unwrap();
        pending.fail(&AuthError::StateMismatch).await;
        let mut response = String::new();
        redirect.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("Maze couldn't sign you in"), "{response}");
        assert!(response.contains("didn't match"));
    }
}
