//! Discover applications that can open a file, for the "Open With" menu.
//!
//! The platform's native "Open With" dialog is a fallback; this module
//! offers a curated list of common applications per file type so users can
//! pick directly without dealing with a file picker.

use std::path::{Path, PathBuf};

/// One application entry for the "Open With" menu.
#[derive(Debug, Clone)]
pub struct OpenWithApp {
    /// Display name (e.g. "Photoshop", "GIMP").
    pub name: String,
    /// Absolute path to the executable. `None` means the app is not installed
    /// on this machine — callers skip it.
    pub exec_path: Option<PathBuf>,
}

/// Return a list of known applications for the file `path`, most relevant
/// first. Only returns apps that are actually installed (verified by probing
/// the filesystem).
pub fn discover_apps(path: &Path) -> Vec<OpenWithApp> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        // --- Image formats -----------------------------------------------
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "svg" | "ico"
        | "heic" | "heif" | "avif" | "jxl" => image_apps(&ext),

        // --- PSD / AI / Sketch -------------------------------------------
        "psd" | "psb" | "ai" | "sketch" | "fig" | "xd" => design_apps(&ext),

        // --- Video -------------------------------------------------------
        // Not a literal list: the classifier in `media::probe` decides what *is*
        // a video for the library, and this gate has to agree with it — the two
        // lists had already drifted once (`flv`/`ts` here, absent there).
        other if trove_core::media::probe::is_video_ext(other) => video_apps(),

        // --- Audio -------------------------------------------------------
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" | "aiff" | "opus" => audio_apps(),

        // --- 3D models ---------------------------------------------------
        "obj" | "fbx" | "gltf" | "glb" | "stl" | "ply" | "dae" | "3ds" | "blend" | "usd"
        | "usdz" => model_apps(),

        // --- Documents ---------------------------------------------------
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "md"
        | "epub" => document_apps(&ext),

        // --- Archives ----------------------------------------------------
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => archive_apps(),

        // --- Fonts -------------------------------------------------------
        "ttf" | "otf" | "woff" | "woff2" | "ttc" => font_apps(),

        // --- Generic: offer viewer + editor ------------------------------
        _ => generic_apps(),
    }
}

// ============================================================================
// Per-category app lists
// ============================================================================

