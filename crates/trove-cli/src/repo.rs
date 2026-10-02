//! `trove repo export|import` — one library as a single `.trove` file.
//!
//! Both commands work from the configuration alone, which is why neither
//! opens a library: export goes through the package writer, which snapshots
//! the database itself and is safe while the desktop app holds the library
//! open, and import creates a library that did not exist yet.

use std::path::{Path, PathBuf};

use serde_json::json;

use trove_core::config::AppConfig;
use trove_core::services::repo_package;

use crate::cli::{Cli, RepoCommand};
use crate::ctx::{CliError, Rendered, Style, resolve_entry};

/// `trove repo export [--out FILE]`
pub fn export(args: &Cli, out: Option<&Path>, style: &Style) -> Result<Rendered, CliError> {
    let config = AppConfig::load();
    let entry = resolve_entry(&config, args.library.as_deref())?;

    let dest = match out {
        Some(path) => with_trove_extension(path),
        None => std::env::current_dir()
            .map_err(|error| CliError::runtime(format!("cannot locate the working directory: {error}")))?
            .join(repo_package::package_file_name(&entry.name)),
    };

    style.progress(&format!(
        "exporting library '{}' to {}",
        entry.slug,
        dest.display()
    ));
    let report = repo_package::export_library_package(&entry.dir(), &entry.name, &dest)?;

    let result = json!({
        "library": { "slug": entry.slug, "name": entry.name },
        "path": report.path.display().to_string(),
        "files": report.files,
        "bytes": report.bytes,
        "linked_packed": report.linked_packed,
        "linked_missing": report.linked_missing,
    });
    let human = format!(
        "package written: {} ({} file(s), {} linked packed, {} linked missing)",
        report.path.display(),
        report.files,
        report.linked_packed,
        report.linked_missing,
    );
    Ok(Rendered::new(result, human))
}

/// `trove repo import FILE [--name NAME] [--activate]`
pub fn import(
    archive: &Path,
    name: Option<&str>,
    activate: bool,
    style: &Style,
) -> Result<Rendered, CliError> {
    if !archive.is_file() {
        return Err(CliError::usage(format!("no such file: {}", archive.display())));
    }
    // The package names the library it was taken from; a `--name` on the
    // command line wins. A file that is not a package is the caller's
    // mistake (exit 2); a package that cannot be read is the world's.
    let manifest = repo_package::read_manifest(archive).map_err(|error| match error {
        trove_core::Error::Validation(_) => CliError::usage(error.to_string()),
        _ => CliError::from(error),
    })?;
    let name = name
        .map(str::to_string)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(manifest.library.name);

    let mut config = AppConfig::load();
    let entry = config
        .add_library(&name)
        .map_err(|error| CliError::runtime(format!("cannot register the library: {error}")))?;

    style.progress(&format!("installing into library '{}'", entry.slug));
    match repo_package::install_library_package(archive, &entry.dir()) {
        Ok(report) => {
            if activate
                && let Err(error) = config.set_active_library(&entry.slug)
            {
                // The library exists either way; the activation is a
                // preference, and the error says so on its own.
                style.note(&format!("could not activate the library: {error}"));
            }
            let result = json!({
                "library": { "slug": entry.slug, "name": entry.name },
                "assets": report.assets_total,
                "materialized": report.materialized,
                "kept_linked": report.kept_linked,
                "missing": report.missing,
                "active": activate,
            });
            let human = format!(
                "library '{}' created: {} asset(s), {} linked restored from the package, {} kept linked, {} missing",
                entry.name,
                report.assets_total,
                report.materialized,
                report.kept_linked,
                report.missing,
            );
            Ok(Rendered::new(result, human))
        }
        Err(error) => {
            // Roll the registration back: an entry pointing at a directory
            // of half a package helps nobody.
            let _ = config.forget_library(&entry.slug);
            let _ = std::fs::remove_dir_all(entry.dir());
            Err(CliError::runtime(format!("import failed: {error}")))
        }
    }
}

/// Force the `.trove` extension the format is known by: `backup.trove`
/// stays, `backup.zip` — or a bare `backup` — becomes `backup.trove`.
fn with_trove_extension(path: &Path) -> PathBuf {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if extension.eq_ignore_ascii_case("trove") => path.to_path_buf(),
        _ => path.with_extension("trove"),
    }
}

/// Dispatch helper: the two subcommands this module owns. Both live in the
/// configuration-only branch of [`crate::main::dispatch`].
pub fn dispatch(
    command: &RepoCommand,
    args: &Cli,
    style: &Style,
) -> Result<Rendered, CliError> {
    match command {
        RepoCommand::Export { out } => export(args, out.as_deref(), style),
        RepoCommand::Import {
            archive,
            name,
            activate,
        } => import(archive, name.as_deref(), *activate, style),
    }
}
