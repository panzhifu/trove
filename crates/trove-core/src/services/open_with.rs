//! Hand a library file to an external application.
//!
//! On Linux the candidate list comes from the XDG application directories:
//! every `.desktop` entry that declares a matching `MimeType` is offered, so
//! the menu reflects what is actually installed instead of a hard-coded
//! list of editors. macOS and Windows have no such registry to read from
//! disk, so they only get the platform's default handler — the same thing
//! `xdg-open` does on Linux.
//!
//! Nothing here links a desktop toolkit: `.desktop` files are plain INI,
//! and the `Exec` line is expanded by hand per the Desktop Entry
//! specification.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// One application that can be asked to open a file.
#[derive(Debug, Clone)]
pub struct Application {
    /// Desktop-file id (`org.gimp.GIMP.desktop`), unique within the menu.
    pub id: String,
    /// `Name=`, shown in the menu.
    pub name: String,
    /// Raw `Exec=` line; expanded with the file path at launch time.
    pub exec: String,
    /// `Icon=`, needed to expand `%i` correctly.
    pub icon: Option<String>,
    /// Lower-cased `MimeType=` entries.
    pub mimes: Vec<String>,
}

/// Applications that can open `mime`, best match first.
///
/// A declared exact type beats a `type/*` wildcard; ties break on the
/// display name so the order is stable across sessions.
pub fn applications_for(mime: &str) -> Vec<Application> {
    rank(&catalogue(), mime)
}

/// The same ranking, on a caller-supplied catalogue (tests).
fn rank(apps: &[Application], mime: &str) -> Vec<Application> {
    let mime = mime.to_ascii_lowercase();
    let generic = mime
        .split_once('/')
        .map(|(family, _)| format!("{family}/*"))
        .unwrap_or_default();

    let mut ranked: Vec<(u8, &Application)> = apps
        .iter()
        .filter_map(|app| {
            let exact = app.mimes.contains(&mime);
            let wildcard = !generic.is_empty() && app.mimes.contains(&generic);
            match (exact, wildcard) {
                (true, _) => Some((0, app)),
                (false, true) => Some((1, app)),
                _ => None,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
    });
    ranked.into_iter().map(|(_, app)| app.clone()).collect()
}

/// Every installed application, parsed once per process.
fn catalogue() -> Vec<Application> {
    let mut cache = CATALOGUE.lock().unwrap_or_else(|e| e.into_inner());
    cache.get_or_insert_with(scan).clone()
}

/// Parsed desktop entries, filled on first use.
static CATALOGUE: Mutex<Option<Vec<Application>>> = Mutex::new(None);

/// Drop the cache so the next lookup rescans the disk. Apps installed while
/// trove is running would otherwise stay invisible until a restart.
pub fn reload_catalogue() {
    let mut cache = CATALOGUE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = None;
}

#[cfg(target_os = "linux")]
fn scan() -> Vec<Application> {
    // Earlier directories win: XDG_DATA_HOME overrides the system ones.
    // A desktop-file id is unique, so the first parse of an id is kept.
    let mut by_id: BTreeMap<String, Application> = BTreeMap::new();
    for dir in application_dirs() {
        let mut files = Vec::new();
        collect_desktop_files(&dir, &mut files);
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(&dir) else {
                continue;
            };
            let id = relative.to_string_lossy().replace('/', "-");
            if by_id.contains_key(&id) {
                continue;
            }
            if let Some(app) = parse_desktop(&id, &text) {
                by_id.insert(id, app);
            }
        }
    }
    by_id.into_values().collect()
}

#[cfg(not(target_os = "linux"))]
fn scan() -> Vec<Application> {
    Vec::new()
}

/// `$XDG_DATA_HOME/applications` plus `$XDG_DATA_DIRS/*/applications`, in
/// precedence order.
#[cfg(target_os = "linux")]
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")));
    if let Some(home) = data_home {
        dirs.push(home.join("applications"));
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|entry| !entry.trim().is_empty())
            .map(|entry| PathBuf::from(entry).join("applications")),
    );
    dirs
}

