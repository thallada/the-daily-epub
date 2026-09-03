//! M7/M8 integration: drive the real `daily-epub serve` binary over TCP.
//!
//! The crate has no library target, so this file cannot link the crate's modules
//! (the endpoint-level tests over `server::router` live in `src/server.rs`).
//! What it *can* do — and what nothing else covers — is prove that the shipped
//! binary boots from `DAILY_EPUB_*` configuration, migrates its database, and
//! answers the spec's routes on a real socket, exactly as the systemd unit runs it
//! (spec §3.12, §3.15).
//!
//! Only std + dev-dependencies are available here, so the HTTP client below is a
//! hand-rolled `GET`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Shared fixture vector, mirrored by the unit tests in `src/server.rs`:
/// `hex(hmac_sha256("test-secret", "2026-08-15/42/up"))[..16]`.
const SECRET: &str = "test-secret";
const TOKEN_UP_ARTICLE_42: &str = "3b314cf7e6d8f50f";
/// base64("opds:hunter2")
const BASIC_AUTH: &str = "b3BkczpodW50ZXIy";

struct Server {
    child: Child,
    port: u16,
    dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(basic_auth: bool) -> Server {
        let dir = tempfile::tempdir().expect("tempdir");
        let xtc_dir = dir.path().join("xtc");
        std::fs::create_dir_all(&xtc_dir).expect("xtc dir");
        std::fs::create_dir_all(dir.path().join("bookorbit")).expect("epub dir");
        let log = std::fs::File::create(dir.path().join("server.log")).expect("log file");

        let port = free_port();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_daily-epub"));
        cmd.arg("serve")
            // cwd must not contain the repo's config.toml.
            .current_dir(dir.path())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("RUST_LOG", "warn")
            .env("DAILY_EPUB_DATABASE_PATH", dir.path().join("daily-epub.db"))
            .env("DAILY_EPUB_SERVER__BIND", format!("127.0.0.1:{port}"))
            .env("DAILY_EPUB_SERVER__HMAC_SECRET", SECRET)
            .env(
                "DAILY_EPUB_SERVER__PUBLIC_URL",
                format!("http://127.0.0.1:{port}"),
            )
            .env("DAILY_EPUB_PUBLISH__XTC_DIR", &xtc_dir)
            .env("DAILY_EPUB_PUBLISH__EPUB_DIR", dir.path().join("bookorbit"))
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        if basic_auth {
            cmd.env("DAILY_EPUB_SERVER__BASIC_AUTH_USER", "opds")
                .env("DAILY_EPUB_SERVER__BASIC_AUTH_PASS", "hunter2");
        }
        let child = cmd.spawn().expect("spawning daily-epub serve");

        let server = Server { child, port, dir };
        server.wait_until_ready();
        server
    }

    fn xtc_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("xtc")
    }

    fn epub_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("bookorbit")
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(res) = try_get(self.port, "/healthz", None)
                && res.status == 200
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let log = std::fs::read_to_string(self.dir.path().join("server.log")).unwrap_or_default();
        panic!(
            "daily-epub serve never became healthy on port {}:\n{log}",
            self.port
        );
    }

    fn get(&self, path: &str) -> HttpResponse {
        try_get(self.port, path, None).expect("request failed")
    }

    fn get_auth(&self, path: &str, credentials: &str) -> HttpResponse {
        try_get(self.port, path, Some(credentials)).expect("request failed")
    }

    fn add_admin(&self, username: &str, password: &str) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_daily-epub"))
            .args(["users", "add", username, "--admin", "--password-stdin"])
            .current_dir(self.dir.path())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env(
                "DAILY_EPUB_DATABASE_PATH",
                self.dir.path().join("daily-epub.db"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawning users add");
        child
            .stdin
            .take()
            .expect("users add stdin")
            .write_all(format!("{password}\n").as_bytes())
            .expect("writing users add password");
        assert!(child.wait().expect("waiting for users add").success());
    }

    fn post_form(&self, path: &str, body: &str) -> HttpResponse {
        try_request(
            self.port,
            "POST",
            path,
            &format!(
                "Content-Type: application/x-www-form-urlencoded\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n",
                body.len()
            ),
            body,
        )
        .expect("request failed")
    }

    fn get_cookie(&self, path: &str, cookie: &str) -> HttpResponse {
        try_request(self.port, "GET", path, &format!("Cookie: {cookie}\r\n"), "")
            .expect("request failed")
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: String,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        let name = format!("{}:", name.to_ascii_lowercase());
        self.headers
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with(&name))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim())
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// A minimal HTTP/1.1 `GET`; `None` when the connection could not be made.
fn try_get(port: u16, path: &str, credentials: Option<&str>) -> Option<HttpResponse> {
    let auth = credentials
        .map(|c| format!("Authorization: Basic {c}\r\n"))
        .unwrap_or_default();
    try_request(port, "GET", path, &auth, "")
}

fn try_request(
    port: u16,
    method: &str,
    path: &str,
    extra_headers: &str,
    body: &str,
) -> Option<HttpResponse> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{extra_headers}\r\n{body}"
    );
    stream.write_all(request.as_bytes()).ok()?;
    stream.flush().ok()?;
    // No half-close here: hyper drops a connection whose peer has shut down its
    // write side before the response is written. `Connection: close` is enough.

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(HttpResponse {
        status,
        headers: head.to_string(),
        body: body.to_string(),
    })
}

