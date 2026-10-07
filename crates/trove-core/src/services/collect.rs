//! Local collect service: a tiny HTTP server on 127.0.0.1 that lands files
//! into the *inbox* directory, where the app's watcher picks them up and
//! imports them.
//!
//! Endpoints (the future browser extension speaks the first two):
//! - `GET  /ping` → `trove ok` (liveness check)
//! - `GET  /health` → the process metrics snapshot as JSON: version, uptime,
//!   whether a library is open and its asset count, import throughput, the
//!   search outbox's backlog, thumbnail-cache hit rate, slow-query counts
//!   ([`crate::metrics::snapshot`])
//! - `POST /add?filename=NAME&source=URL&collection=ID&focus=1` — body is the
//!   raw file bytes
//!   (`curl --data-binary @img.png 'http://127.0.0.1:P/add?filename=a.png'`)
//! - `POST /fetch` — JSON body `{"url": "…", "name": "…", "source": "…",
//!   "referer": "…", "reject_html": true, "collection": "ID", "focus": true}`
//!   downloads the URL server-side (ureq + rustls). The request carries a
//!   browser-like User-Agent and, when the caller knows it, the capturing page
//!   as Referer — that is the point of the fallback: the extension lands here
//!   when its own request was refused by hotlink protection. `reject_html`
//!   refuses a text/html answer (a webpage, not a file) instead of landing one
//!   in the library.
//! - `GET /collections` — the open library's collection tree as JSON, the
//!   target list the extension draws its save-into menu from. Answers 503 with
//!   a reason while no library is recorded, so a caller can fall back to
//!   "wherever imports go".
//!
//! `collection` is a collection id and `focus` asks for the window to come
//! forward; both are recorded on the file, not acted on here — see below.
//!
//! Every saved file gets a `<name>.meta.json` sidecar recording its source URL
//! and, when the caller named one, its target collection; the importer applies
//! both to the fresh asset and deletes the sidecar. The server writes no
//! database at all — the only read it makes is a read-only connection over the
//! open library for `/collections`, which is what lets it run on its own
//! thread while the UI stays single-threaded.
//!
//! All responses carry `Access-Control-Allow-Origin: *` and OPTIONS
//! preflights are answered: the browser extension calls us from
//! `moz-extension://` / `chrome-extension://` origins, and without CORS its
//! `POST /add` (octet-stream) preflight would never pass. The listener is
//! 127.0.0.1-only and the worst case is a file landing in the inbox, so the
//! wildcard origin is acceptable here.
//!
//! Three things the server is careful about, because the inbox is watched and
//! the library *links* what lands there:
//!
//! - Requests run on a fixed worker pool with a bounded queue, and each socket
//!   has an idle timeout, so a client that opens connections and stalls cannot
//!   grow the process without limit.
//! - A body is streamed to disk as it arrives ([`Landing`]), never collected
//!   into a `Vec` — a 512 MB upload costs a 512 MB file and no more.
//! - The file appears in the inbox only via `rename`, so the drain can never
//!   import a half-written file. A truncated import would record a content
//!   hash its own bytes no longer match, and the library links rather than
//!   copies, so that asset would stay wrong forever.

use std::io::{BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::config::AppConfig;
use crate::error::Error;

/// Default listen port for the collect service.
pub const DEFAULT_PORT: u16 = 23916;

/// Set by a save that asked for Trove's window to come forward. A worker
/// thread cannot touch a window, so the request waits here and the inbox pump
/// spends it — that pump runs on the UI thread and already wakes for every
/// collected file, so the window rises when the file is really being taken in,
/// not when the upload starts.
static WAKE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Take the standing "bring Trove forward" request, clearing it.
pub fn take_wake_request() -> bool {
    WAKE_REQUESTED.swap(false, Ordering::Relaxed)
}

/// What the caller said about a captured file: where it came from, and which
/// collection it should be filed into once it is an asset. Both travel on the
/// file's sidecar because the server has no database to write to.
#[derive(Default)]
struct CollectMeta {
    source: Option<String>,
    /// Canonical collection id, already validated at the boundary.
    collection: Option<String>,
}

/// Largest accepted upload/download (512 MB).
const MAX_BODY: u64 = 512 * 1024 * 1024;

/// Largest JSON request body ([`/fetch`] carries only a URL and a name).
const MAX_JSON_BODY: u64 = 64 * 1024;

/// User-Agent for server-side downloads. Hotlink protection routinely
/// refuses bare client UAs outright; presenting a common browser string is
/// the difference between a fallback that works and one that always 403s.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Size of the chunk bodies are streamed in.
const PUMP_CHUNK: usize = 64 * 1024;

/// Concurrent request handlers. The service is local and low-traffic, and a
/// download occupies its worker for its whole duration.
const WORKERS: usize = 4;

/// Connections accepted but not yet picked up by a worker. When the queue is
/// full the accept loop blocks, which is the backpressure: the kernel's accept
/// backlog absorbs the rest.
const QUEUE_DEPTH: usize = WORKERS * 2;

/// How long a socket may sit idle mid-request. The timeout is per read, so a
/// slow upload is fine — this only stops a half-open connection from pinning a
/// worker forever.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Suffix of a file still being written. The inbox drain skips these, and the
/// visible name only appears once the bytes are all there.
const PART_SUFFIX: &str = ".part";

/// Suffix of the notes sidecar (`<file>.trove.json`), written and read by the
/// sidecar-notes plugin in `trove-app`. The drain skips it too, and an inbox
/// file is deleted together with it.
const NOTES_SUFFIX: &str = ".trove.json";

/// Where collected files land.
///
/// This is not a staging area to be swept clean: the library *links* whatever
/// it imports, so a collected file has to stay exactly where it is. The
/// importer consumes each file's `*.meta.json` sidecar and leaves the file
/// alone — deleting it, as the copy-based pipeline used to, would leave the
/// asset pointing at nothing.
pub fn inbox_dir() -> PathBuf {
    crate::paths::incoming_dir()
}

/// Files waiting in the inbox, each with its optional `*.meta.json` sidecar
/// path (present only when the sidecar file exists). Sidecars themselves are
/// never listed as imports, and neither are files still being written —
/// [`Landing`] renames them into place only once they are complete.
pub fn inbox_items() -> Vec<(PathBuf, Option<PathBuf>)> {
    inbox_items_in(&inbox_dir())
}

/// [`inbox_items`] over an explicit directory (tests, alternate inboxes).
///
/// This is the one definition of "what is waiting to be imported" — the
/// importer's own drain must list through here, never re-enumerate by hand:
/// the skip rules below (files still being written, sidecars of both kinds)
/// are exactly what a hand-rolled listing drifts away from.
pub fn inbox_items_in(inbox: &std::path::Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let Ok(entries) = std::fs::read_dir(inbox) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || is_inbox_sidecar(path.file_name()) {
            continue;
        }
        let sidecar = {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            inbox.join(format!("{name}.meta.json"))
        };
        items.push((path, sidecar.is_file().then_some(sidecar)));
    }
    items
}

