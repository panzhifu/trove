//! Update check: ask GitHub for the newest release tag and compare it with
//! the version the running binary was built from — and, since the release
//! grew real installers (see `packaging/`), a download-and-stage half that
//! fetches this platform's artifact.
//!
//! The split between the two halves is deliberate. The *check* is read-only:
//! it answers "is there something newer?" and hands back a link. The
//! *download* exists only where the user asked for it (a click), writes
//! nothing but a file under the state directory's `updates/`, and replaces
//! nothing — the artifact is handed to the OS's own installer (`Setup.exe`,
//! the mounted DMG, the deb handler) and that program owns the upgrade. This
//! module still never touches the running binary or anything a package
//! manager put on disk.
//!
//! The download carries three guarantees. It is *exclusive* — one stream at a
//! time, claimed atomically under the state mutex, so a double click cannot
//! start two. It is *verified* — the bytes must match the release's
//! `BLAKE3SUMS` manifest (published by `release.yml` beside the artifacts;
//! `SHA256SUMS` rides along for external tools) before they count as staged.
//! BLAKE3 is the same digest every content hash in the library uses, and the
//! manifest's trustworthiness comes from being published in the same release
//! over HTTPS — not from the hash function it names. And it is *bounded* — an
//! artifact over [`MAX_ARTIFACT_BYTES`] is refused from its content-length up
//! front and cut off mid-stream if the response lies about its size.
//!
//! The probe is a `HEAD` on `…/releases/latest` with redirects turned off:
//! GitHub answers `302` and puts the tag in the `location` header, so a check
//! costs one request, needs no JSON, and — unlike the REST API — draws
//! nothing from the 60 requests/hour unauthenticated quota.
//!
//! The outcome of the last check is kept here rather than in the config: it
//! is a property of this run, not a preference, and every surface (status
//! bar, About, Settings) has to show the same answer without asking again.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::error::Error;

/// The repository releases are published from.
const REPO: &str = "panzhifu/trove";

/// `…/releases/latest`: GitHub redirects this to the newest release that is
/// not marked pre-release, which is exactly the question worth asking.
const LATEST_URL: &str = "https://github.com/panzhifu/trove/releases/latest";

/// How long the whole probe may take. ureq sets no global timeout by default,
/// so leaving this unset would park the background task on a black-holed
/// connection for as long as the OS keeps the socket around.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long after launch the automatic check waits. The first paint and the
/// startup library scan both want the machine, and a version badge is never
/// urgent enough to compete with them.
pub const STARTUP_DELAY: Duration = Duration::from_secs(10);

/// Upper bound on one update artifact. Real installers are tens of
/// megabytes; the cap is what stops a confused proxy or a wrong response
/// from streaming unboundedly — checked against the response's
/// content-length before the first byte is written, and against the
/// received count while streaming in case the header was absent or lying.
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// The checksum manifest the staged download is verified against: generated
/// by the release workflow with `b3sum`, in the same coreutils line shape as
/// its `SHA256SUMS` sibling. BLAKE3 keeps the crate on the one hash primitive
/// it already uses everywhere (see `media/hash.rs`); 256-bit output means the
/// digest is 64 hex characters, exactly what [`checksum_line`] accepts.
const CHECKSUMS_ASSET: &str = "BLAKE3SUMS";

/// Seconds since the Unix epoch, for the config's last-check timestamp.
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The release page a newer version would be downloaded from.
pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

/// What the most recent check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// No check has completed in this process yet.
    Unknown,
    /// A check is in flight.
    Checking,
    /// The running build is the newest release.
    Current { version: String },
    /// A newer release exists.
    Available { version: String, url: String },
    /// The check could not complete: offline, GitHub unreachable, rate
    /// limited. Kept for Settings to report; never surfaced as a popup.
    Failed { error: String },
}

static STATE: LazyLock<Mutex<UpdateState>> = LazyLock::new(|| Mutex::new(UpdateState::Unknown));

/// The outcome of the most recent check.
///
/// Cloning the string inside is wasted work for a caller that only wants to
/// know whether something is pending — those should call [`available`].
pub fn state() -> UpdateState {
    STATE
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or(UpdateState::Unknown)
}

