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
//! - `POST /add?filename=NAME&source=URL` — body is the raw file bytes
//!   (`curl --data-binary @img.png 'http://127.0.0.1:P/add?filename=a.png'`)
//! - `POST /fetch` — JSON body `{"url": "…", "name": "…", "source": "…",
//!   "referer": "…", "reject_html": true}` downloads the URL server-side
//!   (ureq + rustls). The request carries a browser-like User-Agent and, when
//!   the caller knows it, the capturing page as Referer — that is the point
//!   of the fallback: the extension lands here when its own request was
//!   refused by hotlink protection. `reject_html` refuses a text/html answer
//!   (a webpage, not a file) instead of landing one in the library.
//!
//! Every saved file gets a `<name>.meta.json` sidecar recording the source
//! URL; the importer writes it into `assets.source_url` and deletes both.
//! The server never touches the database: it only writes files, so it can
//! run on its own thread while the UI stays single-threaded.
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
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::config::AppConfig;

/// Default listen port for the collect service.
pub const DEFAULT_PORT: u16 = 23916;

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

/// The paths in `sources` that live in `inbox` — the files Trove put there
/// itself.
///
/// The inbox is the one directory whose files are Trove's to remove: a
/// screenshot or a collected page lands here and the library links it where it
/// stands, so deleting the record can only mean deleting the file. Everywhere
/// else a linked file belongs to the user and outlives its record, which is
/// why a purge has to ask this question first.
///
/// Both sides are canonicalized, so a relocated data root or a symlinked
/// `/tmp` still compares equal; a path that no longer resolves is compared as
/// written. `starts_with` is component-wise, so a sibling `incoming-old/`
/// never matches.
pub fn inbox_files(inbox: &Path, sources: Vec<PathBuf>) -> Vec<PathBuf> {
    let inbox = resolved(inbox);
    sources
        .into_iter()
        .filter(|source| resolved(source).starts_with(&inbox))
        .collect()
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
        ("POST", Some("/add")) => {
            if head.content_length > MAX_BODY {
                return respond(stream, 413, "{\"ok\":false,\"error\":\"body too large\"}");
            }
            let query = parse_query(&head.target);
            let name = query
                .get("filename")
                .cloned()
                .unwrap_or_else(|| "collected.bin".to_string());
            let source = query.get("source").cloned();
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
            match landing.finish(source.as_deref()) {
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
            match landing.finish(source.as_deref()) {
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
    fn finish(mut self, source: Option<&str>) -> std::io::Result<String> {
        let mut file = self.file.take().expect("open until finished");
        file.flush()?;
        // The rename is atomic against a concurrent reader; only fsync makes
        // the *contents* durable. Without it a crash can leave a visible file
        // with nothing in it.
        file.get_ref().sync_all()?;
        drop(file);
        if let Some(source) = source {
            let meta = serde_json::json!({ "source_url": source });
            std::fs::write(sidecar_path(&self.path), meta.to_string())?;
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
pub fn fetch_to_inbox(url: &str) -> Result<String, String> {
    // Checked before the landing exists, so a rejected URL leaves no trace.
    ensure_http(url)?;
    let name = suggested_name(url).unwrap_or_else(|| "collected.bin".to_string());
    let mut landing = Landing::new(&inbox_dir(), &name).map_err(|e| e.to_string())?;
    if let Err(e) = download(url, None, false, |chunk| landing.write(chunk)) {
        landing.abort();
        return Err(e);
    }
    landing.finish(Some(url)).map_err(|e| e.to_string())
}

/// The two schemes this service will fetch.
fn ensure_http(url: &str) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err("only http(s) URLs are supported".into())
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
) -> Result<(), String> {
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
        .call()
        .map_err(|e| e.to_string())?;
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
            return Err("the URL answered with a webpage (text/html), not a file".into());
        }
    }
    let mut reader = response.body_mut().with_config().limit(MAX_BODY).reader();
    let mut chunk = vec![0_u8; PUMP_CHUNK];
    loop {
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        sink(&chunk[..n]).map_err(|e| e.to_string())?;
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
<tr><td>POST</td><td><code>/add?filename=NAME&amp;source=URL</code></td><td>upload raw file bytes</td></tr>
<tr><td>POST</td><td><code>/fetch</code></td><td>server downloads <code>{{"url": "…"}}</code></td></tr>
</table>
<h3>Try it</h3>
<pre>curl --data-binary @image.png   'http://127.0.0.1:{port}/add?filename=image.png&amp;source=https://example.com/image'</pre>
<pre>curl -X POST http://127.0.0.1:{port}/fetch   -d '{{"url":"https://example.com/image.png"}}'</pre>
<p class="dim">Files land in the inbox and import automatically (source URL is kept).
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
}