/// Whether this directory entry is metadata about an import rather than an
/// import: the collect sidecar (`<file>.meta.json`), the sidecar the
/// sidecar-notes plugin reads (`<file>.trove.json`), or a file still being
/// written ([`PART_SUFFIX`]). A sidecar that slipped through would be
/// imported as an asset in its own right.
fn is_inbox_sidecar(name: Option<&std::ffi::OsStr>) -> bool {
    name.and_then(|n| n.to_str())
        .map(|n| n.ends_with(".meta.json") || n.ends_with(NOTES_SUFFIX) || n.ends_with(PART_SUFFIX))
        .unwrap_or(true)
}

/// Why a purge has to ask this question first: the inbox is the one
/// directory whose files are Trove's to remove — a screenshot or a collected
/// page lands here and the library links it where it stands, so deleting the
/// record can only mean deleting the file. Everywhere else a linked file
/// belongs to the user and outlives its record.
pub fn inbox_files(inbox: &Path, sources: Vec<PathBuf>) -> Vec<PathBuf> {
    sources
        .into_iter()
        .filter(|source| is_in_inbox(inbox, source))
        .collect()
}

/// Whether `path` sits inside the inbox — the question behind
/// [`inbox_files`], asked one path at a time when a purge must treat the two
/// kinds differently.
///
/// Both sides are canonicalized, so a relocated data root or a symlinked
/// `/tmp` still compares equal; a path that no longer resolves is compared as
/// written. `starts_with` is component-wise, so a sibling `incoming-old/`
/// never matches.
pub fn is_in_inbox(inbox: &Path, path: &Path) -> bool {
    resolved(path).starts_with(resolved(inbox))
}

/// Canonical form of `path`, or the path itself when it cannot be resolved.
fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Delete one inbox file together with the sidecars that describe it. True
/// when the file went away in this call.
///
/// The sidecars go too: once their file is gone they describe nothing, the
/// drain skips them, and no other code path would ever clean them up — an
/// older note would sit in the inbox forever.
pub fn remove_inbox_file(path: &Path) -> bool {
    let notes = path.with_file_name(format!(
        "{}{NOTES_SUFFIX}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_file(sidecar_path(path));
    let _ = std::fs::remove_file(notes);
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %error, "could not remove an inbox file");
            }
            false
        }
    }
}

/// Start the server on a daemon thread. Returns the bound port, or `None`
/// when the port is taken (another Trove instance is probably listening).
pub fn spawn_server(port: u16) -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let inbox = inbox_dir();
    let queue = Arc::new(Queue::new(QUEUE_DEPTH));
    for i in 0..WORKERS {
        let queue = Arc::clone(&queue);
        let inbox = inbox.clone();
        let _ = std::thread::Builder::new()
            .name(format!("trove-collect-{i}"))
            .spawn(move || {
                loop {
                    let stream = queue.pop();
                    let _ = handle(stream, &inbox);
                }
            });
    }
    std::thread::Builder::new()
        .name("trove-collect".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // A stalled client must not hold a worker for its whole
                // lifetime without ever sending a request.
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                queue.push(stream);
            }
        })
        .ok()?;
    Some(port)
}

/// Hand-off from the accept loop to the workers, with a bounded depth.
///
/// A dozen lines instead of a dependency, and bounded is the whole point: a
/// queue that grows without limit is the same unbounded growth the old
/// thread-per-connection did, only quieter. Blocks on push while full (the
/// accept loop stops accepting) and on pop while empty.
struct Queue {
    slots: Mutex<std::collections::VecDeque<TcpStream>>,
    ready: Condvar,
    depth: usize,
}

impl Queue {
    fn new(depth: usize) -> Self {
        Self {
            slots: Mutex::new(std::collections::VecDeque::with_capacity(depth)),
            ready: Condvar::new(),
            depth,
        }
    }

    fn push(&self, stream: TcpStream) {
        let mut slots = self.lock();
        while slots.len() >= self.depth {
            slots = self.ready.wait(slots).unwrap_or_else(|e| e.into_inner());
        }
        slots.push_back(stream);
        self.ready.notify_one();
    }

