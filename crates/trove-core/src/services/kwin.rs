//! KWin (KDE Plasma) capture over D-Bus.
//!
//! KWin deliberately implements neither `wlr-screencopy` nor
//! `ext-image-copy-capture-v1`, so on Plasma no third-party capture
//! protocol exists — `xcap` and friends can never work there. What KWin
//! does expose is `org.kde.KWin.ScreenShot2`: the caller passes the write
//! end of a pipe plus a small options map, KWin renders the frame, writes
//! the raw pixels into the pipe and answers with a metadata map
//! (`type` = `"raw"`, `format` = a `QImage::Format` value, `width`,
//! `height`, `stride`). This module is that client, which makes capture
//! work in-process on KDE without shelling out to Spectacle.
//!
//! KWin gates the interface behind KDE's restricted-D-Bus mechanism, and
//! the check is picky about identity: it resolves the caller by reading
//! `/proc/<pid>/exe`, then looks for a desktop entry whose `Exec` first
//! token has the *same canonical path* (`utils/serviceutils.h`), and only
//! then reads `X-KDE-DBUS-Restricted-Interfaces` from it. So a desktop
//! entry has to name the exact binary: an installed `trove-app` entry
//! does not authorize a `target/debug/trove-app` run (see
//! `packaging/linux/trove.desktop`, which uses an absolute path for this
//! reason, and the hidden `trove-dev.desktop` that points at the debug
//! binary). Anything else answers
//! `org.kde.KWin.ScreenShot2.Error.NoAuthorized` — which is a
//! desktop-entry problem, not a problem in this module. The compositor
//! also honours `KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1`, but that has to
//! be set for KWin's own process, not for us.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{Fd, OwnedFd, Value};

const DESTINATION: &str = "org.kde.KWin";
const OBJECT_PATH: &str = "/org/kde/KWin/ScreenShot2";
const INTERFACE: &str = "org.kde.KWin.ScreenShot2";

/// Capture the whole workspace (all outputs as KWin composes them) and
/// write it to `dest` as PNG. `Err` carries a reason suitable for the
/// caller's fallback chain — a missing KWin (non-KDE session) is not an
/// error worth surfacing, just a fallthrough.
pub fn capture_workspace(dest: &Path) -> Result<(), String> {
    let image = capture_workspace_image()?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            tracing::error!(
                dir = %parent.display(),
                error = %e,
                "could not create output dir"
            );
            e.to_string()
        })?;
    }
    image.save(dest).map_err(|e| {
        tracing::error!(dest = %dest.display(), error = %e, "kwin: could not save png");
        format!("{}: {e}", dest.display())
    })?;
    tracing::info!(dest = %dest.display(), "kwin: png written");
    Ok(())
}

/// [`capture_workspace`] without the file: the frame itself, for callers
/// that crop it (the region picker) before anything is written.
pub fn capture_workspace_image() -> Result<image::RgbaImage, String> {
    let started = std::time::Instant::now();
    let (mut reader, writer) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
    let connection = Connection::session().map_err(|e| format!("session bus: {e}"))?;
    let proxy = Proxy::new(&connection, DESTINATION, OBJECT_PATH, INTERFACE)
        .map_err(|e| format!("KWin ScreenShot2 proxy: {e}"))?;

    let options: HashMap<String, Value> = HashMap::from([
        ("include-cursor".to_string(), Value::from(false)),
        ("native-resolution".to_string(), Value::from(false)),
    ]);
    // The write end travels to KWin via SCM_RIGHTS; our own copy is dropped
    // when the call returns (KWin has written by then), which is what lets
    // the read below see EOF.
    let writer: std::os::fd::OwnedFd = writer.into();
    let pipe = Fd::from(OwnedFd::from(writer));
    let body = (options, pipe);
    let reply = proxy
        .call_method("CaptureWorkspace", &body)
        .map_err(|e| format!("CaptureWorkspace call: {e}"))?;
    // Drop our copy of the write end now that the reply is in: KWin has
    // written the frame by this point, so the read below reaches EOF.
    drop(body);
    let reply_body = reply.body();
    let results: HashMap<String, Value> = reply_body
        .deserialize()
        .map_err(|e| format!("KWin metadata: {e}"))?;

    let width = meta_u32(&results, "width")?;
    let height = meta_u32(&results, "height")?;
    let stride = meta_u32(&results, "stride")? as usize;
    let format = meta_u32(&results, "format")?;
    tracing::debug!(
        kind = results
            .get("type")
            .and_then(|v| v.downcast_ref::<&str>().ok()),
        width,
        height,
        stride,
        format,
        "kwin: capture metadata"
    );

    let mut raw = Vec::new();
    reader
        .read_to_end(&mut raw)
        .map_err(|e| format!("reading the capture pipe: {e}"))?;
    tracing::info!(
        bytes = raw.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "kwin: frame data read"
    );

    let rgba = to_rgba(&raw, width, height, stride, format)?;
    image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| "kwin: captured buffer does not match its dimensions".to_string())
}

/// One `u` entry of the metadata map.
fn meta_u32(results: &HashMap<String, Value>, key: &str) -> Result<u32, String> {
    let value = results
        .get(key)
        .ok_or_else(|| format!("KWin metadata has no `{key}`"))?;
    u32::try_from(value).map_err(|e| format!("KWin metadata `{key}`: {e}"))
}

/// Repack KWin's raw rows into tightly packed RGBA8.
///
/// `format` is a `QImage::Format` value. KWin hands back either the
/// ARGB32 family (little-endian bytes B,G,R,A) or the RGBA8888 family
/// straight RGBA; premultiplied variants are treated as straight, which is
/// exact for the opaque frames a workspace capture produces.
fn to_rgba(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: u32,
) -> Result<Vec<u8>, String> {
    let order = match format {
        4..=6 => [2usize, 1, 0, 3],   // RGB32 / ARGB32(_Premultiplied)
        17..=20 => [0usize, 1, 2, 3], // RGBX8888 / RGBA8888(_Premultiplied)
        other => {
            return Err(format!("kwin: unexpected QImage format {other}"));
        }
    };
    let row_bytes = width as usize * 4;
    if stride < row_bytes || raw.len() < stride * height as usize {
        return Err(format!(
            "kwin: truncated frame ({} bytes for {width}x{height} stride {stride})",
            raw.len()
        ));
    }
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for row in raw.chunks_exact(stride).take(height as usize) {
        for pixel in row[..row_bytes].as_chunks::<4>().0 {
            out.extend_from_slice(&[
                pixel[order[0]],
                pixel[order[1]],
                pixel[order[2]],
                pixel[order[3]],
            ]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::to_rgba;

    #[test]
    fn argb32_rows_are_swapped_to_rgba() {
        // One ARGB32 pixel (B,G,R,A in memory) with a padded row.
        let raw = [30u8, 20, 10, 255, 0, 0];
        let rgba = to_rgba(&raw, 1, 1, 6, 5).unwrap();
        assert_eq!(rgba, vec![10, 20, 30, 255]);
    }

    #[test]
    fn rgba8888_rows_pass_through() {
        let raw = [10u8, 20, 30, 255];
        let rgba = to_rgba(&raw, 1, 1, 4, 18).unwrap();
        assert_eq!(rgba, vec![10, 20, 30, 255]);
    }

    #[test]
    fn unknown_format_and_truncated_buffers_are_errors() {
        assert!(to_rgba(&[0; 4], 1, 1, 4, 99).is_err());
        assert!(to_rgba(&[0; 3], 1, 1, 4, 18).is_err());
    }
}
