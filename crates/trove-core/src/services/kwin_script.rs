//! The window list, borrowed from KWin's own scripting engine.
//!
//! `org.kde.KWin` cannot enumerate windows: `queryWindowInfo` makes the
//! *user* pick one and `getWindowInfo` needs a handle you already have. A
//! picker that highlights the window under the cursor therefore has to get
//! the list from somewhere else, and the only other door is KWin's
//! scripting interface — `org.kde.kwin.Scripting.loadScript` runs a
//! JavaScript file inside the compositor, where `workspace.stackingOrder`
//! is the live window stack, and the script hands the result back through
//! `callDBus` to a session-bus name this process owns.
//!
//! Three consequences shape the module:
//!
//! * The generated script is a temp file with a process-unique service name
//!   baked in, because two Trove instances (a dev build and an installed
//!   one) must not fight over a single bus name.
//! * The round trip is bounded ([`TIMEOUT`]) and every failure is `None` to
//!   the caller: a picker that waits on a compositor is worse than a picker
//!   with no window snapping, and a bus that is not there is not a dialog.
//! * The list is a snapshot, taken once when the picker opens rather than
//!   per mouse move — the compositor's stack does not move while a
//!   borderless overlay holds the pointer.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

/// How long a window list may take before the picker carries on without it.
pub const TIMEOUT: Duration = Duration::from_millis(600);

/// Script plugin name; also the key `unloadScript` cleans up with.
const PLUGIN: &str = "trove-capture-window-list";

/// D-Bus interface the generated script calls back on. The name is
/// per-process (`…p<pid>`) so two Trove instances can both own a service.
const CALLBACK_INTERFACE: &str = "cn.trove.Capture";

/// One window, in workspace coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// KWin's `Window::internalId()` — the handle `CaptureWindow` wants.
    pub handle: String,
    pub caption: String,
    /// `resourceClass`, i.e. the application.
    pub app: String,
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// What the script sends: the fields as KWin reports them.
///
/// Geometry is read as `f64` because a JSON number is a JSON number; the
/// conversion to whole workspace pixels happens once, in [`parse`].
#[derive(serde::Deserialize)]
struct RawWindow {
    handle: String,
    #[serde(default)]
    caption: String,
    #[serde(default)]
    app: String,
    #[serde(default)]
    pid: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Turn the script's JSON into windows worth snapping to.
///
/// Drops anything Trove itself owns (own windows are hidden from the frozen
/// frame — offering them as candidates would highlight something the user
/// cannot see), anything degenerate, and anything that does not fit the
/// coordinate range the picker works in.
pub(crate) fn parse(json: &str) -> Option<Vec<WindowInfo>> {
    let raw: Vec<RawWindow> = serde_json::from_str(json).ok()?;
    let own_pid = std::process::id();
    Some(
        raw.into_iter()
            .filter(|w| w.pid != own_pid)
            .filter(|w| w.width >= 1.0 && w.height >= 1.0)
            .filter(|w| {
                w.x.is_finite()
                    && w.y.is_finite()
                    && w.x >= i32::MIN as f64
                    && w.y >= i32::MIN as f64
                    && w.x + w.width <= i32::MAX as f64
                    && w.y + w.height <= i32::MAX as f64
            })
            .map(|w| WindowInfo {
                handle: w.handle,
                caption: w.caption,
                app: w.app,
                pid: w.pid,
                x: w.x.round() as i32,
                y: w.y.round() as i32,
                width: w.width.round() as u32,
                height: w.height.round() as u32,
            })
            .collect(),
    )
}

/// The JavaScript KWin runs: read the window stack, hand it back.
///
/// `workspace.stackingOrder` (not `windowList`) because the picker needs to
/// know which window is on top; `frameGeometry` because that is what
/// `CaptureWindow` with `include-decoration` renders, so the highlight and
/// the capture agree on where the edges are.
fn script_source(service: &str) -> String {
    SCRIPT.replace("__SERVICE__", service)
}

const SCRIPT: &str = r#"(function () {
  const windows = workspace.stackingOrder.map(function (w) {
    const g = w.frameGeometry;
    return {
      handle: String(w.internalId),
      caption: w.caption || "",
      app: w.resourceClass || "",
      pid: w.pid || 0,
      x: g.x,
      y: g.y,
      width: g.width,
      height: g.height
    };
  });
  callDBus("__SERVICE__", "/capture", "cn.trove.Capture", "WindowList", JSON.stringify(windows));
})();
"#;

/// The callback KWin's script talks to.
struct Callback {
    sender: Mutex<mpsc::Sender<String>>,
}

#[zbus::interface(name = "cn.trove.Capture")]
impl Callback {
    /// zbus publishes this as `WindowList`, which is what the script calls.
    fn window_list(&self, json: String) {
        if let Ok(sender) = self.sender.lock() {
            let _ = sender.send(json);
        }
    }
}