/// Publish a new check outcome.
pub fn set_state(next: UpdateState) {
    if let Ok(mut slot) = STATE.lock() {
        *slot = next;
    }
}

/// The pending release as `(version, page)`, or `None` when the running build
/// is current or the last check failed.
///
/// Allocates nothing unless there is something to show, so the status bar can
/// ask every frame.
pub fn available() -> Option<(String, String)> {
    let slot = STATE.lock().ok()?;
    let UpdateState::Available { version, url } = &*slot else {
        return None;
    };
    Some((version.clone(), url.clone()))
}

/// Run the probe and publish the outcome, returning it for the caller's own
/// use. Never returns a `Result`: the only caller is a background task whose
/// sole job is to update a badge, and a silent failure is the correct
/// behaviour when the machine is offline.
pub fn check_now(current: &str) -> UpdateState {
    set_state(UpdateState::Checking);
    let next = match probe(current) {
        Ok(Some((version, url))) => UpdateState::Available { version, url },
        Ok(None) => UpdateState::Current {
            version: current.to_string(),
        },
        Err(error) => UpdateState::Failed {
            error: error.to_string(),
        },
    };
    set_state(next.clone());
    next
}

/// The network half: `Some((version, page))` when a newer release exists,
/// `None` when the running build is the newest one.
///
/// `current` is the running build's version, passed in rather than read from
/// this crate's manifest: releases are cut from `trove-app`'s version, and
/// the two must not be allowed to drift apart silently.
pub fn probe(current: &str) -> Result<Option<(String, String)>, Error> {
    let tag = latest_tag()?;
    // `releases/latest` already skips releases GitHub knows are pre-release,
    // but a tag like `v0.5.0-rc.1` published as a *normal* release would come
    // through, and being nagged about someone else's release candidate is
    // worse than being told a little late.
    if is_prerelease(&tag) || !is_newer(current, &tag) {
        return Ok(None);
    }
    Ok(Some((tag, releases_page())))
}

/// The newest release tag with the leading `v` stripped.
pub fn latest_tag() -> Result<String, Error> {
    let config = ureq::config::Config::builder()
        // Hand back the `302` itself: the tag we want is in its `location`
        // header, and following the redirect would only fetch an HTML page.
        // With redirects off ureq returns the response instead of an error.
        .max_redirects(0)
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("trove/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = ureq::Agent::new_with_config(config)
        .head(LATEST_URL)
        .call()?;
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            Error::Network("no location header on the releases/latest response".into())
        })?;
    // Only the tag is taken from the header — the page the user is sent to is
    // always built from `REPO` above, so a hostile or broken response cannot
    // redirect them anywhere.
    parse_tag(location)
        .ok_or_else(|| Error::Network(format!("unexpected release location: {location}")))
}

/// Whether `remote` is a newer release than `current`, comparing dotted
/// decimal versions: `is_newer("0.4.2", "0.4.10")` is `true`, because `2 < 10`
/// numerically even though `"2" > "10"` as text.
///
/// Anything that is not three-odd dot-separated numbers never counts as
/// newer, so a surprising tag cannot nag the user.
pub fn is_newer(current: &str, remote: &str) -> bool {
    match (parse_numeric(current), parse_numeric(remote)) {
        (Some(current), Some(remote)) => remote > current,
        _ => false,
    }
}

/// The numeric release parts of a version (`"v0.4.2"` → `[0, 4, 2]`), or
/// `None` when the string has any other shape. A pre-release or build suffix
/// is dropped rather than compared: `0.5.0-rc.1` and `0.5.0` share a release
/// number, and `is_prerelease` is what tells those apart.
///
/// Any number of parts is accepted, so a shortened tag still compares
/// sensibly: `"0.5"` is `[0, 5]`, which the vector ordering already reads as
/// newer than `[0, 4, 2]` and older than nothing.
fn parse_numeric(version: &str) -> Option<Vec<u32>> {
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or_default();
    if core.is_empty() {
        return None;
    }
    core.split('.')
        .map(|part| part.parse::<u32>().ok())
        .collect()
}

/// Pull the tag out of a `location` header pointing at a release page:
/// `https://github.com/owner/repo/releases/tag/v0.5.0` → `0.5.0`.
fn parse_tag(location: &str) -> Option<String> {
    let tag = location.trim_end_matches('/').rsplit('/').next()?.trim();
    if tag.is_empty() {
        return None;
    }
    Some(tag.trim_start_matches('v').to_string())
}

