//! KWin (KDE Plasma) capture over D-Bus.
//!
//! KWin deliberately implements neither `wlr-screencopy` nor
//! `ext-image-copy-capture-v1`, so on Plasma no third-party capture
//! protocol exists — `xcap` and friends can never work there. What KWin
//! does expose is `org.kde.KWin.ScreenShot2`: the caller passes the write
//! end of a pipe plus a small options map, KWin renders the frame, writes
//! the raw pixels into the pipe and answers with a metadata map
//! (`type` = `"raw"`, `format` = a `QImage::Format` value, `width`,
//! `height`, `stride`, plus `windowId`, `screen` and `scale` on newer
//! compositors). This module is that client, which makes capture work
//! in-process on KDE without shelling out to Spectacle.
//!
//! The interface has one method per *target* — the workspace, a screen, a
//! window, a rectangle — and this module exposes exactly those, because
//! capturing at the target's own granularity is both sharper (KWin can
//! render at native resolution instead of a downscaled workspace frame)
//! and cheaper than cropping a whole frame after the fact:
//!
//! | function here | ScreenShot2 method | target |
//! | --- | --- | --- |
//! | [`capture_workspace_image`] | `CaptureWorkspace` | every output, as composed |
//! | [`capture_screen_image`] | `CaptureScreen` | one output by `QScreen::name()` |
//! | [`capture_active_screen_image`] | `CaptureActiveScreen` | the output with focus |
//! | [`capture_active_window_image`] | `CaptureActiveWindow` | the focused window |
//! | [`capture_window_image`] | `CaptureWindow` | one window by its `internalId` |
//! | [`capture_area_image`] | `CaptureArea` | a rectangle in workspace coordinates |
//! | [`capture_interactive_window_image`] | `CaptureInteractive(0)` | whatever the user clicks |
//!
//! Two option keys are load-bearing. `native-resolution` decides whether
//! the frame comes back at device pixels or at logical size — the region
//! picker depends on the logical default so that its frozen frame, the
//! overlay's own coordinates and the workspace rectangle all live in one
//! space. `hide-caller-windows` (compositor version 5 and up) defaults to
//! *true* inside KWin, which is why a workspace capture never contains
//! Trove's own window; capturing Trove's own panels instead means sending
//! `false`, and only when [`version`] says the compositor knows the key.
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
use std::sync::OnceLock;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{Fd, OwnedFd, Value};

const DESTINATION: &str = "org.kde.KWin";
const OBJECT_PATH: &str = "/org/kde/KWin/ScreenShot2";
const INTERFACE: &str = "org.kde.KWin.ScreenShot2";

/// How a capture attempt failed.
///
/// The caller needs the difference between "there is no KWin here" (fall
/// through to another backend) and "the user dismissed KWin's own picker"
/// (report nothing), so this cannot collapse into a plain string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No compositor answered, or the interface is not ours to use: a
    /// non-KDE session, a missing desktop entry, a refused call.
    Unavailable(String),
    /// The user cancelled KWin's own interactive picker.
    Cancelled,
    /// KWin was reachable and the capture still failed.
    Failed(String),
}

impl Failure {
    /// The bare reason, for callers that build their own message.
    pub fn message(&self) -> String {
        match self {
            Failure::Unavailable(reason) | Failure::Failed(reason) => reason.clone(),
            Failure::Cancelled => "cancelled".into(),
        }
    }

    /// `kwin: <reason>` for callers that concatenate a fallback chain.
    pub fn labelled(&self) -> String {
        format!("kwin: {}", self.message())
    }
}

/// The optional keys a caller can ask for.
///
/// Every field is an `Option` because KWin ignores a key the method does
/// not know, but this module should not pretend to have an opinion: only
/// what is `Some` goes onto the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub cursor: bool,
    pub native: bool,
    pub decoration: Option<bool>,
    pub shadow: Option<bool>,
    pub hide_caller: Option<bool>,
}

impl Options {
    /// Nothing but the compositor's own defaults.
    pub const fn defaults() -> Self {
        Self {
            cursor: false,
            native: false,
            decoration: None,
            shadow: None,
            hide_caller: None,
        }
    }
}

