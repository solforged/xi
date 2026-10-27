//! The egress proxy sandboxed processes reach the network through.
//!
//! One [`Listener`] per origin: a session's tools share one, and every
//! extension that declares `net` gets its own, so the socket a connection
//! arrives on identifies who connected. A listener speaks HTTP `CONNECT`
//! tunnels and absolute-form plain HTTP requests, parses the destination,
//! and hands a [`Connect`] to the turn that owns the session log. The turn
//! evaluates policy, records a `tool_decision`, and answers with a
//! [`Verdict`]. Nothing here decides anything; a connection with no turn to
//! ask gets `503`.
//!
//! On macOS the listener is loopback TCP and the Seatbelt profile allows
//! exactly that port. On Linux it is a Unix socket; the sandbox helper runs a
//! forwarder on loopback inside the network namespace that relays to it.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;
use xi_sandbox::Egress;

/// Longest request head the proxy reads before answering `431`.
const HEAD_LIMIT: usize = 64 * 1024;
/// Time allowed for the upstream TCP connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Loopback port the Linux forwarder listens on inside every sandbox.
#[cfg(target_os = "linux")]
const FORWARDER_PORT: u16 = 3128;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A connection waiting for a decision.
pub(crate) struct Connect {
    /// Destination host, without port or brackets.
    pub host: String,
    /// Destination port.
    pub port: u16,
    /// The extension whose listener accepted the connection, if any.
    pub extension: Option<String>,
    /// Whether the extension manifest lists this host. Always true for the
    /// tools listener.
    pub permitted: bool,
    /// Where the decision goes. Dropping it answers `502`.
    pub reply: oneshot::Sender<Verdict>,
}

/// The turn's answer for one connection.
pub(crate) enum Verdict {
    /// Connect to the destination and relay.
    Allow,
    /// Refuse with this message.
    Deny(String),
}

/// Delivers a parsed connection to whoever owns the session log. Returning
/// the connection means nobody is listening; the client gets `503`.
pub(crate) type Deliver = dyn Fn(Connect) -> Option<Connect> + Send + Sync;

/// A bound proxy socket. Accepting starts with [`Listener::start`], which
/// needs a Tokio runtime; binding does not.
pub(crate) struct Listener {
    egress: Egress,
    bound: Option<Bound>,
    task: Option<tokio::task::JoinHandle<()>>,
}

enum Bound {
    #[cfg(not(target_os = "linux"))]
    Tcp(std::net::TcpListener),
    #[cfg(target_os = "linux")]
    Unix(std::os::unix::net::UnixListener),
}

impl Listener {
    /// Binds a fresh proxy socket. `label` names the socket file on Linux.
    pub fn bind(label: &str) -> io::Result<Self> {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (label, n);
            let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
            listener.set_nonblocking(true)?;
            let port = listener.local_addr()?.port();
            Ok(Self {
                egress: Egress {
                    port,
                    socket: PathBuf::new(),
                },
                bound: Some(Bound::Tcp(listener)),
                task: None,
            })
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            // Socket paths are limited to 108 bytes, so the session directory
            // is too deep. Use the runtime directory, or a private directory
            // under the temporary directory.
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(|d| PathBuf::from(d).join("xi"))
                .unwrap_or_else(|| {
                    std::env::temp_dir().join(format!("xi-{}", nix::unistd::getuid()))
                });
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            let socket = dir.join(format!("egress-{}-{label}-{n}.sock", std::process::id()));
            let _ = std::fs::remove_file(&socket);
            let listener = std::os::unix::net::UnixListener::bind(&socket)?;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok(Self {
                egress: Egress {
                    port: FORWARDER_PORT,
                    socket,
                },
                bound: Some(Bound::Unix(listener)),
                task: None,
            })
        }
    }

    /// What a sandbox spec needs to reach this listener.
    pub fn egress(&self) -> &Egress {
        &self.egress
    }

    /// Starts accepting. Repeated calls do nothing. Must run on a runtime.
    pub fn start(
        &mut self,
        extension: Option<String>,
        hosts: Option<globset::GlobSet>,
        deliver: Box<Deliver>,
    ) {
        let Some(bound) = self.bound.take() else {
            return;
        };
        let deliver: std::sync::Arc<Deliver> = deliver.into();
        self.task = Some(tokio::spawn(async move {
            match bound {
                #[cfg(not(target_os = "linux"))]
                Bound::Tcp(listener) => {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                        return;
                    };
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            return;
                        };
                        let origin = Origin {
                            extension: extension.clone(),
                            hosts: hosts.clone(),
                            deliver: deliver.clone(),
                        };
                        tokio::spawn(serve(stream, origin));
                    }
                }
                #[cfg(target_os = "linux")]
                Bound::Unix(listener) => {
                    let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
                        return;
                    };
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            return;
                        };
                        let origin = Origin {
                            extension: extension.clone(),
                            hosts: hosts.clone(),
                            deliver: deliver.clone(),
                        };
                        tokio::spawn(serve(stream, origin));
                    }
                }
            }
        }));
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = std::fs::remove_file(&self.egress.socket);
        }
    }
}