/// Desktop entries may sit in sub-directories (`applications/kde4/`).
#[cfg(target_os = "linux")]
fn collect_desktop_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_desktop_files(&path, out),
            Ok(_) if path.extension().and_then(|e| e.to_str()) == Some("desktop") => out.push(path),
            _ => {}
        }
    }
}

/// Parse one `.desktop` file. Returns `None` for anything that must not
/// show up in a menu: non-application entries, `NoDisplay`/`Hidden`, and
/// terminal programs (we have no terminal to attach them to).
fn parse_desktop(id: &str, text: &str) -> Option<Application> {
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut mimes = Vec::new();
    let mut kind = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut terminal = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_entry = group == "Desktop Entry";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        // Localized keys (`Name[de]`) are ignored: the menu is better off
        // consistent than translated for a handful of entries.
        if key.contains('[') {
            continue;
        }
        match key {
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "Icon" => icon = Some(value.to_string()),
            "Type" => kind = Some(value.to_string()),
            "MimeType" => {
                mimes = value
                    .split(';')
                    .map(|m| m.trim().to_ascii_lowercase())
                    .filter(|m| !m.is_empty())
                    .collect()
            }
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "Terminal" => terminal = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    if kind.as_deref() != Some("Application") || no_display || hidden || terminal {
        return None;
    }
    let (name, exec) = (name?, exec?);
    if name.is_empty() || exec.trim().is_empty() {
        return None;
    }
    Some(Application {
        id: id.to_string(),
        name,
        exec,
        icon,
        mimes,
    })
}

/// Launch `app` on `file`, detached. The child keeps running if trove exits.
pub fn launch(app: &Application, file: &Path) -> std::io::Result<()> {
    let argv = expand_exec(&app.exec, &app.name, app.icon.as_deref(), file);
    let Some((program, args)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the desktop entry has an empty Exec line",
        ));
    };
    spawn(program, args)
}

/// Open `file` with the desktop's default handler.
pub fn launch_default(file: &Path) -> std::io::Result<()> {
    let path = file.to_string_lossy().into_owned();
    #[cfg(target_os = "linux")]
    {
        spawn("xdg-open", &[path])
    }
    #[cfg(target_os = "macos")]
    {
        spawn("open", &[path])
    }
    #[cfg(target_os = "windows")]
    {
        // `start` is a cmd builtin; the empty title keeps a quoted path from
        // being read as the window title.
        spawn(
            "cmd",
            &["/C".to_string(), "start".to_string(), String::new(), path],
        )
    }
}

/// Spawn a detached child: stdio is discarded so the launched application
/// never inherits trove's pipes and keeps running after trove exits.
fn spawn(program: &str, args: &[String]) -> std::io::Result<()> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Expand an `Exec` line into an argv for one file.
///
/// Implements the field codes that matter for opening a file (`%f %F %u %U`,
/// `%i`, `%c`, `%%`); the deprecated ones are dropped, as the specification
/// requires. An entry with no file code at all gets the path appended — the
/// spec says the file is then passed as if `%f` were used.
pub fn expand_exec(exec: &str, name: &str, icon: Option<&str>, file: &Path) -> Vec<String> {
    let path = file.to_string_lossy().into_owned();
    let mut argv = Vec::new();
    let mut took_file = false;

    for token in tokenize(exec) {
        match token.as_str() {
            "%i" => {
                if let Some(icon) = icon {
                    argv.push("--icon".to_string());
                    argv.push(icon.to_string());
                }
                continue;
            }
            // The path of the desktop file itself; trove has no use for it.
            "%k" => continue,
            _ => {}
        }

        let mut expanded = String::new();
        let mut chars = token.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                expanded.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => expanded.push('%'),
                Some('f' | 'F' | 'u' | 'U') => {
                    expanded.push_str(&path);
                    took_file = true;
                }
                Some('c') => expanded.push_str(name),
                // Deprecated or unknown: drop the code, keep the text.
                Some(_) => {}
                None => expanded.push('%'),
            }
        }
        if !expanded.is_empty() {
            argv.push(expanded);
        }
    }

    if !took_file {
        argv.push(path);
    }
    argv
}