/// Capture the whole workspace (all outputs as KWin composes them).
pub fn capture_workspace_image() -> Result<image::RgbaImage, Failure> {
    // `hide-caller-windows` already defaults to true inside KWin, so
    // Trove's own window stays out of the frame; say it explicitly so a
    // future compositor default cannot change what the picker shows.
    let options = Options {
        hide_caller: Some(true),
        ..Options::defaults()
    };
    call("CaptureWorkspace", |fd| {
        (option_map(options, version()), fd)
    })
}

/// Capture one output by `QScreen::name()`.
pub fn capture_screen_image(name: &str, options: Options) -> Result<image::RgbaImage, Failure> {
    let name = name.to_string();
    call("CaptureScreen", move |fd| {
        (name, option_map(options, version()), fd)
    })
}

/// Capture the output that currently has focus.
pub fn capture_active_screen_image(options: Options) -> Result<image::RgbaImage, Failure> {
    call("CaptureActiveScreen", move |fd| {
        (option_map(options, version()), fd)
    })
}

/// Capture the focused window.
pub fn capture_active_window_image(options: Options) -> Result<image::RgbaImage, Failure> {
    call("CaptureActiveWindow", move |fd| {
        (option_map(options, version()), fd)
    })
}

/// Capture one window by the handle KWin knows it by — the string form of
/// `Window::internalId()`, which is what `CaptureWindow` resolves through
/// `workspace()->findWindow(QUuid(handle))`. Any other spelling answers
/// `InvalidWindow`.
pub fn capture_window_image(handle: &str, options: Options) -> Result<image::RgbaImage, Failure> {
    let handle = handle.to_string();
    call("CaptureWindow", move |fd| {
        (handle, option_map(options, version()), fd)
    })
}

/// Capture a rectangle in workspace coordinates (`CaptureArea`). Passing
/// `native` renders the crop at device resolution instead of slicing a
/// logical frame.
pub fn capture_area_image(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    options: Options,
) -> Result<image::RgbaImage, Failure> {
    call("CaptureArea", move |fd| {
        (x, y, width, height, option_map(options, version()), fd)
    })
}

/// Let KWin run its own window picker (`CaptureInteractive` with kind 0):
/// the cursor becomes a targeting reticule, KWin highlights the window
/// under it and draws its own prompt, and the chosen window comes back as
/// a frame. The interaction belongs to the compositor, so its prompt is
/// not ours to translate — [`Failure::Cancelled`] is the normal way out.
pub fn capture_interactive_window_image(options: Options) -> Result<image::RgbaImage, Failure> {
    call("CaptureInteractive", move |fd| {
        (0u32, option_map(options, version()), fd)
    })
}

/// The name of the output that currently has focus (`org.kde.KWin`'s
/// `activeOutputName`), ready to be handed to [`capture_screen_image`].
pub fn active_output_name() -> Result<String, Failure> {
    let connection = connection()?;
    let proxy = Proxy::new(&connection, DESTINATION, "/KWin", "org.kde.KWin")
        .map_err(|e| classify("org.kde.KWin proxy", e))?;
    let reply = proxy
        .call_method("activeOutputName", &())
        .map_err(|e| classify("activeOutputName", e))?;
    let body = reply.body();
    body.deserialize::<String>()
        .map_err(|e| Failure::Failed(format!("activeOutputName reply: {e}")))
}

/// The compositor's `ScreenShot2.Version`, cached for the session: it only
/// changes when KWin restarts, and every capture would otherwise pay a
/// round trip for it. `0` means "unknown", which switches off the keys
/// that need a minimum version.
pub fn version() -> u32 {
    static VERSION: OnceLock<u32> = OnceLock::new();
    *VERSION.get_or_init(|| query_version().unwrap_or(0))
}

fn query_version() -> Option<u32> {
    let connection = Connection::session().ok()?;
    let proxy = Proxy::new(&connection, DESTINATION, OBJECT_PATH, INTERFACE).ok()?;
    proxy.get_property::<u32>("Version").ok()
}