struct Origin {
    extension: Option<String>,
    hosts: Option<globset::GlobSet>,
    deliver: std::sync::Arc<Deliver>,
}

/// Builds the case-insensitive host matcher for an extension's `net` list.
pub(crate) fn host_set(patterns: &[String]) -> Result<globset::GlobSet, globset::Error> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            globset::GlobBuilder::new(pattern)
                .case_insensitive(true)
                .build()?,
        );
    }
    builder.build()
}

/// A parsed proxy request.
struct Request {
    host: String,
    port: u16,
    /// `CONNECT`: answer `200` and tunnel. Otherwise forward the rewritten
    /// head, then relay.
    tunnel: bool,
    /// The request head to forward for plain HTTP, with hop-by-hop headers
    /// replaced by `Connection: close` so one proxy connection carries one
    /// destination.
    head: Vec<u8>,
}

async fn serve<S>(mut client: S, origin: Origin)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut buffer = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(end) = find_head_end(&buffer) {
            break end;
        }
        if buffer.len() >= HEAD_LIMIT {
            respond(
                &mut client,
                "431 Request Header Fields Too Large",
                "request head too large",
            )
            .await;
            return;
        }
        let mut chunk = [0u8; 4096];
        match client.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    };
    let rest = buffer.split_off(head_end);
    let request = match parse(&buffer) {
        Ok(request) => request,
        Err(message) => {
            respond(&mut client, "400 Bad Request", message).await;
            return;
        }
    };
    let permitted = origin
        .hosts
        .as_ref()
        .is_none_or(|hosts| hosts.is_match(&request.host));
    let (reply, verdict) = oneshot::channel();
    let connect = Connect {
        host: request.host.clone(),
        port: request.port,
        extension: origin.extension.clone(),
        permitted,
        reply,
    };
    if (origin.deliver)(connect).is_some() {
        respond(
            &mut client,
            "503 Service Unavailable",
            "no session is serving this extension",
        )
        .await;
        return;
    }
    match verdict.await {
        Ok(Verdict::Allow) => {}
        Ok(Verdict::Deny(message)) => {
            respond(&mut client, "403 Forbidden", &message).await;
            return;
        }
        Err(_) => {
            respond(&mut client, "502 Bad Gateway", "session closed").await;
            return;
        }
    }
    let upstream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((request.host.as_str(), request.port)),
    )
    .await;
    let mut upstream = match upstream {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            respond(
                &mut client,
                "502 Bad Gateway",
                &format!(
                    "cannot connect to {}:{}: {error}",
                    request.host, request.port
                ),
            )
            .await;
            return;
        }
        Err(_) => {
            respond(
                &mut client,
                "504 Gateway Timeout",
                &format!("connecting to {}:{} timed out", request.host, request.port),
            )
            .await;
            return;
        }
    };
    if request.tunnel {
        if client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
    } else if upstream.write_all(&request.head).await.is_err() {
        return;
    }
    if !rest.is_empty() && upstream.write_all(&rest).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