/// Split a command line into arguments, honouring quotes and backslash
/// escapes (`Exec` values are a single line, so no newline handling).
fn tokenize(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = exec.chars();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) if c == '\\' => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c == '\\' => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                    started = true;
                }
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Application, expand_exec, parse_desktop, rank};
    use std::path::Path;

    fn app(id: &str, name: &str, exec: &str, mimes: &[&str]) -> Application {
        Application {
            id: id.to_string(),
            name: name.to_string(),
            exec: exec.to_string(),
            icon: None,
            mimes: mimes.iter().map(|m| m.to_string()).collect(),
        }
    }

    #[test]
    fn a_file_code_becomes_the_path() {
        let argv = expand_exec("gimp-2.10 %U", "GIMP", None, Path::new("/tmp/a b.png"));
        assert_eq!(argv, vec!["gimp-2.10", "/tmp/a b.png"]);
    }

    #[test]
    fn an_entry_without_a_file_code_gets_the_path_appended() {
        let argv = expand_exec("eog", "Eye of GNOME", None, Path::new("/tmp/a.png"));
        assert_eq!(argv, vec!["eog", "/tmp/a.png"]);
    }

    #[test]
    fn quotes_group_arguments_and_percent_percent_is_literal() {
        let argv = expand_exec(
            r#"prog --label "two words" 100%% %f"#,
            "Prog",
            None,
            Path::new("/tmp/a.png"),
        );
        assert_eq!(
            argv,
            vec!["prog", "--label", "two words", "100%", "/tmp/a.png"]
        );
    }

    #[test]
    fn the_icon_code_expands_only_when_an_icon_is_known() {
        let with = expand_exec("prog %i %f", "Prog", Some("prog"), Path::new("/tmp/a.png"));
        assert_eq!(with, vec!["prog", "--icon", "prog", "/tmp/a.png"]);

        let without = expand_exec("prog %i %f", "Prog", None, Path::new("/tmp/a.png"));
        assert_eq!(without, vec!["prog", "/tmp/a.png"]);
    }

    #[test]
    fn hidden_and_non_application_entries_are_skipped() {
        let hidden =
            "[Desktop Entry]\nType=Application\nName=Editor\nExec=editor %f\nNoDisplay=true\n";
        assert!(parse_desktop("editor.desktop", hidden).is_none());

        let link = "[Desktop Entry]\nType=Link\nName=Site\nExec=none\n";
        assert!(parse_desktop("site.desktop", link).is_none());

        let terminal = "[Desktop Entry]\nType=Application\nName=Vim\nExec=vim %f\nTerminal=true\n";
        assert!(parse_desktop("vim.desktop", terminal).is_none());
    }

    #[test]
    fn a_desktop_entry_keeps_its_mime_types() {
        let text = "[Desktop Entry]\nType=Application\nName=Krita\nExec=krita %U\nIcon=krita\nMimeType=image/png;image/jpeg;\n\n[Desktop Action New]\nName=New\n";
        let app = parse_desktop("krita.desktop", text).expect("parsed");
        assert_eq!(app.name, "Krita");
        assert_eq!(app.icon.as_deref(), Some("krita"));
        assert_eq!(app.mimes, vec!["image/png", "image/jpeg"]);
    }

    #[test]
    fn an_exact_mime_type_outranks_a_wildcard() {
        let apps = vec![
            app("wild.desktop", "Aardvark", "wild %f", &["image/*"]),
            app("exact.desktop", "Zebra", "exact %f", &["image/png"]),
            app("other.desktop", "Unrelated", "other %f", &["video/mp4"]),
        ];
        let ranked = rank(&apps, "image/png");
        let names: Vec<&str> = ranked.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Zebra", "Aardvark"]);

        // Wildcards still show up for a type nothing claims exactly.
        let ranked = rank(&apps, "image/webp");
        let names: Vec<&str> = ranked.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Aardvark"]);
    }
}