    /// Never returns early: workers are daemons that live as long as the
    /// process, and the queue is only ever abandoned when the process ends.
    fn pop(&self) -> TcpStream {
        let mut slots = self.lock();
        loop {
            if let Some(stream) = slots.pop_front() {
                self.ready.notify_one();
                return stream;
            }
            slots = self.ready.wait(slots).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::VecDeque<TcpStream>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The request line and headers, plus whatever body bytes arrived with them.
struct Head {
    method: String,
    /// Path plus raw query string.
    target: String,
    content_length: u64,
    /// Body bytes read past the head — clients are free to send both in one
    /// packet, and dropping them would truncate the upload.
    rest: Vec<u8>,
}

fn handle(mut stream: TcpStream, inbox: &Path) -> std::io::Result<()> {
    let head = match read_head(&mut stream) {
        Ok(Some(head)) => head,
        _ => {
            return respond(stream, 400, "{\"ok\":false,\"error\":\"bad request\"}");
        }
    };

    match (head.method.as_str(), head.target.split('?').next()) {
        // CORS preflight: browsers send OPTIONS before the octet-stream
        // POST /add; without a 2xx the actual upload never fires.
        ("OPTIONS", _) => respond(stream, 204, ""),
        ("GET", Some("/") | Some("")) => respond_html(stream, 200, &index_page()),
        ("GET", Some("/ping")) => respond(stream, 200, "trove ok"),
        ("GET", Some("/health")) => {
            // The metrics registry is process-global statics, so the server
            // thread reads it without going anywhere near the store or the
            // UI thread.
            let body = serde_json::to_string(&crate::metrics::snapshot())
                .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"snapshot\"}".to_string());
            respond(stream, 200, &body)
        }
        ("GET", Some("/collections")) => {
            let (status, body) = collections_body();
            respond(stream, status, &body)
        }
        ("POST", Some("/add")) => {
            if head.content_length > MAX_BODY {
                return respond(stream, 413, "{\"ok\":false,\"error\":\"body too large\"}");
            }
            let query = parse_query(&head.target);
            let name = query
                .get("filename")
                .cloned()
                .unwrap_or_else(|| "collected.bin".to_string());
            let collection = match collection_target(query.get("collection").map(String::as_str)) {
                Ok(id) => id,
                Err(message) => {
                    // The upload is read and dropped rather than closed over:
                    // resetting mid-body reaches the browser as "cannot reach
                    // Trove", and the user would be told their app is not
                    // running when what they got was a bad destination.
                    let _ = pump_body(&mut stream, &head.rest, head.content_length, |_| Ok(()));
                    return respond(stream, 400, &error_body(&message));
                }
            };
            note_focus(flag_from_text(query.get("focus").map(String::as_str)));
            let meta = CollectMeta {
                source: query.get("source").cloned(),
                collection,
            };
            let mut landing = match Landing::new(inbox, &name) {
                Ok(landing) => landing,
                Err(e) => {
                    return respond(stream, 500, &format!("{{\"ok\":false,\"error\":\"{e}\"}}"));
                }
            };
            if let Err(e) = pump_body(&mut stream, &head.rest, head.content_length, |chunk| {
                landing.write(chunk)
            }) {
                landing.abort();
                return respond(stream, 400, &format!("{{\"ok\":false,\"error\":\"{e}\"}}"));
            }
            match landing.finish(&meta) {
                Ok(saved) => respond(
                    stream,
                    200,
                    &format!("{{\"ok\":true,\"file\":\"{saved}\"}}"),
                ),
                Err(e) => respond(stream, 500, &format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
            }
        }
        ("POST", Some("/fetch")) => {
            let raw = match read_small_body(&mut stream, &head.rest, head.content_length) {
                Ok(raw) => raw,
                Err(e) => {
                    return respond(stream, 400, &format!("{{\"ok\":false,\"error\":\"{e}\"}}"));
                }
            };
            let body: serde_json::Value = match serde_json::from_slice(&raw) {
                Ok(v) => v,
                Err(e) => {
                    return respond(
                        stream,
                        400,
                        &format!("{{\"ok\":false,\"error\":\"bad json: {e}\"}}"),
                    );
                }
            };
            let Some(url) = body.get("url").and_then(|v| v.as_str()).map(String::from) else {
                return respond(stream, 400, "{\"ok\":false,\"error\":\"url required\"}");
            };
            let name = body
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| suggested_name(&url))
                .unwrap_or_else(|| "collected.bin".to_string());
            let source = body
                .get("source")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| Some(url.clone()));
            let referer = body
                .get("referer")
                .and_then(|v| v.as_str())
                .map(String::from);
            let reject_html = body
                .get("reject_html")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Checked before anything is downloaded: a save cannot name a
            // destination that does not exist, and finding that out after
            // fetching the bytes would waste them.
            let collection =
                match collection_target(body.get("collection").and_then(|v| v.as_str())) {
                    Ok(id) => id,
                    Err(message) => return respond(stream, 400, &error_body(&message)),
                };
            note_focus(body.get("focus").and_then(|v| v.as_bool()).unwrap_or(false));
            let meta = CollectMeta { source, collection };
            let mut landing = match Landing::new(inbox, &name) {
                Ok(landing) => landing,
                Err(e) => {
                    return respond(stream, 500, &format!("{{\"ok\":false,\"error\":\"{e}\"}}"));
                }
            };
            if let Err(e) = download(&url, referer.as_deref(), reject_html, |chunk| {
                landing.write(chunk)
            }) {
                landing.abort();
                return respond(
                    stream,
                    502,
                    &format!("{{\"ok\":false,\"error\":\"download failed: {e}\"}}"),
                );
            }
            match landing.finish(&meta) {
                Ok(saved) => respond(
                    stream,
                    200,
                    &format!("{{\"ok\":true,\"file\":\"{saved}\"}}"),
                ),
                Err(e) => respond(stream, 500, &format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
            }
        }
        _ => respond(stream, 404, "{\"ok\":false,\"error\":\"not found\"}"),
    }
}

/// Read one HTTP request head: up to `\r\n\r\n`, capped at 64 KiB. Body bytes
/// that arrived in the same packet come back in [`Head::rest`] rather than
/// being dropped — a client is free to send head and body together.
fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<Head>> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    let head_end;
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            head_end = pos;
            break;
        }
        if buf.len() > 64 * 1024 {
            return Ok(None);
        }
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_length: u64 = 0;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    Ok(Some(Head {
        method,
        target,
        content_length,
        rest: buf[head_end + 4..].to_vec(),
    }))
}