async fn respond<S: AsyncWrite + Unpin>(client: &mut S, status: &str, message: &str) {
    let body = format!("xi egress proxy: {message}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(response.as_bytes()).await;
    let _ = client.shutdown().await;
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

/// Parses a request head. `CONNECT host:port` tunnels; `METHOD http://host/…`
/// is plain HTTP. Origin-form requests have no destination and are refused.
fn parse(head: &[u8]) -> Result<Request, &'static str> {
    let text = std::str::from_utf8(head).map_err(|_| "request head is not UTF-8")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed request line");
    };
    if !version.starts_with("HTTP/1.") {
        return Err("only HTTP/1.x is supported");
    }
    if method == "CONNECT" {
        let (host, port) = split_authority(target, 443)?;
        return Ok(Request {
            host,
            port,
            tunnel: true,
            head: Vec::new(),
        });
    }
    let Some(after_scheme) = target.strip_prefix("http://") else {
        if target.starts_with('/') {
            return Err("origin-form request; use CONNECT or an absolute URL");
        }
        return Err("only http:// targets and CONNECT are supported");
    };
    let authority = after_scheme.split('/').next().unwrap_or_default();
    let (host, port) = split_authority(authority, 80)?;
    let mut rewritten = String::with_capacity(text.len() + 32);
    rewritten.push_str(line);
    rewritten.push_str("\r\n");
    for header in lines {
        if header.is_empty() {
            continue;
        }
        let name = header.split(':').next().unwrap_or_default().trim();
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("keep-alive")
        {
            continue;
        }
        rewritten.push_str(header);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str("Connection: close\r\n\r\n");
    Ok(Request {
        host,
        port,
        tunnel: false,
        head: rewritten.into_bytes(),
    })
}

/// Splits `host[:port]`, with IPv6 literals in brackets.
fn split_authority(authority: &str, default_port: u16) -> Result<(String, u16), &'static str> {
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']').ok_or("unterminated IPv6 literal")?;
        (host, rest.strip_prefix(':'))
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host.is_empty() {
        return Err("empty host");
    }
    let port = match port {
        Some(port) if !port.is_empty() => port.parse().map_err(|_| "invalid port")?,
        _ => default_port,
    };
    Ok((host.to_ascii_lowercase(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_and_absolute_form_parse_and_origin_form_is_refused() {
        let request =
            parse(b"CONNECT Example.org:8443 HTTP/1.1\r\nHost: example.org\r\n\r\n").unwrap();
        assert_eq!(
            (request.host.as_str(), request.port, request.tunnel),
            ("example.org", 8443, true)
        );
        let request = parse(b"CONNECT [::1]:443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!((request.host.as_str(), request.port), ("::1", 443));
        let request = parse(
            b"GET http://example.org/a?b=c HTTP/1.1\r\nHost: example.org\r\nProxy-Connection: Keep-Alive\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            (request.host.as_str(), request.port, request.tunnel),
            ("example.org", 80, false)
        );
        assert_eq!(
            request.head,
            b"GET http://example.org/a?b=c HTTP/1.1\r\nHost: example.org\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(
            parse(b"GET http://user@example.org:81/ HTTP/1.1\r\n\r\n")
                .unwrap()
                .port,
            81
        );
        assert!(parse(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").is_err());
        assert!(parse(b"GET https://example.org/ HTTP/1.1\r\n\r\n").is_err());
        assert!(parse(b"CONNECT example.org:x HTTP/1.1\r\n\r\n").is_err());
        assert!(parse(b"CONNECT example.org:443 HTTP/2\r\n\r\n").is_err());
    }

    #[test]
    fn host_sets_match_case_insensitively() {
        let hosts = host_set(&["*.github.com".into(), "api.example.org".into()]).unwrap();
        assert!(hosts.is_match("API.GitHub.com"));
        assert!(hosts.is_match("api.example.org"));
        assert!(!hosts.is_match("github.com"));
        assert!(!hosts.is_match("example.org"));
    }
}