/// Whether a tag names a release candidate (`v0.5.0-rc.1`, `0.5.0-beta.2`).
fn is_prerelease(tag: &str) -> bool {
    let tag = tag.to_ascii_lowercase();
    tag.contains("-alpha") || tag.contains("-beta") || tag.contains("-rc")
}

// ---------------------------------------------------------------------------
// Download & stage
// ---------------------------------------------------------------------------

/// What the staged download is doing. Kept next to [`UpdateState`] and read
/// per render by the same surfaces, for the same reason: one answer, every
/// window, no re-asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadState {
    /// Nothing downloaded in this process (or nothing is in flight).
    Idle,
    /// Bytes are landing. `total` is `0` until the response's
    /// content-length is known; the UI shows percent only then.
    Downloading { received: u64, total: u64 },
    /// The artifact is complete under the state directory's `updates/`, and
    /// its BLAKE3 matched the release's `BLAKE3SUMS` line for this asset.
    /// Ready for [`open_staged`].
    Staged {
        version: String,
        path: std::path::PathBuf,
        /// BLAKE3 of the staged bytes, lowercase hex — the digest the
        /// release's checksum manifest vouched for (see
        /// [`expected_digest`]) and the same hash every content hash in
        /// the library uses, recorded here and in the log so what landed
        /// can be identified later.
        hash: String,
    },
    /// The download could not complete. Kept for the About row; never a
    /// popup.
    Failed { error: String },
}

static DOWNLOAD_STATE: LazyLock<Mutex<DownloadState>> =
    LazyLock::new(|| Mutex::new(DownloadState::Idle));

/// The outcome of the most recent download attempt.
pub fn download_state() -> DownloadState {
    DOWNLOAD_STATE
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or(DownloadState::Idle)
}

/// Publish a new download outcome.
pub fn set_download_state(next: DownloadState) {
    if let Ok(mut slot) = DOWNLOAD_STATE.lock() {
        *slot = next;
    }
}

/// The user asked to stop the in-flight download. Checked between chunks;
/// the partial `.part` file is removed and the state goes back to
/// [`DownloadState::Idle`]. Reset by the next [`download_and_stage`].
pub fn cancel_download() {
    CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

static CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The release asset this build can install, by the exact name the release
/// workflow publishes (`release.yml`'s glob), or `None` where the release
/// ships nothing this platform can use — the archive targets stay on the
/// "open the releases page" path.
///
/// The mapping lives on the platform at *runtime* (`std::env::consts`), not
/// in `#[cfg]`, so the whole table is testable on every host.
pub fn installer_asset(version: &str) -> Option<String> {
    asset_name_for(version, std::env::consts::OS, std::env::consts::ARCH)
}

/// The pure form of [`installer_asset`]: the names `packaging/` and
/// `release.yml` publish, keyed on OS and arch.
fn asset_name_for(version: &str, os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        // Inno Setup's wizard (PrivilegesRequired=admin, UAC included).
        ("windows", "x86_64") => Some(format!("Trove-{version}-Setup.exe")),
        // hdiutil UDZO images, named for the binaries' architecture.
        ("macos", "aarch64") => Some(format!("Trove-{version}-aarch64.dmg")),
        ("macos", "x86_64") => Some(format!("Trove-{version}-x86_64.dmg")),
        // The deb is the primary Linux artifact (its Depends line carries
        // the glibc floor documented in packaging/README.md); rpm users and
        // tar.gz unpackers keep the releases page.
        ("linux", "x86_64") => Some(format!("trove_{version}_amd64.deb")),
        _ => None,
    }
}

/// The URL the artifact downloads from: the release's `download/v<tag>`
/// prefix plus the exact asset name. Built from [`REPO`], like the release
/// page, so a hostile check response cannot point the download anywhere
/// else.
pub fn download_url(version: &str) -> Option<String> {
    let asset = installer_asset(version)?;
    Some(format!(
        "https://github.com/{REPO}/releases/download/v{version}/{asset}"
    ))
}