fn image_apps(ext: &str) -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    // Platform-specific image editors
    if cfg!(target_os = "windows") {
        apps.push(app("Photoshop", ps_path_windows()));
        apps.push(app(
            "GIMP",
            Some("C:\\Program Files\\GIMP 2\\bin\\gimp-2.10.exe"),
        ));
        apps.push(app(
            "Paint.NET",
            Some("C:\\Program Files\\paint.net\\PaintDotNet.exe"),
        ));
        apps.push(app(
            "IrfanView",
            Some("C:\\Program Files\\IrfanView\\i_view64.exe"),
        ));
        apps.push(app("PhotoPea (Browser)", None::<PathBuf>)); // web-based, special-cased
    } else if cfg!(target_os = "macos") {
        apps.push(app(
            "Photoshop",
            Some("/Applications/Adobe Photoshop 2024/Adobe Photoshop 2024.app"),
        ));
        apps.push(app("GIMP", Some("/Applications/GIMP.app")));
        apps.push(app("Preview", Some("/Applications/Preview.app")));
        apps.push(app(
            "Pixelmator Pro",
            Some("/Applications/Pixelmator Pro.app"),
        ));
        apps.push(app(
            "Affinity Photo",
            Some("/Applications/Affinity Photo 2.app"),
        ));
    } else {
        apps.push(app("GIMP", find_in_path("gimp")));
        apps.push(app("Krita", find_in_path("krita")));
        apps.push(app("ImageMagick (display)", find_in_path("display")));
    }

    // Cross-platform
    if ext == "png"
        || ext == "jpg"
        || ext == "jpeg"
        || ext == "gif"
        || ext == "webp"
        || ext == "bmp"
    {
        apps.push(app("VS Code", find_in_path_vscode()));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn design_apps(ext: &str) -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        apps.push(app("Photoshop", ps_path_windows()));
        apps.push(app("Illustrator", Some("C:\\Program Files\\Adobe\\Adobe Illustrator 2024\\Support Files\\Contents\\Windows\\Illustrator.exe")));
        apps.push(app(
            "GIMP",
            Some("C:\\Program Files\\GIMP 2\\bin\\gimp-2.10.exe"),
        ));
    } else if cfg!(target_os = "macos") {
        apps.push(app(
            "Photoshop",
            Some("/Applications/Adobe Photoshop 2024/Adobe Photoshop 2024.app"),
        ));
        apps.push(app(
            "Illustrator",
            Some("/Applications/Adobe Illustrator 2024/Adobe Illustrator.app"),
        ));
        apps.push(app("GIMP", Some("/Applications/GIMP.app")));
        apps.push(app("Figma", Some("/Applications/Figma.app")));
        if ext == "sketch" {
            apps.push(app("Sketch", Some("/Applications/Sketch.app")));
        }
    } else {
        apps.push(app("GIMP", find_in_path("gimp")));
        apps.push(app("Inkscape", find_in_path("inkscape")));
        apps.push(app("Figma (Browser)", None::<PathBuf>));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn video_apps() -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        apps.push(app(
            "VLC",
            Some("C:\\Program Files\\VideoLAN\\VLC\\vlc.exe"),
        ));
        apps.push(app(
            "DaVinci Resolve",
            Some("C:\\Program Files\\Blackmagic Design\\DaVinci Resolve\\Resolve.exe"),
        ));
        apps.push(app(
            "Premiere Pro",
            Some("C:\\Program Files\\Adobe\\Adobe Premiere Pro 2024\\Adobe Premiere Pro.exe"),
        ));
        apps.push(app("FFmpeg (CLI)", find_in_path("ffmpeg")));
    } else if cfg!(target_os = "macos") {
        apps.push(app("VLC", Some("/Applications/VLC.app")));
        apps.push(app(
            "QuickTime Player",
            Some("/Applications/QuickTime Player.app"),
        ));
        apps.push(app(
            "DaVinci Resolve",
            Some("/Applications/DaVinci Resolve/DaVinci Resolve.app"),
        ));
        apps.push(app(
            "Final Cut Pro",
            Some("/Applications/Final Cut Pro.app"),
        ));
        apps.push(app(
            "Premiere Pro",
            Some("/Applications/Adobe Premiere Pro 2024/Adobe Premiere Pro.app"),
        ));
        apps.push(app("IINA", Some("/Applications/IINA.app")));
    } else {
        apps.push(app("VLC", find_in_path("vlc")));
        apps.push(app("FFmpeg (CLI)", find_in_path("ffmpeg")));
        apps.push(app("MPV", find_in_path("mpv")));
        apps.push(app("Kdenlive", find_in_path("kdenlive")));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn audio_apps() -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        apps.push(app(
            "Audacity",
            Some("C:\\Program Files\\Audacity\\Audacity.exe"),
        ));
        apps.push(app(
            "VLC",
            Some("C:\\Program Files\\VideoLAN\\VLC\\vlc.exe"),
        ));
        apps.push(app(
            "Adobe Audition",
            Some("C:\\Program Files\\Adobe\\Adobe Audition 2024\\Adobe Audition.exe"),
        ));
    } else if cfg!(target_os = "macos") {
        apps.push(app("Audacity", Some("/Applications/Audacity.app")));
        apps.push(app("VLC", Some("/Applications/VLC.app")));
        apps.push(app("GarageBand", Some("/Applications/GarageBand.app")));
        apps.push(app("Logic Pro", Some("/Applications/Logic Pro.app")));
        apps.push(app(
            "Adobe Audition",
            Some("/Applications/Adobe Audition 2024/Adobe Audition.app"),
        ));
    } else {
        apps.push(app("Audacity", find_in_path("audacity")));
        apps.push(app("VLC", find_in_path("vlc")));
        apps.push(app("Audacious", find_in_path("audacious")));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn model_apps() -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        apps.push(app(
            "Blender",
            Some("C:\\Program Files\\Blender Foundation\\Blender 4.x\\blender.exe"),
        ));
        apps.push(app(
            "MeshLab",
            Some("C:\\Program Files\\VCG\\MeshLab\\meshlab.exe"),
        ));
        apps.push(app(
            "3ds Max",
            Some("C:\\Program Files\\Autodesk\\3ds Max 2024\\3dsmax.exe"),
        ));
        apps.push(app(
            "Maya",
            Some("C:\\Program Files\\Autodesk\\Maya2024\\bin\\maya.exe"),
        ));
    } else if cfg!(target_os = "macos") {
        apps.push(app("Blender", Some("/Applications/Blender.app")));
        apps.push(app("MeshLab", Some("/Applications/MeshLab2022.app")));
        apps.push(app("Cheetah3D", Some("/Applications/Cheetah3D.app")));
    } else {
        apps.push(app("Blender", find_in_path("blender")));
        apps.push(app("MeshLab", find_in_path("meshlab")));
        apps.push(app("FreeCAD", find_in_path("freecad")));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn document_apps(ext: &str) -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        if ext == "pdf" {
            apps.push(app(
                "Adobe Acrobat",
                Some("C:\\Program Files\\Adobe\\Acrobat DC\\Acrobat\\Acrobat.exe"),
            ));
            apps.push(app(
                "SumatraPDF",
                Some("C:\\Program Files\\SumatraPDF\\SumatraPDF.exe"),
            ));
        }
        apps.push(app("VS Code", find_in_path_vscode()));
        apps.push(app(
            "Notepad++",
            Some("C:\\Program Files\\Notepad++\\notepad++.exe"),
        ));
    } else if cfg!(target_os = "macos") {
        if ext == "pdf" {
            apps.push(app("Preview", Some("/Applications/Preview.app")));
            apps.push(app(
                "Adobe Acrobat",
                Some("/Applications/Adobe Acrobat DC/Adobe Acrobat.app"),
            ));
        }
        apps.push(app("VS Code", Some("/Applications/Visual Studio Code.app")));
        apps.push(app("TextEdit", Some("/Applications/TextEdit.app")));
        apps.push(app("Typora", Some("/Applications/Typora.app")));
    } else {
        if ext == "pdf" {
            apps.push(app("Evince", find_in_path("evince")));
            apps.push(app("Okular", find_in_path("okular")));
            apps.push(app("Firefox", find_in_path("firefox")));
        }
        apps.push(app("VS Code", find_in_path_vscode()));
        apps.push(app("Gedit", find_in_path("gedit")));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn archive_apps() -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        apps.push(app("7-Zip", Some("C:\\Program Files\\7-Zip\\7zFM.exe")));
        apps.push(app("WinRAR", Some("C:\\Program Files\\WinRAR\\WinRAR.exe")));
    } else if cfg!(target_os = "macos") {
        apps.push(app(
            "The Unarchiver",
            Some("/Applications/The Unarchiver.app"),
        ));
        apps.push(app("Keka", Some("/Applications/Keka.app")));
    } else {
        apps.push(app("File Roller", find_in_path("file-roller")));
        apps.push(app("Ark", find_in_path("ark")));
        apps.push(app("xarchiver", find_in_path("xarchiver")));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn font_apps() -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        apps.push(app(
            "FontForge",
            Some("C:\\Program Files (x86)\\FontForge\\fontforge.exe"),
        ));
        apps.push(app(
            "BirdFont",
            Some("C:\\Program Files\\BirdFont\\birdfont.exe"),
        ));
    } else if cfg!(target_os = "macos") {
        apps.push(app("Font Book", Some("/Applications/Font Book.app")));
        apps.push(app("FontForge", Some("/Applications/FontForge.app")));
        apps.push(app("Glyphs", Some("/Applications/Glyphs 3.app")));
        apps.push(app("BirdFont", Some("/Applications/BirdFont.app")));
    } else {
        apps.push(app("FontForge", find_in_path("fontforge")));
        apps.push(app("BirdFont", find_in_path("birdfont")));
        apps.push(app("GNOME Font Viewer", find_in_path("gnome-font-viewer")));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

fn generic_apps() -> Vec<OpenWithApp> {
    let mut apps = Vec::new();

    if cfg!(target_os = "windows") {
        apps.push(app("VS Code", find_in_path_vscode()));
        apps.push(app(
            "Notepad++",
            Some("C:\\Program Files\\Notepad++\\notepad++.exe"),
        ));
    } else if cfg!(target_os = "macos") {
        apps.push(app("VS Code", Some("/Applications/Visual Studio Code.app")));
        apps.push(app("TextEdit", Some("/Applications/TextEdit.app")));
    } else {
        apps.push(app("VS Code", find_in_path_vscode()));
        apps.push(app("Gedit", find_in_path("gedit")));
    }

    apps.into_iter().filter(|a| a.exec_path.is_some()).collect()
}

// ============================================================================
// Helpers
// ============================================================================

/// Build an [`OpenWithApp`] with an optional exec path. Accepts anything
/// that converts into a `PathBuf` (e.g. `&str`, `String`, `PathBuf`).
fn app(name: &str, exec_path: Option<impl Into<PathBuf>>) -> OpenWithApp {
    // Verify the path actually exists before returning it.
    let verified = exec_path.and_then(|p| {
        let p: PathBuf = p.into();
        p.is_file().then_some(p)
    });
    OpenWithApp {
        name: name.to_string(),
        exec_path: verified,
    }
}

/// Look up a command on `$PATH`.
fn find_in_path(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| {
                let candidate = dir.join(cmd);
                if candidate.is_file() {
                    return Some(candidate);
                }
                // Windows also tries .exe / .cmd / .bat
                if cfg!(target_os = "windows") {
                    for ext in &["exe", "cmd", "bat"] {
                        let candidate = dir.join(format!("{cmd}.{ext}"));
                        if candidate.is_file() {
                            return Some(candidate);
                        }
                    }
                }
                None
            })
            .find_map(|p| p)
    })
}