#[test]
fn binary_serves_health_issues_and_rating_endpoints() {
    let server = Server::start(false);

    for path in ["/", "/issues", "/feed.xml", "/login"] {
        assert_eq!(server.get(path).status, 200, "{path}");
    }
    let dashboard = server.get("/dashboard");
    assert_eq!(dashboard.status, 302);
    assert_eq!(
        dashboard.header("location"),
        Some("/login?next=%2Fdashboard")
    );

    server.add_admin("admin", "correct horse battery");
    let login = server.post_form(
        "/login",
        "username=admin&password=correct+horse+battery&next=%2Faccount",
    );
    assert_eq!(login.status, 303, "{login:?}");
    let cookie = login
        .header("set-cookie")
        .expect("login cookie")
        .split(';')
        .next()
        .expect("cookie pair");
    let account = server.get_cookie("/account", cookie);
    assert_eq!(account.status, 200, "{account:?}");
    assert!(account.body.contains("admin"));

    let res = server.get("/healthz");
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "ok");

    // Migrations ran on startup, so `issues.json` answers with an empty list.
    let res = server.get("/issues.json");
    assert_eq!(res.status, 200);
    assert_eq!(
        res.header("content-type"),
        Some("application/json"),
        "{}",
        res.headers
    );
    assert_eq!(res.body.trim(), "[]");

    // A tampered token never reaches the database.
    let res = server.get("/r/2026-08-15/42/up?t=deadbeefdeadbeef");
    assert_eq!(res.status, 403);
    assert!(res.body.contains("Invalid link"), "{}", res.body);

    // The shared HMAC vector verifies, but article 42 does not exist here.
    let res = server.get(&format!("/r/2026-08-15/42/up?t={TOKEN_UP_ARTICLE_42}"));
    assert_eq!(res.status, 404, "token vector no longer verifies: {res:?}");
    assert!(res.body.contains("Unknown article"), "{}", res.body);

    // The confirmation pages stay e-ink sized and self contained (§3.9).
    assert!(res.body.len() < 1024, "page is {} bytes", res.body.len());
    assert!(!res.body.contains("<link"));

    // Malformed date / vote are rejected before any lookup.
    assert_eq!(
        server
            .get(&format!("/r/nope/42/up?t={TOKEN_UP_ARTICLE_42}"))
            .status,
        400
    );
    assert_eq!(
        server
            .get(&format!("/r/2026-08-15/42/maybe?t={TOKEN_UP_ARTICLE_42}"))
            .status,
        400
    );
    assert_eq!(server.get("/nope").status, 404);
}

#[test]
fn binary_serves_opds_and_files_behind_basic_auth() {
    let server = Server::start(true);
    write(
        &server.epub_dir().join("The Daily EPUB - 2026-08-15.epub"),
        "STANDARD",
    );
    write(
        &server
            .epub_dir()
            .join("The Daily EPUB - 2026-08-15 (X4).epub"),
        "X4EPUB",
    );
    write(
        &server
            .xtc_dir()
            .join("The Daily EPUB - 2026-08-15 (X4).xtch"),
        "XTCH",
    );
    write(&server.dir.path().join("secret"), "top secret");

    // No credentials → challenge.
    let res = server.get("/opds/daily.xml");
    assert_eq!(res.status, 401);
    assert_eq!(
        res.header("www-authenticate"),
        Some("Basic realm=\"The Daily EPUB\", charset=\"UTF-8\"")
    );
    assert_eq!(
        server.get_auth("/opds/daily.xml", "bm9wZTpub3Bl").status,
        401
    );

    // Correct credentials → a feed built from the publish dir, typed as OPDS.
    let res = server.get_auth("/opds/daily.xml", BASIC_AUTH);
    assert_eq!(res.status, 200);
    assert!(
        res.header("content-type")
            .unwrap_or_default()
            .starts_with("application/atom+xml"),
        "{}",
        res.headers
    );
    assert!(res.body.contains("opds-spec.org/acquisition"));
    // Both editions, and only as `application/epub+zip` — the one acquisition
    // type CrossPoint's parser accepts.
    assert_eq!(res.body.matches("<entry>").count(), 2, "{}", res.body);
    assert_eq!(
        res.body.matches("type=\"application/epub+zip\"").count(),
        2,
        "{}",
        res.body
    );
    assert!(!res.body.contains(".xtch"), "{}", res.body);

    // The acquisition link in the feed resolves to the file itself.
    let res = server.get_auth(
        "/files/epub/The%20Daily%20EPUB%20-%202026-08-15%20%28X4%29.epub",
        BASIC_AUTH,
    );
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "X4EPUB");
    assert_eq!(res.header("content-type"), Some("application/epub+zip"));

    // XTC stays reachable by URL for sideloading, just unlisted.
    let res = server.get_auth(
        "/files/xtc/The%20Daily%20EPUB%20-%202026-08-15%20%28X4%29.xtch",
        BASIC_AUTH,
    );
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "XTCH");
    assert_eq!(res.header("content-type"), Some("application/octet-stream"));

    // Path traversal, percent-encoded so the URL parser cannot normalize it away.
    for attack in [
        "/files/xtc/..%2Fsecret",
        "/files/xtc/%2e%2e%2fsecret",
        "/files/xtc/%2Fetc%2Fpasswd",
    ] {
        let res = server.get_auth(attack, BASIC_AUTH);
        assert_eq!(res.status, 400, "{attack} was not rejected: {res:?}");
        assert!(!res.body.contains("top secret"));
    }
    // Unauthenticated traversal is refused before the path is even looked at.
    assert_eq!(server.get("/files/xtc/..%2Fsecret").status, 401);

    // /healthz stays open so the reverse proxy can probe it.
    assert_eq!(server.get("/healthz").status, 200);
}

fn write(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
}
