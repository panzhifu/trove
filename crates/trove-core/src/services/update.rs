//! Update check: ask GitHub for the newest release tag and compare it with
//! the version the running binary was built from.
//!
//! Read-only by design. Trove ships as a plain archive on three platforms, so
//! the upgrade step belongs to whoever put the binary there — a package
//! manager on Linux, the user unpacking the next archive elsewhere. All this
//! module does is answer "is there something newer?" and hand back a link;
//! it never downloads or replaces anything.
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
        Err(error) => UpdateState::Failed { error },
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
pub fn probe(current: &str) -> Result<Option<(String, String)>, String> {
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
pub fn latest_tag() -> Result<String, String> {
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
        .call()
        .map_err(|e| e.to_string())?;
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "no location header on the releases/latest response".to_string())?;
    // Only the tag is taken from the header — the page the user is sent to is
    // always built from `REPO` above, so a hostile or broken response cannot
    // redirect them anywhere.
    parse_tag(location).ok_or_else(|| format!("unexpected release location: {location}"))
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
