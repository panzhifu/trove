//! Local collect service: a tiny HTTP server on 127.0.0.1 that lands files
//! into the *inbox* directory, where the app's watcher picks them up and
//! imports them.
//!
//! Endpoints (the future browser extension speaks the first two):
//! - `GET  /ping` → `trove ok` (health check)
//! - `POST /add?filename=NAME&source=URL` — body is the raw file bytes
//!   (`curl --data-binary @img.png 'http://127.0.0.1:P/add?filename=a.png'`)
//! - `POST /fetch` — JSON body `{"url": "…", "name": "…", "source": "…"}`
//!   downloads the URL server-side (ureq + rustls).
//!
//! Every saved file gets a `<name>.meta.json` sidecar recording the source
//! URL; the importer writes it into `assets.source_url` and deletes both.
//! The server never touches the database: it only writes files, so it can
//! run on its own thread while the UI stays single-threaded.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use crate::config::AppConfig;

/// Default listen port for the collect service.
pub const DEFAULT_PORT: u16 = 23916;

/// Largest accepted upload/download (512 MB).
const MAX_BODY: u64 = 512 * 1024 * 1024;

/// Where collected files wait for the importer.
pub fn inbox_dir() -> PathBuf {
    AppConfig::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("inbox")
}

/// Start the server on a daemon thread. Returns the bound port, or `None`
/// when the port is taken (another Trove instance is probably listening).
pub fn spawn_server(port: u16) -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let inbox = inbox_dir();
    std::thread::Builder::new()
        .name("trove-collect".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // One thread per request keeps a slow download from blocking
                // the accept loop; requests are strictly local.
                let inbox = inbox.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &inbox);
                });
            }
        })
        .ok()?;
    Some(port)
}

struct Request {
    method: String,
    /// Path plus raw query string.
    target: String,
    body: Vec<u8>,
}

fn handle(mut stream: TcpStream, inbox: &Path) -> std::io::Result<()> {
    let request = match read_request(&mut stream) {
        Ok(Some(request)) => request,
        _ => {
            return respond(stream, 400, "{\"ok\":false,\"error\":\"bad request\"}");
        }
    };

    match (request.method.as_str(), request.target.split('?').next()) {
        ("GET", Some("/") | Some("")) => respond_html(stream, 200, &index_page()),
        ("GET", Some("/ping")) => respond(stream, 200, "trove ok"),
        ("POST", Some("/add")) => {
            let query = parse_query(&request.target);
            let name = query
                .get("filename")
                .cloned()
                .unwrap_or_else(|| "collected.bin".to_string());
            let source = query.get("source").cloned();
            match save(inbox, &name, source, &request.body) {
                Ok(saved) => respond(
                    stream,
                    200,
                    &format!("{{\"ok\":true,\"file\":\"{saved}\"}}"),
                ),
                Err(e) => respond(stream, 400, &format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
            }
        }
        ("POST", Some("/fetch")) => {
            let body: serde_json::Value = match serde_json::from_slice(&request.body) {
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
                .or_else(|| suggested_name(&url));
            let source = body
                .get("source")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| Some(url.clone()));
            match fetch(&url) {
                Ok(bytes) => match save(
                    inbox,
                    &name.unwrap_or_else(|| "collected.bin".to_string()),
                    source,
                    &bytes,
                ) {
                    Ok(saved) => respond(
                        stream,
                        200,
                        &format!("{{\"ok\":true,\"file\":\"{saved}\"}}"),
                    ),
                    Err(e) => respond(stream, 500, &format!("{{\"ok\":false,\"error\":\"{e}\"}}")),
                },
                Err(e) => respond(
                    stream,
                    502,
                    &format!("{{\"ok\":false,\"error\":\"download failed: {e}\"}}"),
                ),
            }
        }
        _ => respond(stream, 404, "{\"ok\":false,\"error\":\"not found\"}"),
    }
}

/// Read one HTTP request: head up to `\r\n\r\n`, then Content-Length bytes.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
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

    let mut content_length: usize = 0;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length as u64 > MAX_BODY {
        return Ok(None);
    }

    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(Some(Request {
        method,
        target,
        body,
    }))
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

/// Write the bytes plus sidecar under a collision-free name.
fn save(
    inbox: &Path,
    raw_name: &str,
    source: Option<String>,
    bytes: &[u8],
) -> Result<String, String> {
    std::fs::create_dir_all(inbox).map_err(|e| e.to_string())?;
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
    let name = if safe.trim().is_empty() {
        "collected.bin".to_string()
    } else {
        safe
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = inbox.join(format!("{nanos}-{name}"));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    if let Some(source) = source {
        let sidecar = inbox.join(format!("{}-{name}.meta.json", nanos));
        let sidecar = sidecar
            .file_name()
            .map(|n| inbox.join(n))
            .unwrap_or(sidecar);
        let meta = serde_json::json!({ "source_url": source });
        std::fs::write(sidecar, meta.to_string()).map_err(|e| e.to_string())?;
    }
    Ok(path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default())
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http(s) URLs are supported".into());
    }
    let mut response = ureq::get(url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| e.to_string())
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
<tr><td>GET</td><td><code>/ping</code></td><td>health check</td></tr>
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
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
