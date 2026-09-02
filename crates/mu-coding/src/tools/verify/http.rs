//! Loopback static file server for the web probe (mu-lg8j1).
//!
//! Headless Chrome fetches the artifact — and its sibling scripts,
//! styles and assets — over HTTP rather than `file://`, so the same URL
//! works for a local Chrome and for one on another host (reached through
//! a reverse ssh port-forward, see [`super::cdp::Launcher`]). Read-only,
//! confined to one directory (canonical-path check, so `..` and symlink
//! escapes 404), one request per connection, no dependencies beyond
//! tokio. It is not a general web server and never listens off loopback.
//!
//! The artifact's directory may be a repo root or `$HOME`, and loopback
//! is shared with every local process (and, over the ssh forward, with
//! the remote host's), so exposure is narrowed three ways: every URL
//! carries a per-server random token a co-tenant cannot guess; dotfiles
//! and dot-directories (`.env`, `.git`, `.ssh`) are never served; only
//! web-asset extensions ([`content_type`]) are served — source, config
//! and unknown files 404.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const HEAD_MAX_BYTES: usize = 8 * 1024;
/// Simultaneous connections the server will handle; a page fetching many
/// assets at once fits, a flood does not.
const MAX_CONNECTIONS: usize = 32;

pub struct StaticServer {
    port: u16,
    root: PathBuf,
    /// Random per-server URL prefix; requests without it 404.
    token: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl std::fmt::Debug for StaticServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticServer")
            .field("port", &self.port)
            .field("root", &self.root)
            .finish()
    }
}

impl StaticServer {
    /// Bind `127.0.0.1:0` and serve `root` (canonicalized) until dropped.
    pub async fn serve_dir(root: &Path) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let token = format!(
            "{:016x}{:016x}",
            rand::random::<u64>(),
            rand::random::<u64>()
        );
        let (tx, mut rx) = oneshot::channel::<()>();
        let served_root = root.clone();
        let served_token = token.clone();
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let Ok(permit) = permits.clone().acquire_owned().await else { break };
                            let root = served_root.clone();
                            let token = served_token.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(30),
                                    handle(stream, &root, &token),
                                )
                                .await;
                            });
                        }
                        Err(_) => break,
                    },
                }
            }
        });
        Ok(Self {
            port,
            root,
            token,
            shutdown: Some(tx),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// URL of a file directly under the root, as seen from a client on
    /// the same loopback (local Chrome, or the far end of a reverse
    /// port-forward that maps the same port number).
    pub fn url_for(&self, file_name: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}/{}",
            self.port,
            self.token,
            percent_encode(file_name)
        )
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let v = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Map a request path onto a servable file under `root`, or `None`
/// (⇒ 404): the first component must be `token`; no `..`, no dotfile or
/// dot-directory anywhere; only web-asset extensions; and the canonical
/// path must stay under `root` (symlink escapes 404).
pub fn resolve(root: &Path, token: &str, request_path: &str) -> Option<PathBuf> {
    let path = request_path.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(path)?;
    let decoded = String::from_utf8(decoded).ok()?;
    if decoded.contains('\0') {
        return None;
    }
    let mut comps = decoded.split('/').filter(|c| !c.is_empty() && *c != ".");
    if comps.next()? != token {
        return None;
    }
    let mut target = root.to_path_buf();
    for comp in comps {
        if comp == ".." || comp.starts_with('.') {
            return None;
        }
        target.push(comp);
    }
    if !servable_extension(&target) {
        return None;
    }
    let canonical = target.canonicalize().ok()?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return None;
    }
    Some(canonical)
}

/// Only files a browser page legitimately loads are served; anything
/// else under the directory (source, config, keys) is not.
pub fn servable_extension(path: &Path) -> bool {
    content_type(path) != "application/octet-stream"
}

pub fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs" | "cjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn handle(mut stream: TcpStream, root: &Path, token: &str) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > HEAD_MAX_BYTES {
                    break;
                }
            }
        }
    }
    let request_line = head
        .split(|b| *b == b'\n')
        .next()
        .map(|l| String::from_utf8_lossy(l).trim().to_string())
        .unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let response: Vec<u8> = match method {
        "GET" | "HEAD" => match resolve(root, token, path) {
            Some(file) => match tokio::fs::read(&file).await {
                Ok(body) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
                        content_type(&file),
                        body.len()
                    )
                    .into_bytes();
                    if method == "GET" {
                        r.extend_from_slice(&body);
                    }
                    r
                }
                Err(_) => simple(404, "not found"),
            },
            None => simple(404, "not found"),
        },
        _ => simple(405, "method not allowed"),
    };
    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

fn simple(code: u16, text: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {code} {text}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{text}",
        text.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, path: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        let code: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (code, body)
    }

    #[tokio::test]
    async fn serves_files_under_root_and_404s_outside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("game.html"), "<html>hi</html>").unwrap();
        std::fs::create_dir(dir.path().join("js")).unwrap();
        std::fs::write(dir.path().join("js").join("a b.js"), "console.log(1)").unwrap();
        let secret = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(secret.path(), "SECRET").unwrap();
        let server = StaticServer::serve_dir(dir.path()).await.unwrap();

        let t = server.token.clone();
        let (code, body) = get(server.port(), &format!("/{t}/game.html")).await;
        assert_eq!((code, body.as_str()), (200, "<html>hi</html>"));
        let (code, body) = get(server.port(), &format!("/{t}/js/a%20b.js?v=1")).await;
        assert_eq!((code, body.as_str()), (200, "console.log(1)"));
        assert_eq!(get(server.port(), &format!("/{t}/nope.html")).await.0, 404);
        // Without the token nothing is served, however well-known the name.
        assert_eq!(get(server.port(), "/game.html").await.0, 404);
        assert_eq!(get(server.port(), "/0000/game.html").await.0, 404);
        // Traversal, encoded traversal, and absolute paths all 404.
        for p in [
            "/../etc/passwd",
            "/%2e%2e/%2e%2e/etc/passwd",
            "/js/../../etc/passwd",
        ] {
            assert_eq!(get(server.port(), &format!("/{t}{p}")).await.0, 404, "{p}");
        }
        // Dotfiles / dot-directories and non-asset extensions are never served.
        std::fs::write(dir.path().join(".env"), "KEY=1").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "x").unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main(){}").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        for p in ["/.env", "/.git/config", "/main.rs", "/Cargo.toml"] {
            assert_eq!(get(server.port(), &format!("/{t}{p}")).await.0, 404, "{p}");
        }
        // A symlink out of the root is refused by the canonical check.
        std::os::unix::fs::symlink(secret.path(), dir.path().join("link.txt")).unwrap();
        assert_eq!(get(server.port(), &format!("/{t}/link.txt")).await.0, 404);
        // Directory is not a file.
        assert_eq!(get(server.port(), &format!("/{t}/js")).await.0, 404);
        assert_eq!(
            server.url_for("a b.html"),
            format!("http://127.0.0.1:{}/{}/a%20b.html", server.port(), t)
        );
    }

    #[test]
    fn content_types_and_decoding() {
        assert_eq!(
            content_type(Path::new("x.HTML")),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("x.wasm")), "application/wasm");
        assert_eq!(
            content_type(Path::new("x.unknown")),
            "application/octet-stream"
        );
        assert_eq!(percent_decode("a%20b").unwrap(), b"a b");
        assert!(percent_decode("a%2").is_none());
    }
}