/// Ask KWin for its window stack, in stacking order (bottom first).
///
/// `None` whenever the answer is not available in time or not available at
/// all — no session bus, no scripting interface, a script that failed to
/// load, a compositor that took too long. The caller treats that as "no
/// window snapping" and carries on.
pub fn window_list() -> Option<Vec<WindowInfo>> {
    let service = format!("{CALLBACK_INTERFACE}.p{}", std::process::id());
    let script = write_script(&service, &script_source(&service))?;
    let outcome = run(&service, &script);
    let _ = std::fs::remove_file(&script);
    match outcome {
        Ok(json) => parse(&json),
        Err(reason) => {
            tracing::debug!(reason, "kwin script: no window list");
            None
        }
    }
}

/// Write the generated script where KWin can read it.
fn write_script(service: &str, source: &str) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("{PLUGIN}-{service}.js"));
    match std::fs::write(&path, source) {
        Ok(()) => Some(path),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "kwin script: could not write");
            None
        }
    }
}

/// Serve the callback name, run the script once, wait for the answer.
fn run(service: &str, script: &std::path::Path) -> Result<String, String> {
    use zbus::blocking::connection::Builder;

    let (sender, receiver) = mpsc::channel();
    // The connection owns both the well-known name and the object that
    // receives the reply, so it has to outlive the wait below.
    let connection = Builder::session()
        .map_err(|e| format!("session bus: {e}"))?
        .name(service.to_string())
        .map_err(|e| format!("claiming {service}: {e}"))?
        .serve_at(
            "/capture",
            Callback {
                sender: Mutex::new(sender),
            },
        )
        .map_err(|e| format!("serving {service}: {e}"))?
        .build()
        .map_err(|e| format!("{service}: {e}"))?;

    let scripting = zbus::blocking::Proxy::new(
        &connection,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .map_err(|e| format!("scripting proxy: {e}"))?;

    // A previous run leaves the plugin loaded with the same file path; drop
    // it first so this call always runs the script it just wrote.
    let _ = scripting.call_method("unloadScript", &(PLUGIN,));
    scripting
        .call_method(
            "loadScript",
            &(script.to_string_lossy().to_string(), PLUGIN),
        )
        .map_err(|e| format!("loadScript: {e}"))?;
    scripting
        .call_method("start", &())
        .map_err(|e| format!("scripting start: {e}"))?;

    let outcome = receiver
        .recv_timeout(TIMEOUT)
        .map_err(|e| format!("no answer within {TIMEOUT:?}: {e}"));

    // Both of these are housekeeping: a failure here must not lose the
    // answer that already arrived.
    if let Err(error) = scripting.call_method("unloadScript", &(PLUGIN,)) {
        tracing::debug!(%error, "kwin script: could not unload");
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_names_the_service_it_calls_back_on() {
        let source = script_source("cn.trove.Capture.p42");
        assert!(source.contains("\"cn.trove.Capture.p42\""));
        assert!(source.contains("workspace.stackingOrder"));
        assert!(source.contains("WindowList"));
        // Nothing was left behind by the template.
        assert!(!source.contains("__SERVICE__"));
    }

    #[test]
    fn trove_own_windows_are_not_candidates() {
        let json = format!(
            r#"[{{"handle":"mine","x":0,"y":0,"width":100,"height":100,"pid":{}}}]"#,
            std::process::id()
        );
        assert_eq!(parse(&json).unwrap().len(), 0);
    }

    #[test]
    fn degenerate_and_hostile_geometry_is_dropped() {
        let json = r#"[
            {"handle":"ok","x":1,"y":2,"width":30,"height":40,"pid":9},
            {"handle":"flat","x":1,"y":2,"width":0,"height":40,"pid":9},
            {"handle":"huge","x":1,"y":2,"width":1e30,"height":40,"pid":9}
        ]"#;
        let windows = parse(json).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].handle, "ok");
        assert_eq!((windows[0].x, windows[0].y), (1, 2));
        assert_eq!((windows[0].width, windows[0].height), (30, 40));
    }

    #[test]
    fn a_broken_payload_is_no_answer_rather_than_a_panic() {
        assert!(parse("not json").is_none());
        assert!(parse("{}").is_none());
        // Missing geometry fields: the entry is unusable, the list is not.
        assert!(parse(r#"[{"handle":"x"}]"#).is_none());
        assert_eq!(parse("[]").unwrap().len(), 0);
    }

    #[test]
    fn fractional_geometry_rounds_to_whole_pixels() {
        let windows =
            parse(r#"[{"handle":"h","x":10.6,"y":20.4,"width":30.5,"height":40.5,"pid":9}]"#)
                .unwrap();
        assert_eq!(
            (
                windows[0].x,
                windows[0].y,
                windows[0].width,
                windows[0].height
            ),
            (11, 20, 31, 41)
        );
    }

    #[test]
    fn a_missing_caption_or_pid_still_yields_a_window() {
        let windows =
            parse(r#"[{"handle":"h","x":0,"y":0,"width":10,"height":10,"caption":"Hi"}]"#).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].caption, "Hi");
        assert_eq!(windows[0].app, "");
        assert_eq!(windows[0].pid, 0);
    }
}