/// Copy exactly `len` body bytes into `sink`, starting with the bytes
/// [`read_head`] already pulled off the socket.
///
/// The body is never collected into a `Vec`: a 512 MB upload costs the file it
/// is written to and one chunk of buffer, instead of an equal-sized allocation
/// per concurrent request.
fn pump_body(
    stream: &mut TcpStream,
    rest: &[u8],
    len: u64,
    mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut remaining = len;
    let carry = rest.len().min(remaining as usize);
    if carry > 0 {
        sink(&rest[..carry])?;
        remaining -= carry as u64;
    }
    let mut chunk = vec![0_u8; PUMP_CHUNK];
    while remaining > 0 {
        let want = remaining.min(PUMP_CHUNK as u64) as usize;
        let n = stream.read(&mut chunk[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request body truncated",
            ));
        }
        sink(&chunk[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Read a small body (the `/fetch` JSON) into memory, refusing anything past
/// [`MAX_JSON_BODY`]. A 512 MB cap exists for media, not for a URL and a name.
fn read_small_body(stream: &mut TcpStream, rest: &[u8], len: u64) -> std::io::Result<Vec<u8>> {
    if len > MAX_JSON_BODY {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "json body too large",
        ));
    }
    let mut body = Vec::with_capacity(len as usize);
    pump_body(stream, rest, len, |chunk| {
        body.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(body)
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_query(target: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Some((_, query)) = target.split_once('?') else {
        return map;
    };
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(url_decode(k), url_decode(v));
        }
    }
    map
}

/// Minimal percent-decoding (queries only ever carry names and URLs).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Derive a file name from the URL path.
fn suggested_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let name = path.rsplit('/').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// The `collection` argument, checked at the boundary and handed back
/// canonicalised.
///
/// A caller that named a destination must not get a file that quietly landed
/// somewhere else, so anything that is not a collection id fails the request
/// instead of being dropped. An absent, empty, or `null` argument means "no
/// preference" — a query string cannot spell null, and the extension's recent
/// list carries the library root as `null`.
fn collection_target(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(text) = raw.map(str::trim).filter(|t| !t.is_empty() && *t != "null") else {
        return Ok(None);
    };
    uuid::Uuid::parse_str(text)
        .map(|id| Some(id.to_string()))
        .map_err(|_| format!("collection `{text}` is not a collection id"))
}

/// A query-string flag: `1`, `true` and `yes` (any case) mean on.
fn flag_from_text(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Remember that the save wants Trove's window brought forward, for the UI
/// thread to spend on its next inbox pass.
fn note_focus(on: bool) {
    if on {
        WAKE_REQUESTED.store(true, Ordering::Relaxed);
    }
}

/// A JSON error body. Through serde, because an argument echoed back in the
/// message may itself contain quotes.
fn error_body(message: &str) -> String {
    serde_json::json!({ "ok": false, "error": message }).to_string()
}

/// The library the app runs on: the recorded active slug, resolved the way
/// startup resolves it. `None` when nothing is recorded, or when its database
/// is not on disk — then there is no tree to offer.
fn active_library_db() -> Option<(String, PathBuf)> {
    let config = AppConfig::load();
    config.active_library.as_ref()?;
    let entry = config.active_entry();
    let db = entry.dir().join("library.db");
    db.is_file().then(|| (entry.name.clone(), db))
}

/// `GET /collections` — the open library's collection tree, the list the
/// extension draws its save-into menu from.
///
/// Flat and in display order (a parent, then its whole subtree) with
/// `parentId` on every row, so nesting is the caller's business and one
/// request covers any depth. `path` is carried because two collections may
/// share a name at different depths, and an unqualified "图标" in a menu says
/// which one it is only once its parent is spelled out.
///
/// Read-only by construction: a second connection over the same file is what
/// WAL is for, and nothing in here writes. A library that is not open, or is
/// unreadable, answers 503 with the reason rather than an empty tree — an
/// empty tree would read as "this library has no collections", which is a
/// different fact, and would send saves to the wrong place without a word.
fn collections_body() -> (u16, String) {
    let Some((library, db)) = active_library_db() else {
        return (503, error_body("no library is open"));
    };
    catalog_body(&library, &db)
}

/// The catalog over one library file, split from [`collections_body`] so the
/// tree it builds is testable without a written config pointing at it.
fn catalog_body(library: &str, db: &Path) -> (u16, String) {
    let conn = match rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(error) => {
            tracing::warn!(%error, path = %db.display(), "collect service could not read the library");
            return (
                503,
                error_body("the library could not be opened for reading"),
            );
        }
    };
    // A checkpoint holding the write lock must not make a menu request wait
    // out the socket timeout; the caller keeps its last tree anyway.
    if let Err(error) = conn.busy_timeout(Duration::from_millis(250)) {
        tracing::warn!(%error, "collect service could not set a busy timeout");
    }
    let (collections, counts) = match (
        crate::store::collections::list(&conn),
        crate::store::collections::asset_counts(&conn),
    ) {
        (Ok(collections), Ok(counts)) => (collections, counts),
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, "collect service could not list collections");
            return (503, error_body("the collection tree could not be read"));
        }
    };

    // Children per parent, each sibling run ordered the way the app orders it:
    // position, then name.
    let mut children: std::collections::HashMap<
        Option<uuid::Uuid>,
        Vec<&crate::model::Collection>,
    > = std::collections::HashMap::new();
    for collection in &collections {
        children
            .entry(collection.parent_id)
            .or_default()
            .push(collection);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    // Walk down from the roots, so the array is readable as a list even to a
    // caller that ignores `parentId`. A cycle could not terminate this walk,
    // but `collections::move_to` refuses one, and a `parent_id` pointing at a
    // missing row keeps that subtree out of the walk rather than hanging it.
    let mut entries = Vec::with_capacity(collections.len());
    let mut stack: Vec<(String, &crate::model::Collection)> = children
        .get(&None)
        .into_iter()
        .flatten()
        .rev()
        .map(|c| (c.name.clone(), *c))
        .collect();
    while let Some((path, collection)) = stack.pop() {
        entries.push(serde_json::json!({
            "id": collection.id.to_string(),
            "name": collection.name,
            "parentId": collection.parent_id.map(|p| p.to_string()),
            "path": path,
            "assetCount": counts.get(&collection.id).copied().unwrap_or(0),
        }));
        if let Some(siblings) = children.get(&Some(collection.id)) {
            for child in siblings.iter().rev() {
                stack.push((format!("{path}/{}", child.name), *child));
            }
        }
    }
    (
        200,
        serde_json::json!({ "ok": true, "library": library, "collections": entries }).to_string(),
    )
}

/// A file being landed in the inbox.
///
/// Bytes go to `<stem>.part`; the visible name appears only via `rename`, once
/// the file is complete and its sidecar is in place. Both halves of that order
/// matter to the drain:
///
/// - a half-written file that got imported would record a content hash its
///   own bytes no longer match, and the library *links* its files, so that
///   asset would stay wrong for good;
/// - a file whose sidecar has not been written yet imports *without* its source
///   URL — the drain pairs the two by name at scan time, so the sidecar has to
///   exist before the file is visible.
struct Landing {
    part: PathBuf,
    path: PathBuf,
    file: Option<BufWriter<std::fs::File>>,
}

impl Landing {
    fn new(inbox: &Path, raw_name: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(inbox)?;
        let name = sanitize_name(raw_name);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // `create_new` on the part file is what makes the name unique: two
        // captures in the same nanosecond race here, and the loser takes a
        // numbered name instead of writing over the winner.
        for n in 0..64 {
            let stem = if n == 0 {
                format!("{nanos}-{name}")
            } else {
                format!("{nanos}-{n}-{name}")
            };
            let part = inbox.join(format!("{stem}{PART_SUFFIX}"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)
            {
                Ok(file) => {
                    return Ok(Self {
                        part,
                        path: inbox.join(stem),
                        file: Some(BufWriter::new(file)),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "inbox name collision",
        ))
    }

    fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.write_all(chunk),
            None => Err(std::io::Error::other("landing already finished")),
        }
    }

    /// Flush, write the sidecar, rename into the visible name, and hand the
    /// file name back for the response body.
    ///
    /// No sidecar goes out when the caller said nothing about the file: the
    /// drain would otherwise carry an empty description of a capture that
    /// needs none.
    fn finish(mut self, meta: &CollectMeta) -> std::io::Result<String> {
        let mut file = self.file.take().expect("open until finished");
        file.flush()?;
        // The rename is atomic against a concurrent reader; only fsync makes
        // the *contents* durable. Without it a crash can leave a visible file
        // with nothing in it.
        file.get_ref().sync_all()?;
        drop(file);
        if meta.source.is_some() || meta.collection.is_some() {
            let mut sidecar = serde_json::Map::new();
            if let Some(source) = meta.source.as_deref() {
                sidecar.insert("source_url".to_string(), serde_json::json!(source));
            }
            if let Some(collection) = meta.collection.as_deref() {
                sidecar.insert("collection".to_string(), serde_json::json!(collection));
            }
            std::fs::write(
                sidecar_path(&self.path),
                serde_json::Value::Object(sidecar).to_string(),
            )?;
        }
        std::fs::rename(&self.part, &self.path)?;
        Ok(self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default())
    }

    /// Give up: drop the partial file so a failed request leaves nothing behind.
    fn abort(mut self) {
        self.file = None;
        let _ = std::fs::remove_file(&self.part);
    }
}

/// `<name>.meta.json` next to `<name>` — the sidecar name the drain looks for.
fn sidecar_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.meta.json"))
}

/// Keep the client's name recognisable but harmless as a path component, and
/// never let it end in a suffix the drain treats as internal — a captured file
/// named `shot.part` would otherwise sit in the inbox forever, invisible.
fn sanitize_name(raw_name: &str) -> String {
    let safe: String = raw_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._- ()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim().to_string();
    if safe.is_empty() {
        return "collected.bin".to_string();
    }
    if safe.ends_with(PART_SUFFIX) || safe.ends_with(".meta.json") {
        format!("{safe}.bin")
    } else {
        safe
    }
}

/// In-app URL import: download `url` into the inbox with a source-URL sidecar,
/// so the next inbox drain imports it and records the source on the asset.
/// Returns the saved file name.
pub fn fetch_to_inbox(url: &str) -> Result<String, Error> {
    // Checked before the landing exists, so a rejected URL leaves no trace.
    ensure_http(url)?;
    let name = suggested_name(url).unwrap_or_else(|| "collected.bin".to_string());
    let mut landing = Landing::new(&inbox_dir(), &name).map_err(Error::from)?;
    if let Err(e) = download(url, None, false, |chunk| landing.write(chunk)) {
        landing.abort();
        return Err(e);
    }
    landing
        .finish(&CollectMeta {
            source: Some(url.to_string()),
            collection: None,
        })
        .map_err(Error::from)
}

/// The two schemes this service will fetch.
fn ensure_http(url: &str) -> Result<(), Error> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(Error::Validation("only http(s) URLs are supported".into()))
    }
}