/// Where staged artifacts land: the state directory's `updates/`. The state
/// directory (not data, not cache) because a staged installer is neither a
/// document the user owns nor a derived artifact that can be rebuilt on
/// demand — it is a one-shot hand-off, and its loss costs one download.
pub fn updates_dir() -> std::path::PathBuf {
    crate::paths::state_dir().join("updates")
}

/// Where `version`'s artifact sits once staged, and whether it is there.
pub fn staged_path(version: &str) -> Option<std::path::PathBuf> {
    let path = updates_dir().join(installer_asset(version)?);
    path.is_file().then_some(path)
}

/// Claim the single download slot: `true` with the state reading
/// [`DownloadState::Downloading`], or `false` because a download is already
/// in flight — in which case the caller must not touch the `.part` file, the
/// state, or [`CANCELLED`].
///
/// Checking and claiming have to be one step under the same mutex: two quick
/// clicks spawn two background tasks, and from two threads a check-then-set
/// can both observe `Idle`.
fn try_begin_download() -> bool {
    let mut slot = crate::sync::lock(&DOWNLOAD_STATE);
    if matches!(*slot, DownloadState::Downloading { .. }) {
        return false;
    }
    *slot = DownloadState::Downloading {
        received: 0,
        total: 0,
    };
    true
}

/// Download this platform's artifact for `version` into
/// [`updates_dir`], publish progress into [`DownloadState`], and stage it
/// verified: the bytes are hashed as they stream and must match the
/// release's [`CHECKSUMS_ASSET`] line for this asset before the `.part`
/// file is renamed into place. One download runs at a time — a call that
/// arrives while another is in flight is refused without disturbing it.
///
/// Streaming, not `read_to_end`: an installer is tens of megabytes, the
/// state is updated as it goes, and a cancel is honoured between chunks
/// rather than after the fact. The bytes land on a `.part` sibling first and
/// a rename moves them into place, so a crash or a cancel costs at most the
/// partial file, never a half-written installer that looks finished.
///
/// Runs on the caller's executor — `ureq` is `Send`, the state is a mutex,
/// nothing here touches a window.
pub fn download_and_stage(version: &str) -> Result<std::path::PathBuf, Error> {
    let Some(url) = download_url(version) else {
        return Err(Error::Network(
            "this build has no downloadable installer; use the releases page".into(),
        ));
    };
    let asset = installer_asset(version).expect("the URL exists only when the asset does");
    let dir = crate::paths::ensure(&updates_dir())?;
    let dest = dir.join(&asset);
    let part = dir.join(format!("{asset}.part"));

    // The claim comes before every fallible step that would otherwise leave
    // the slot held without a stream behind it, and before the cancellation
    // reset: a refused second caller must not blind the first task to the
    // cancel already pending for it.
    if !try_begin_download() {
        return Err(Error::Network("a download is already in progress".into()));
    }
    CANCELLED.store(false, std::sync::atomic::Ordering::Relaxed);

    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The manifest comes first: a release that will not vouch for its
        // own artifact never costs the (large) download.
        let expected = expected_digest(version, &asset)?;
        // No global timeout — a tens-of-megabytes stream would die of it.
        // Each phase gets its own: connect once, response headers once,
        // and a per-read ceiling that only trips when the stream actually
        // stops moving.
        let config = ureq::config::Config::builder()
            .timeout_connect(Some(TIMEOUT))
            .timeout_recv_response(Some(TIMEOUT))
            .timeout_recv_body(Some(Duration::from_secs(30)))
            .user_agent(concat!("trove/", env!("CARGO_PKG_VERSION")))
            .build();
        let response = ureq::Agent::new_with_config(config)
            .get(&url)
            .call()
            .map_err(|error| Error::Network(format!("download failed: {error}")))?;
        let total = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        // The honest response states its size; one past the cap is refused
        // before a byte is written.
        if total > MAX_ARTIFACT_BYTES {
            return Err(Error::Network(format!(
                "artifact is {total} bytes, over the {} MiB download cap",
                MAX_ARTIFACT_BYTES / (1024 * 1024)
            )));
        }
        use std::io::{Read as _, Write as _};
        let mut reader = response.into_body().into_reader();
        let mut hasher = blake3::Hasher::new();
        let mut file = std::fs::File::create(&part)?;
        let mut buf = vec![0u8; 64 * 1024];
        let (mut received, mut reported): (u64, u64) = (0, 0);
        loop {
            if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_file(&part);
                return Err(Error::Network("download cancelled".into()));
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            received += n as u64;
            // The streaming half of the cap: a response that never sent a
            // content-length (or sent a false one) is cut off here.
            if received > MAX_ARTIFACT_BYTES {
                let _ = std::fs::remove_file(&part);
                return Err(Error::Network(format!(
                    "artifact is over the {} MiB download cap",
                    MAX_ARTIFACT_BYTES / (1024 * 1024)
                )));
            }
            // Lock churn, bounded: the About row polls per frame, but a
            // mutex tick per 64 KiB chunk buys nothing over one per 256 KiB.
            if received - reported >= 256 * 1024 {
                reported = received;
                set_download_state(DownloadState::Downloading { received, total });
            }
        }
        file.flush()?;
        if total != 0 && received != total {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Network(format!(
                "download truncated: {received} of {total} bytes"
            )));
        }
        let hash = crate::media::hash::hex(hasher.finalize().as_bytes());
        // The manifest has the final word: bytes the release does not vouch
        // for never become a staged installer.
        if !hash.eq_ignore_ascii_case(&expected) {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Network(format!(
                "checksum mismatch for {asset}: expected {expected}, got {hash}"
            )));
        }
        std::fs::rename(&part, &dest)?;
        Ok((dest, hash))
    }))
    // A panic mid-stream must not leave the slot claimed forever: the state
    // would read `Downloading` with nothing in flight, and every later
    // download this session would be refused by the single-flight check.
    .unwrap_or_else(|_| {
        let _ = std::fs::remove_file(&part);
        Err(Error::Network("download panicked".into()))
    });

    match run {
        Ok((path, hash)) => {
            tracing::info!(%version, %hash, bytes = path.metadata().map(|m| m.len()).unwrap_or(0), "update staged");
            set_download_state(DownloadState::Staged {
                version: version.to_string(),
                path: path.clone(),
                hash,
            });
            Ok(path)
        }
        Err(error) => {
            set_download_state(DownloadState::Failed {
                error: error.to_string(),
            });
            Err(error)
        }
    }
}