/// VS Code has a different command name per platform.
fn find_in_path_vscode() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        // Try the standard install locations first
        for path in &[
            "C:\\Program Files\\Microsoft VS Code\\Code.exe",
            "C:\\Program Files (x86)\\Microsoft VS Code\\Code.exe",
        ] {
            let p = PathBuf::from(path);
            if p.is_file() {
                return Some(p);
            }
        }
        find_in_path("code.exe").or_else(|| find_in_path("code"))
    } else if cfg!(target_os = "macos") {
        let p =
            PathBuf::from("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code");
        if p.is_file() {
            return Some(p);
        }
        find_in_path("code")
    } else {
        find_in_path("code").or_else(|| find_in_path("code-oss"))
    }
}

fn ps_path_windows() -> Option<PathBuf> {
    // Search common Photoshop install locations
    for ver in &["2025", "2024", "2023", "2022"] {
        let path = format!(
            "C:\\Program Files\\Adobe\\Adobe Photoshop {}\\Photoshop.exe",
            ver
        );
        let p = PathBuf::from(&path);
        if p.is_file() {
            return Some(p);
        }
    }
    // Also try Program Files (x86)
    for ver in &["2025", "2024", "2023", "2022"] {
        let path = format!(
            "C:\\Program Files (x86)\\Adobe\\Adobe Photoshop {}\\Photoshop.exe",
            ver
        );
        let p = PathBuf::from(&path);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}