/// Build a call's options map.
///
/// Pure, and split out from [`Options`] for one reason: the version gate.
/// `hide-caller-windows` only exists from compositor version 5 on, and a
/// key an older KWin does not know is not a harmless extra — it is a
/// rejected call. `version == 0` (unknown) sends nothing, which keeps the
/// compositor's own default in force, exactly as if this module never
/// mentioned the key.
pub(crate) fn option_map(options: Options, version: u32) -> HashMap<String, Value<'static>> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("include-cursor".into(), Value::from(options.cursor));
    map.insert("native-resolution".into(), Value::from(options.native));
    if let Some(decoration) = options.decoration {
        map.insert("include-decoration".into(), Value::from(decoration));
    }
    if let Some(shadow) = options.shadow {
        map.insert("include-shadow".into(), Value::from(shadow));
    }
    if let Some(hide) = options.hide_caller
        && version >= 5
    {
        map.insert("hide-caller-windows".into(), Value::from(hide));
    }
    map
}

/// Run one capture: build the pipe, let `body` put the write end into the
/// call, then read back what KWin wrote.
///
/// The write end travels to KWin via SCM_RIGHTS. Our own copy is dropped
/// when `body` goes out of scope right after the call — KWin has written
/// the frame by then — which is what lets the read below see EOF.
fn call<B, F>(method: &str, body: F) -> Result<image::RgbaImage, Failure>
where
    F: FnOnce(Fd<'static>) -> B,
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let started = std::time::Instant::now();
    let (mut reader, writer) =
        std::io::pipe().map_err(|e| Failure::Unavailable(format!("pipe: {e}")))?;
    let connection = connection()?;
    let proxy = Proxy::new(&connection, DESTINATION, OBJECT_PATH, INTERFACE)
        .map_err(|e| classify("KWin ScreenShot2 proxy", e))?;

    let writer: std::os::fd::OwnedFd = writer.into();
    let body = body(Fd::from(OwnedFd::from(writer)));
    let reply = proxy
        .call_method(method, &body)
        .map_err(|e| classify(method, e))?;
    drop(body);

    let reply_body = reply.body();
    let results: HashMap<String, Value<'_>> = reply_body
        .deserialize()
        .map_err(|e| Failure::Failed(format!("KWin metadata: {e}")))?;

    let width = meta_u32(&results, "width")?;
    let height = meta_u32(&results, "height")?;
    let stride = meta_u32(&results, "stride")? as usize;
    let format = meta_u32(&results, "format")?;
    tracing::debug!(
        method,
        kind = results
            .get("type")
            .and_then(|v| v.downcast_ref::<&str>().ok()),
        window_id = results
            .get("windowId")
            .and_then(|v| v.downcast_ref::<&str>().ok()),
        screen = results
            .get("screen")
            .and_then(|v| v.downcast_ref::<&str>().ok()),
        scale = results
            .get("scale")
            .and_then(|v| v.downcast_ref::<f64>().ok()),
        width,
        height,
        stride,
        format,
        "kwin: capture metadata"
    );

    let mut raw = Vec::new();
    reader
        .read_to_end(&mut raw)
        .map_err(|e| Failure::Failed(format!("reading the capture pipe: {e}")))?;
    tracing::info!(
        method,
        bytes = raw.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "kwin: frame data read"
    );

    let rgba = to_rgba(&raw, width, height, stride, format).map_err(Failure::Failed)?;
    image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
        Failure::Failed("kwin: captured buffer does not match its dimensions".into())
    })
}

fn connection() -> Result<Connection, Failure> {
    Connection::session().map_err(|e| Failure::Unavailable(format!("session bus: {e}")))
}

/// One `u` entry of the metadata map.
fn meta_u32(results: &HashMap<String, Value<'_>>, key: &str) -> Result<u32, Failure> {
    let value = results
        .get(key)
        .ok_or_else(|| Failure::Failed(format!("KWin metadata has no `{key}`")))?;
    u32::try_from(value).map_err(|e| Failure::Failed(format!("KWin metadata `{key}`: {e}")))
}