/// The BLAKE3 the release's [`CHECKSUMS_ASSET`] manifest records for
/// `asset`, fetched from the same release the artifact comes from.
///
/// A manifest that will not load — an older release that predates it, a
/// yanked asset, a network that dropped between the two requests — fails the
/// download rather than waving the bytes through unverified: fail closed is
/// the only honest default for a check whose absence reads as success.
fn expected_digest(version: &str, asset: &str) -> Result<String, Error> {
    let url = format!("https://github.com/{REPO}/releases/download/v{version}/{CHECKSUMS_ASSET}");
    let config = ureq::config::Config::builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(concat!("trove/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = ureq::Agent::new_with_config(config)
        .get(&url)
        .call()
        .map_err(|error| Error::Network(format!("fetch {CHECKSUMS_ASSET}: {error}")))?;
    use std::io::Read as _;
    // One line per asset; a megabyte is already absurd, so the read is
    // capped rather than trusted to the response to end where it should.
    let mut manifest = String::new();
    response
        .into_body()
        .into_reader()
        .take(1024 * 1024)
        .read_to_string(&mut manifest)
        .map_err(|error| Error::Network(format!("read {CHECKSUMS_ASSET}: {error}")))?;
    checksum_line(&manifest, asset)
        .ok_or_else(|| Error::Network(format!("{CHECKSUMS_ASSET} names no {asset}")))
}

/// The checksum line for `asset` in a coreutils-format manifest (`b3sum`
/// writes the same shape `sha256sum` does): lines of
/// `<64 hex digits>  <name>`, whitespace-separated, where `name` may carry
/// the directory the manifest was generated in (the release job's artifact
/// subdirectories). Matching is on the file's basename, and the digest comes
/// back lowercase hex. `None` for every malformed line — a truncated
/// manifest cannot stand in for a missing one.
fn checksum_line(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let line = line.trim();
        let (digest, name) = line.split_once(char::is_whitespace)?;
        if name.trim().rsplit('/').next()? != asset {
            return None;
        }
        let digest = digest.trim();
        let shaped = digest.len() == 64 && digest.chars().all(|ch| ch.is_ascii_hexdigit());
        shaped.then(|| digest.to_ascii_lowercase())
    })
}