/// Download `url` and stream the body into `sink`, in chunks.
///
/// `referer` — the capturing page, when the caller knows it — and the
/// browser-like [`BROWSER_UA`] are what let this path through hotlink
/// protection, which is the whole reason the browser extension falls back to
/// it. With `reject_html`, a text/html answer is refused instead of landed in
/// the library: this path exists for *files*, and a webpage slipping in
/// silently is worse than a failed save.
fn download(
    url: &str,
    referer: Option<&str>,
    reject_html: bool,
    mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<(), Error> {
    ensure_http(url)?;
    let request = ureq::get(url).header("User-Agent", BROWSER_UA);
    let request = match referer {
        Some(page) => request.header("Referer", page),
        None => request,
    };
    let mut response = request
        .config()
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .call()?;
    if reject_html {
        let is_html = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .is_some_and(|value| value.starts_with("text/html"));
        if is_html {
            return Err(Error::Validation(
                "the URL answered with a webpage (text/html), not a file".into(),
            ));
        }
    }
    let mut reader = response.body_mut().with_config().limit(MAX_BODY).reader();
    let mut chunk = vec![0_u8; PUMP_CHUNK];
    loop {
        let n = reader.read(&mut chunk).map_err(Error::from)?;
        if n == 0 {
            return Ok(());
        }
        sink(&chunk[..n]).map_err(Error::from)?;
    }
}

/// Browser-friendly landing page: someone opening the endpoint in a tab
/// should see what this is instead of a JSON 404.
fn index_page() -> String {
    let port = AppConfig::load().collect_port();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Trove collect service</title>
<style>
  body {{ font-family: system-ui, sans-serif; background: #141414; color: #e6e6e6;
         max-width: 46rem; margin: 3rem auto; padding: 0 1.5rem; line-height: 1.6; }}
  code, pre {{ background: #1f1f1f; border: 1px solid #333; border-radius: 6px; }}
  code {{ padding: .1rem .35rem; }}
  pre {{ padding: .75rem 1rem; overflow-x: auto; }}
  h1 {{ font-size: 1.3rem; }} .ok {{ color: #4ade80; }} .dim {{ color: #9a9a9a; }}
  table {{ border-collapse: collapse; }} td, th {{ border: 1px solid #333; padding: .3rem .6rem;
         text-align: left; }} th {{ background: #1f1f1f; }}
</style>
</head>
<body>
<h1><span class="ok">&#9679;</span> Trove collect service is running</h1>
<p class="dim">本地采集服务已就绪 —— 浏览器扩展 / curl 把文件 POST 到这里，Trove 会自动导入。</p>
<h3>Endpoints</h3>
<table>
<tr><th>Method</th><th>Path</th><th>Purpose</th></tr>
<tr><td>GET</td><td><code>/ping</code></td><td>liveness check</td></tr>
<tr><td>GET</td><td><code>/health</code></td><td>metrics &amp; health snapshot (JSON)</td></tr>
<tr><td>GET</td><td><code>/collections</code></td><td>the open library&#39;s collection tree (JSON)</td></tr>
<tr><td>POST</td><td><code>/add?filename=NAME&amp;source=URL&amp;collection=ID&amp;focus=1</code></td><td>upload raw file bytes</td></tr>
<tr><td>POST</td><td><code>/fetch</code></td><td>server downloads <code>{{"url": "…", "collection": "ID"}}</code></td></tr>
</table>
<h3>Try it</h3>
<pre>curl --data-binary @image.png   'http://127.0.0.1:{port}/add?filename=image.png&amp;source=https://example.com/image'</pre>
<pre>curl -X POST http://127.0.0.1:{port}/fetch   -d '{{"url":"https://example.com/image.png"}}'</pre>
<p class="dim">Files land in the inbox and import automatically (source URL, and a
<code>collection</code> when one was named, are applied to the asset).
设置 ▸ 通用 可关闭此服务。</p>
</body>
</html>
"#
    )
}

fn respond_html(mut stream: TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} OK
Content-Type: text/html; charset=utf-8
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: GET, POST, OPTIONS
Access-Control-Allow-Headers: Content-Type
Content-Length: {}
Connection: close

",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

fn respond(mut stream: TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Max-Age: 86400\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::Temp;

    /// A file with any content, at its final path.
    fn write(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }

    #[test]
    fn sidecars_and_half_written_files_are_never_listed_as_imports() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("shot.png"), b"png").unwrap();
        std::fs::write(inbox.join("shot.png.meta.json"), b"{}").unwrap();
        std::fs::write(inbox.join("shot.png.trove.json"), b"{}").unwrap();
        std::fs::write(inbox.join("upload.png.part"), b"half").unwrap();

        let items = inbox_items_in(&inbox);
        assert_eq!(
            items.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
            vec![inbox.join("shot.png")],
            "only the complete file is waiting"
        );

        std::fs::remove_dir_all(&inbox).unwrap();
    }

    /// The purge rule leans on this: only files in the inbox are Trove's to
    /// delete, and "in the inbox" has to mean the directory itself rather than
    /// anything whose path happens to start with the same letters.
    #[test]
    fn only_files_inside_the_inbox_are_troves_own() {
        let root = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        let inbox = root.join("incoming");
        let nested = inbox.join("nested");
        let sibling = root.join("incoming-old");
        let pictures = root.join("pictures");
        for dir in [&nested, &sibling, &pictures] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let inside = write(&inbox, "shot.png");
        let deep = write(&nested, "deep.png");
        let near_miss = write(&sibling, "shot.png");
        let users = write(&pictures, "holiday.jpg");

        let owned = inbox_files(
            &inbox,
            vec![
                inside.clone(),
                deep.clone(),
                near_miss.clone(),
                users.clone(),
            ],
        );
        assert_eq!(owned, vec![inside.clone(), deep]);

        // A spelling with a `.` in it still resolves to the same file.
        assert_eq!(
            inbox_files(&inbox, vec![inbox.join(".").join("shot.png")]).len(),
            1
        );

        // An inbox that does not exist yet owns nothing.
        assert!(inbox_files(&root.join("missing"), vec![users.clone()]).is_empty());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_an_inbox_file_takes_its_sidecars() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        let shot = write(&inbox, "shot.png");
        let meta = inbox.join("shot.png.meta.json");
        let notes = inbox.join("shot.png.trove.json");
        std::fs::write(&meta, b"{}").unwrap();
        std::fs::write(&notes, b"{}").unwrap();
        let neighbour = write(&inbox, "other.png");

        assert!(remove_inbox_file(&shot));
        assert!(!shot.exists());
        assert!(!meta.exists(), "the collect sidecar goes with the file");
        assert!(!notes.exists(), "so does the notes sidecar");
        assert!(neighbour.exists(), "nothing else in the inbox is touched");

        // Asking twice reports no second removal.
        assert!(!remove_inbox_file(&shot));

        std::fs::remove_dir_all(&inbox).unwrap();
    }

    #[test]
    fn fetch_to_inbox_rejects_non_http_urls() {
        assert!(fetch_to_inbox("ftp://example.com/a.png").is_err());
        assert!(fetch_to_inbox("file:///etc/passwd").is_err());
        assert!(fetch_to_inbox("data:text/plain,hi").is_err());
    }

    /// A destination the caller names is honoured or the request fails: a save
    /// must never land a file somewhere else because its id was wrong. Ids
    /// come back canonical, because the sidecar's contents are compared
    /// against the database.
    #[test]
    fn a_named_collection_is_canonicalised_and_a_wrong_one_is_refused() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            collection_target(Some(&id.to_string())).unwrap(),
            Some(id.to_string())
        );
        assert_eq!(
            collection_target(Some(&format!("{{{id}}}"))).unwrap(),
            Some(id.to_string()),
            "braces are the same id"
        );
        assert_eq!(
            collection_target(Some(&id.simple().to_string())).unwrap(),
            Some(id.to_string()),
            "and so is the hyphenless spelling"
        );
        for no_preference in [None, Some(""), Some("  "), Some("null")] {
            assert_eq!(
                collection_target(no_preference).unwrap(),
                None,
                "{no_preference:?} is no preference"
            );
        }
        assert!(collection_target(Some("favorites")).is_err());
    }

    #[test]
    fn a_query_flag_reads_as_text() {
        for on in ["1", "true", "TRUE", " yes "] {
            assert!(flag_from_text(Some(on)), "{on} means on");
        }
        for off in [None, Some(""), Some("0"), Some("false"), Some("no")] {
            assert!(!flag_from_text(off), "{off:?} means off");
        }
    }

    /// The extension's save adds two things to the old `/add`: the collection
    /// to file into and a request to bring Trove forward. Neither is acted on
    /// here — the first waits on the sidecar for the importer, the second in a
    /// process flag for the UI thread.
    #[test]
    fn an_upload_records_its_destination_and_its_wake_request() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        let inbox_for_assert = inbox.clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let _ = take_wake_request();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        let collection = uuid::Uuid::new_v4();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = b"png-bytes";
        let head = format!(
            "POST /add?filename=pic.png&source=https%3A%2F%2Fexample.com%2Fpic&collection={collection}&focus=1 HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("\"ok\":true"), "{response}");

        let sidecars: Vec<_> = std::fs::read_dir(&inbox_for_assert)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".meta.json"))
            .collect();
        assert_eq!(sidecars.len(), 1, "one sidecar for the one upload");
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecars[0]).unwrap()).unwrap();
        assert_eq!(meta["source_url"], "https://example.com/pic");
        assert_eq!(meta["collection"], collection.to_string());

        assert!(
            take_wake_request(),
            "the wake request reaches the UI thread"
        );
        assert!(
            !take_wake_request(),
            "and is spent — it must not raise the window again later"
        );

        std::fs::remove_dir_all(&inbox_for_assert).unwrap();
    }

    /// A destination that is not a collection id is refused before a byte is
    /// landed. Filing the capture where the user did not ask, and answering
    /// `ok`, would be worse than failing the save.
    #[test]
    fn an_upload_naming_a_non_collection_lands_nothing() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        let inbox_for_assert = inbox.clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = b"png-bytes";
        let head = format!(
            "POST /add?filename=pic.png&collection=favorites HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("400"), "{response}");
        assert!(response.contains("not a collection id"), "{response}");
        assert!(
            inbox_items_in(&inbox_for_assert).is_empty(),
            "a refused save leaves no file and no part behind"
        );

        std::fs::remove_dir_all(&inbox_for_assert).unwrap();
    }

    /// A library file: three collections (two roots, one nested) with real
    /// assets filed into two of them. Real records because `Store` turns
    /// foreign keys on, so a membership row needs both sides.
    fn catalog_fixture() -> (Temp, PathBuf, [uuid::Uuid; 3]) {
        let root = Temp::new("collect-catalog");
        let db = root.path().join("library.db");
        let store = crate::store::Store::open(&db).unwrap();
        let conn = store.conn();
        let new = |parent_id: Option<uuid::Uuid>, name: &str, position: i64| {
            crate::store::collections::create(
                conn,
                &crate::model::NewCollection {
                    parent_id,
                    name: name.to_string(),
                    position,
                },
            )
            .unwrap()
            .id
        };
        let reference = new(None, "参考", 0);
        let icons = new(None, "图标", 1);
        let png = new(Some(icons), "PNG", 0);
        let file = |collection: uuid::Uuid, how_many: u64| {
            for n in 0..how_many {
                let asset = sample_asset(&format!("{collection}-{n}.png"));
                crate::store::assets::insert(conn, &asset).unwrap();
                crate::store::collections::add_asset(
                    conn,
                    crate::model::CollectionId(collection),
                    crate::model::AssetId(asset.id),
                )
                .unwrap();
            }
        };
        file(icons, 2);
        file(png, 1);
        drop(store);
        (root, db, [reference, icons, png])
    }

    /// The smallest record `assets` accepts.
    fn sample_asset(name: &str) -> crate::model::Asset {
        use crate::model::{AssetKind, AssetLocation, AssetSeed, Placement, UsageStatus, now};
        let id = uuid::Uuid::new_v4();
        crate::model::Asset::from_seed(AssetSeed {
            id,
            location: AssetLocation::Stored {
                rel_path: format!("media/{}/{}", &id.to_string()[..2], name),
            },
            file_name: name.to_string(),
            ext: "png".into(),
            mime: "image/png".into(),
            size_bytes: 128,
            content_hash: Some(crate::model::ContentHash::from_hasher("a".repeat(64))),
            kind: AssetKind::Image,
            width: Some(1),
            height: Some(1),
            duration_ms: None,
            captured_at: None,
            title: None,
            description: None,
            rating: None,
            is_favorite: false,
            source_url: None,
            usage_status: UsageStatus::Unused,
            commercial_use: None,
            facts: Default::default(),
            created_at: now(),
            updated_at: now(),
            placement: Placement::Live,
        })
    }

    /// `GET /collections`: one flat array, a parent then its subtree, each row
    /// naming its parent and carrying the whole path — the menu needs the path
    /// because "图标" alone does not say which of two same-named collections
    /// a save will land in.
    #[test]
    fn the_collection_catalog_is_flat_in_display_order_with_paths() {
        let (_root, db, [reference, icons, png]) = catalog_fixture();
        let (status, body) = catalog_body("我的素材库", &db);
        assert_eq!(status, 200, "{body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["library"], "我的素材库");
        let rows = parsed["collections"].as_array().unwrap();
        let listed: Vec<(String, String, u64)> = rows
            .iter()
            .map(|row| {
                (
                    row["path"].as_str().unwrap().to_string(),
                    row["id"].as_str().unwrap().to_string(),
                    row["assetCount"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            listed
                .iter()
                .map(|(path, _, _)| path.as_str())
                .collect::<Vec<_>>(),
            vec!["参考", "图标", "图标/PNG"],
            "roots by position, each subtree right after its parent"
        );
        assert_eq!(
            listed
                .iter()
                .map(|(_, _, count)| *count)
                .collect::<Vec<_>>(),
            vec![0, 2, 1],
            "directly-held assets, not the subtree's"
        );
        assert_eq!(listed[0].1, reference.to_string());
        assert_eq!(listed[2].1, png.to_string());
        // Only the nested row names a parent, and it is the middle row's id.
        let parents: Vec<Option<String>> = rows
            .iter()
            .map(|row| row["parentId"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            parents,
            vec![None, None, Some(icons.to_string())],
            "nesting is carried per row, so one request covers any depth"
        );
    }

    /// A file that is not a library answers 503 with the reason. An empty
    /// `collections` array would be a *different* claim — "this library has
    /// none" — and the extension would render a menu with nothing in it
    /// instead of falling back to the default destination.
    #[test]
    fn an_unreadable_library_says_so_instead_of_claiming_no_collections() {
        let root = Temp::new("collect-catalog-bogus");
        let bogus = root.path().join("library.db");
        std::fs::write(&bogus, b"definitely not sqlite").unwrap();
        let (status, body) = catalog_body("我的素材库", &bogus);
        assert_eq!(status, 503, "{body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            parsed.get("collections").is_none(),
            "no tree is offered at all: {body}"
        );
        assert!(
            parsed["error"]
                .as_str()
                .is_some_and(|why| why.contains("could not be")),
            "the reason is stated, not swallowed: {body}"
        );
    }

    /// `/fetch` validates its JSON — a missing url is a 400 — and the fields
    /// the browser extension sends alongside the old ones (`referer`,
    /// `reject_html`) parse. Both paths reject before any network is touched.
    #[test]
    fn fetch_json_is_validated_and_new_fields_parse() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        let send = |body: &str| {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let head = format!(
                "POST /fetch HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        };

        let response = send("{}");
        assert!(response.contains("400"), "{response}");
        assert!(response.contains("url required"), "{response}");

        let response = send(
            r#"{"url":"ftp://example.com/a.png","name":"a.png","source":"https://page.example/","referer":"https://page.example/","reject_html":true}"#,
        );
        assert!(response.contains("502"), "{response}");
        assert!(response.contains("only http(s)"), "{response}");
    }

    #[test]
    fn server_saves_upload_with_sidecar() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        let inbox_for_assert = inbox.clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        // POST /add with a raw body.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();

        let body = b"png-bytes";
        let head = format!(
            "POST /add?filename=pic.png&source=https%3A%2F%2Fexample.com%2Fpic HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("\"ok\":true"), "{response}");

        // The file plus sidecar are in the inbox.
        let entries: Vec<String> = std::fs::read_dir(&inbox_for_assert)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(entries.iter().any(|n| n.ends_with("-pic.png")));
        let sidecar = entries.iter().find(|n| n.ends_with(".meta.json")).unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(inbox_for_assert.join(sidecar)).unwrap())
                .unwrap();
        assert_eq!(meta["source_url"], "https://example.com/pic");

        std::fs::remove_dir_all(&inbox_for_assert).unwrap();
    }

    #[test]
    fn cors_headers_and_preflight() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        // Preflight for the extension's POST /add must pass with CORS headers.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"OPTIONS /add HTTP/1.1\r\nOrigin: moz-extension://abc\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("204"), "{response}");
        assert!(
            response.contains("Access-Control-Allow-Origin: *"),
            "{response}"
        );
        assert!(response.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS"));

        // Plain requests (e.g. the popup's ping) expose the headers too.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"GET /ping HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("trove ok"));
        assert!(response.contains("Access-Control-Allow-Origin: *"));
    }

    #[test]
    fn ping_and_404() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"GET /ping HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("trove ok"));

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"GET /nope HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("404"));

        // Browsing the root shows the landing page, not a JSON 404.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("text/html"));
        assert!(response.contains("collect service is running"));

        // `/collections` is routed. Whether the answer is a tree or a refusal
        // depends on which library this process's configuration points at, and
        // nothing here asserts either — the contract being pinned is that the
        // path is served and answers JSON, which is what `catalog_body`'s own
        // tests cannot see. Read-only throughout: the route never writes.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /collections HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(!response.contains("404"), "{response}");
        let body = response
            .rsplit_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|e| {
            panic!("the catalog answer is not JSON ({e}): {response}");
        });
        assert!(
            parsed["ok"].is_boolean() || parsed["error"].is_string(),
            "{body}"
        );
    }

    /// `/health` serves the metrics snapshot as JSON: parseable, versioned,
    /// with the headline library gauges present.
    #[test]
    fn health_serves_the_metrics_snapshot() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"GET /health HTTP/1.1\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("200"), "{response}");
        assert!(response.contains("application/json"), "{response}");

        let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
        let value: serde_json::Value = serde_json::from_str(body).expect("valid JSON body");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert!(value["uptime_secs"].is_u64());
        assert!(value["library"]["open"].is_boolean());
        assert!(value["outbox"]["drained_rows_total"].is_u64());
        assert!(
            value["thumb_cache"]["hit_rate"].is_number()
                || value["thumb_cache"]["hit_rate"].is_null()
        );
        assert!(value["queries"]["slow_threshold_ms"].is_u64());
    }

    /// The drain must never see a file that is still being written, nor a
    /// captured name that would hide behind the internal suffixes.
    #[test]
    fn partial_uploads_and_internal_suffixes_stay_invisible() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("a.png"), b"done").unwrap();
        std::fs::write(inbox.join("b.png.part"), b"half").unwrap();
        std::fs::write(inbox.join("c.png.meta.json"), b"{}").unwrap();

        let items = inbox_items_in(&inbox);
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(items[0].0.ends_with("a.png"));

        assert_eq!(sanitize_name("shot.part"), "shot.part.bin");
        assert_eq!(sanitize_name("x.meta.json"), "x.meta.json.bin");
        assert_eq!(sanitize_name("   "), "collected.bin");
        assert_eq!(sanitize_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_name("  shot.png  "), "shot.png");

        std::fs::remove_dir_all(&inbox).ok();
    }

    /// A body bigger than one pump chunk lands byte-for-byte, with no partial
    /// file left behind. The body deliberately contains a header terminator and
    /// non-UTF8 bytes: it must be copied by length, never parsed.
    #[test]
    fn a_multi_chunk_upload_lands_exactly() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        let inbox_for_assert = inbox.clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(&vec![0xFF_u8; 200 * 1024]);
        body.extend_from_slice(b"\r\n\r\ntail");

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let head = format!(
            "POST /add?filename=big.bin HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        // Split the body across two writes: the head and the first packet must
        // not be mistaken for the whole request.
        stream.write_all(&body[..1000]).unwrap();
        stream.write_all(&body[1000..]).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("\"ok\":true"), "{response}");

        let entries: Vec<PathBuf> = std::fs::read_dir(&inbox_for_assert)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(
            !entries
                .iter()
                .any(|p| p.to_string_lossy().ends_with(PART_SUFFIX)),
            "partial file left behind: {entries:?}"
        );
        let landed = entries
            .iter()
            .find(|p| p.to_string_lossy().ends_with("-big.bin"))
            .unwrap_or_else(|| panic!("no landed file in {entries:?}"));
        assert_eq!(std::fs::read(landed).unwrap(), body);

        std::fs::remove_dir_all(&inbox_for_assert).ok();
    }

    /// A client that gives up mid-body must not leave a truncated file in the
    /// inbox: importing one would record a hash its bytes no longer match.
    #[test]
    fn a_truncated_upload_leaves_nothing_behind() {
        let inbox = std::env::temp_dir().join(format!("trove-inbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&inbox).unwrap();
        let inbox_for_assert = inbox.clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream, &inbox);
            }
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"POST /add?filename=cut.png HTTP/1.1\r\nContent-Length: 100000\r\n\r\n")
            .unwrap();
        stream.write_all(&[0_u8; 10]).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(response.contains("400"), "{response}");

        let entries: Vec<String> = std::fs::read_dir(&inbox_for_assert)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(entries.is_empty(), "left behind: {entries:?}");

        std::fs::remove_dir_all(&inbox_for_assert).ok();
    }

    /// A scratch directory that removes itself. The other tests here clean up
    /// by hand; the catalog ones hold a `Store` and a read-only connection
    /// over the same file, and a failing assertion should not leave both of
    /// them sitting in `/tmp`.
    mod tempdir {
        use std::path::PathBuf;

        pub struct Temp(PathBuf);
        impl Temp {
            pub fn new(name: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "trove-{name}-{}-{}",
                    std::process::id(),
                    crate::model::new_id().simple()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Temp(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Temp {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
    }
}