/// Sort a D-Bus error into "not for us", "the user said no" or "it broke".
///
/// A missing service or interface means there is no KWin to talk to, which
/// is a fall-through rather than a failure; `NoAuthorized` is a desktop
/// entry that does not name this binary, and the external toolchain is the
/// right answer there too (the reason still rides along in the message).
fn classify(method: &str, error: zbus::Error) -> Failure {
    let message = if method.is_empty() {
        error.to_string()
    } else {
        format!("{method}: {error}")
    };
    match &error {
        zbus::Error::MethodError(name, _, _) => classify_error_name(name.as_str(), message),
        _ => Failure::Unavailable(message),
    }
}

/// The name half of [`classify`], pure so it can be tested without a bus.
fn classify_error_name(name: &str, message: String) -> Failure {
    if name.ends_with("Cancelled") || name.contains("UserCancel") {
        return Failure::Cancelled;
    }
    if name.contains("NoAuthorized") || name.contains("InvalidWindow") {
        return Failure::Unavailable(message);
    }
    Failure::Failed(message)
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
    use super::*;

    fn value_bool(map: &HashMap<String, Value<'static>>, key: &str) -> Option<bool> {
        map.get(key).and_then(|v| bool::try_from(v).ok())
    }

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

    #[test]
    fn cursor_and_native_are_always_on_the_wire() {
        let map = option_map(Options::defaults(), 5);
        assert_eq!(value_bool(&map, "include-cursor"), Some(false));
        assert_eq!(value_bool(&map, "native-resolution"), Some(false));
    }

    #[test]
    fn unset_optional_keys_are_left_out() {
        let map = option_map(Options::defaults(), 5);
        assert!(!map.contains_key("include-decoration"));
        assert!(!map.contains_key("include-shadow"));
        assert!(!map.contains_key("hide-caller-windows"));

        let set = option_map(
            Options {
                decoration: Some(true),
                shadow: Some(false),
                hide_caller: Some(true),
                ..Options::defaults()
            },
            5,
        );
        assert_eq!(value_bool(&set, "include-decoration"), Some(true));
        assert_eq!(value_bool(&set, "include-shadow"), Some(false));
        assert_eq!(value_bool(&set, "hide-caller-windows"), Some(true));
    }

    #[test]
    fn hide_caller_windows_needs_compositor_version_five() {
        let wanted = Options {
            hide_caller: Some(false),
            ..Options::defaults()
        };
        // 0 is "version unknown": the compositor's own default (true) stays
        // in force, which is what a workspace capture wants.
        assert!(!option_map(wanted, 0).contains_key("hide-caller-windows"));
        assert!(!option_map(wanted, 4).contains_key("hide-caller-windows"));
        let five = option_map(wanted, 5);
        assert_eq!(value_bool(&five, "hide-caller-windows"), Some(false));
    }

    #[test]
    fn cancellations_are_told_apart_from_failures() {
        assert_eq!(
            classify_error_name("org.kde.KWin.ScreenShot2.Error.Cancelled", "x".into()),
            Failure::Cancelled
        );
        assert_eq!(
            classify_error_name("org.kde.KWin.Error.UserCancel", "x".into()),
            Failure::Cancelled
        );
        assert!(matches!(
            classify_error_name("org.kde.KWin.ScreenShot2.Error.NoAuthorized", "x".into()),
            Failure::Unavailable(_)
        ));
        assert!(matches!(
            classify_error_name("org.kde.KWin.ScreenShot2.Error.InvalidWindow", "x".into()),
            Failure::Unavailable(_)
        ));
        assert!(matches!(
            classify_error_name("org.kde.KWin.ScreenShot2.Error.InvalidOption", "x".into()),
            Failure::Failed(_)
        ));
    }

    #[test]
    fn a_missing_interface_is_a_fall_through_not_a_failure() {
        assert!(matches!(
            classify("CaptureWorkspace", zbus::Error::InterfaceNotFound),
            Failure::Unavailable(_)
        ));
    }
}