/// Hand the staged artifact to the OS's own handler: `Setup.exe` launches
/// its wizard, the DMG mounts into Finder, the deb opens the package
/// installer. Whatever happens next belongs to that program and the user —
/// this module's part ends here.
///
/// The caller decides whether to quit the app afterwards (Windows: yes —
/// Inno refuses to overwrite a running binary; macOS and Linux: no, nothing
/// about the open step conflicts with the running app).
pub fn open_staged(version: &str) -> Result<(), Error> {
    let Some(path) = staged_path(version) else {
        return Err(Error::NotFound("staged update"));
    };
    crate::services::open_external::open(&path, crate::services::open_external::OpenTarget::Default)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_numeric_release_wins() {
        assert!(is_newer("0.4.2", "0.4.10"));
        assert!(is_newer("0.4.2", "0.5.0"));
        assert!(is_newer("0.4.2", "1.0.0"));
        assert!(!is_newer("0.4.2", "0.4.2"));
        assert!(!is_newer("0.5.0", "0.4.9"));
    }

    #[test]
    fn a_leading_v_is_not_part_of_the_number() {
        assert!(is_newer("0.4.2", "v0.5.0"));
        assert!(is_newer("v0.4.2", "v0.5.0"));
    }

    #[test]
    fn a_shortened_version_compares_by_its_parts() {
        // `0.5` is `0.5.0` with the tail dropped, and the numeric parts say
        // so on their own: `[0, 5] > [0, 4, 2]`.
        assert!(is_newer("0.4.2", "0.5"));
        assert!(is_newer("0.4", "0.4.2"));
        assert!(!is_newer("0.4.2", "0.4"));
    }

    #[test]
    fn other_shapes_never_count_as_newer() {
        assert!(!is_newer("0.4.2", ""));
        assert!(!is_newer("0.4.2", "nightly"));
        assert!(!is_newer("0.4.2", "0.5.x"));
        assert!(!is_newer("garbage", "9.9.9"));
    }

    #[test]
    fn prerelease_tags_are_recognised() {
        assert!(is_prerelease("0.5.0-rc.1"));
        assert!(is_prerelease("v0.5.0-beta"));
        assert!(is_prerelease("0.5.0-ALPHA"));
        assert!(!is_prerelease("0.5.0"));
        assert!(!is_prerelease("0.5.0+build.7"));
    }

    #[test]
    fn tags_parse_out_of_a_release_location() {
        assert_eq!(
            parse_tag("https://github.com/panzhifu/trove/releases/tag/v0.5.0").as_deref(),
            Some("0.5.0")
        );
        assert_eq!(
            parse_tag("https://github.com/panzhifu/trove/releases/tag/0.4.2/").as_deref(),
            Some("0.4.2")
        );
        assert_eq!(
            parse_tag("https://github.com/panzhifu/trove").as_deref(),
            Some("trove")
        );
        assert_eq!(parse_tag(""), None);
    }

    #[test]
    fn a_release_candidate_is_skipped_by_the_probe() {
        // Only the decision is tested here: `probe` itself would need the
        // network. `is_prerelease` + `is_newer` are the whole rule.
        let tag = "0.5.0-rc.1";
        assert!(is_prerelease(tag));
        assert!(is_newer("0.4.2", tag));
    }

    /// The artifact table, against the names `release.yml` uploads. Every
    /// combination is listed here, including the `None`s: a platform the
    /// release ships nothing for must fall back to the releases page, not
    /// to a guessed filename.
    #[test]
    fn the_installer_table_matches_the_release_glob() {
        assert_eq!(
            asset_name_for("0.5.2", "windows", "x86_64").as_deref(),
            Some("Trove-0.5.2-Setup.exe")
        );
        assert_eq!(asset_name_for("0.5.2", "windows", "aarch64"), None);
        assert_eq!(
            asset_name_for("0.5.2", "macos", "aarch64").as_deref(),
            Some("Trove-0.5.2-aarch64.dmg")
        );
        assert_eq!(
            asset_name_for("0.5.2", "macos", "x86_64").as_deref(),
            Some("Trove-0.5.2-x86_64.dmg")
        );
        assert_eq!(
            asset_name_for("0.5.2", "linux", "x86_64").as_deref(),
            Some("trove_0.5.2_amd64.deb")
        );
        assert_eq!(asset_name_for("0.5.2", "linux", "aarch64"), None);
        assert_eq!(asset_name_for("0.5.2", "freebsd", "x86_64"), None);
    }

    /// The download URL is the release's `download/v<tag>` prefix plus the
    /// asset name — and there is no URL where there is no asset.
    #[test]
    fn the_download_url_points_at_the_release_asset() {
        assert_eq!(
            download_url_for_test("0.5.2", "windows", "x86_64").as_deref(),
            Some(
                "https://github.com/panzhifu/trove/releases/download/v0.5.2/Trove-0.5.2-Setup.exe"
            )
        );
        assert_eq!(download_url_for_test("0.5.2", "linux", "aarch64"), None);
    }

    /// The pure table behind `download_url`, callable without depending on
    /// this host's platform.
    fn download_url_for_test(version: &str, os: &str, arch: &str) -> Option<String> {
        let asset = asset_name_for(version, os, arch)?;
        Some(format!(
            "https://github.com/{REPO}/releases/download/v{version}/{asset}"
        ))
    }

    /// The manifest parser, against the shapes `b3sum` and the release
    /// job actually produce: two-space separation, a name that may carry the
    /// artifact subdirectory it was generated under, uppercase hex coming
    /// back lowercase — and the refusals: a foreign asset, a digest that is
    /// not 64 hex digits. A truncated manifest line cannot stand in for a
    /// real one.
    #[test]
    fn the_checksum_manifest_is_parsed_by_asset_name() {
        let setup = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let deb = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let manifest = format!(
            "{setup}  Trove-0.5.2-Setup.exe\n{deb}  trove-x86_64-unknown-linux-gnu/trove_0.5.2_amd64.deb\n"
        );
        assert_eq!(
            checksum_line(&manifest, "Trove-0.5.2-Setup.exe").as_deref(),
            Some(setup)
        );
        assert_eq!(
            checksum_line(&manifest, "trove_0.5.2_amd64.deb").as_deref(),
            Some(deb),
            "the subdirectory the manifest was generated under is not part of the key"
        );
        assert_eq!(checksum_line(&manifest, "trove-0.5.2-aarch64.dmg"), None);

        // One space also parses, and hex is normalized to lowercase.
        let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        assert_eq!(
            checksum_line(
                &format!("{upper} trove-0.5.2-x86_64.tar.gz\n"),
                "trove-0.5.2-x86_64.tar.gz"
            )
            .as_deref(),
            Some(upper.to_ascii_lowercase().as_str())
        );

        // Not 64 hex digits: skipped, never matched.
        assert_eq!(
            checksum_line("deadbeef  Trove-0.5.2-Setup.exe", "Trove-0.5.2-Setup.exe"),
            None
        );
    }

    /// The download slot is exclusive and the refusal atomic: a second
    /// claimant is refused while the first holds it. The state is restored
    /// so the test leaves no phantom download behind.
    #[test]
    fn a_held_download_slot_refuses_a_second_claim() {
        let previous = download_state();
        assert!(
            !matches!(previous, DownloadState::Downloading { .. }),
            "another test left a download in flight"
        );
        assert!(try_begin_download());
        assert!(matches!(
            download_state(),
            DownloadState::Downloading { .. }
        ));
        assert!(!try_begin_download(), "the second claim must be refused");
        set_download_state(previous);
    }

    /// The one test that talks to GitHub: it pins the assumption the whole
    /// probe rests on — that `releases/latest` answers `302` with the tag in
    /// its `location` header, and that redirects-off really does hand back
    /// the redirect instead of following it. Ignored by default; run with
    /// `cargo test -p trove-core -- --ignored latest_release`.
    #[test]
    #[ignore = "requires network access"]
    fn the_latest_release_probe_reads_a_tag_out_of_github() {
        let tag = latest_tag().expect("github should answer the probe");
        assert!(
            parse_numeric(&tag).is_some(),
            "unexpected tag shape: {tag:?}"
        );
    }
}
