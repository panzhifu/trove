//! The app's view of its offline license: read the stored key, verify it
//! against the built-in public key, and say what that means today.
//!
//! Deliberately small and synchronous: a verify is an Ed25519 check on 85
//! bytes, and the config read is a few kilobytes — the settings page re-reads
//! per render, the way its other rows re-read their config. What lives here
//! is the *policy*: an expired key (a build newer than the key's update
//! coverage) is surfaced as an expired state, never folded into "not
//! activated", because the two ask different things of the user. Nothing in
//! here ever locks the library down; a failed verification reads as absent
//! and says so in the log.

use chrono::NaiveDate;
use trove_core::license::{self, License, LicenseError};

/// Where the license-info dialog's "buy" button sends people. Aim this at the
/// store listing once one exists (the plan is an Afdian/Mianbaoduo listing for
/// domestic buyers); the repository is only a placeholder until then, so a
/// click there does not strand anyone on a page that cannot sell them a key.
pub const PURCHASE_URL: &str = "https://github.com/panzhifu/trove";

/// Support address shown in the license-info dialog. Buying is asynchronous
/// (pay, then a key arrives by mail), so there has to be a human on the other
/// end for the sale that goes sideways — a wrong address, a lost key, a
/// renewal.
pub const CONTACT_EMAIL: &str = "noke601508@outlook.com";

/// How many assets one library holds on the free tier. Deliberately a
/// constant: tuning the funnel is a one-line change away, and the honest
/// framing is per library ("免费版每库 N 条") rather than an enforceable
/// global quota.
pub const FREE_ASSET_CAP: usize = 500;

use crate::app::settings_write;

/// The running build's release date, stamped by `build.rs`. `None` means the
/// stamp is absent or unreadable — the coverage check is then skipped rather
/// than guessed at (a hand build keeps working on a range-bound key).
pub fn build_date() -> Option<NaiveDate> {
    let raw = option_env!("TROVE_BUILD_DATE")?;
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()
}

/// What the stored license means right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseStatus {
    /// Nothing stored, or what was stored no longer verifies. Both read the
    /// same to the user; the log tells them apart.
    NotActivated,
    /// Verified, and this build is inside its update coverage (or the key is
    /// perpetual).
    Active(License),
    /// Verified, but this build shipped after the key's coverage ended: the
    /// covered builds keep working, this one asks for a renewal.
    Expired { until: NaiveDate },
}

/// Read, verify, classify. One call per use site — the settings page's row
/// and the activate/deactivate paths — so there is no cached state to go
/// stale against the file on disk.
pub fn current() -> LicenseStatus {
    let Some(key) = trove_core::config::AppConfig::load().license else {
        return LicenseStatus::NotActivated;
    };
    classify(license::verify(&key, build_date()))
}

/// The policy layer between a verification result and what the user is told.
/// A failed verification reads as "not activated" — the stored state is only
/// ever a key that once passed, so a failure means the file was edited or
/// corrupted, and either way the honest answer is the same.
fn classify(verification: Result<License, LicenseError>) -> LicenseStatus {
    match verification {
        Ok(info) => LicenseStatus::Active(info),
        Err(LicenseError::UpdatesExpired { until, .. }) => LicenseStatus::Expired { until },
        Err(error) => {
            tracing::warn!(
                %error,
                "the stored license no longer verifies; treating it as not activated"
            );
            LicenseStatus::NotActivated
        }
    }
}

/// Verify a pasted key and store it. Fails without writing anything when the
/// key does not verify — the stored state is only ever a key that passed.
pub fn activate(key: &str) -> Result<License, LicenseError> {
    let info = license::verify(key, build_date())?;
    let mut config = trove_core::config::AppConfig::load();
    config.license = Some(key.trim().to_string());
    settings_write::note(config.save(), "app config");
    Ok(info)
}

/// Remove the stored key. An idempotent no-op when nothing was stored.
pub fn deactivate() {
    let mut config = trove_core::config::AppConfig::load();
    if config.license.is_some() {
        config.license = None;
        settings_write::note(config.save(), "app config");
    }
}

/// The import gate: what every import door consults before starting a job.
///
/// An active license removes the cap entirely. An expired or absent one
/// leaves the cap on — and per the standing policy, neither state ever
/// touches what is already in the library: past the cap, new imports are
/// refused while browsing, editing, deleting and exporting stay free.
pub struct LicenseGate {
    licensed: bool,
    cap: usize,
}

impl LicenseGate {
    /// The gate as the running installation sees it right now.
    pub fn for_current() -> Self {
        Self {
            licensed: matches!(current(), LicenseStatus::Active(_)),
            cap: FREE_ASSET_CAP,
        }
    }

    /// Whether an import may start given the library's current asset count.
    /// At exactly the cap the answer is no — the library is full.
    pub fn permits_import(&self, asset_count: u64) -> bool {
        self.licensed || (asset_count as usize) < self.cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use trove_core::license::{
        EDITION_STANDARD, KEY_ID_1, PRODUCT_TROVE, blob_from_parts, format_blob,
    };

    /// `activate` verifies before it writes: a key signed by some other
    /// keypair — the common forgery, and the everyday typo — is refused and
    /// leaves the stored state untouched. The success path needs the real
    /// signing key and lives with the issuer's end-to-end test instead.
    #[test]
    fn activate_refuses_a_key_that_does_not_verify() {
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let mut payload = [0u8; 21];
        payload[0] = PRODUCT_TROVE;
        payload[1] = KEY_ID_1;
        payload[2] = EDITION_STANDARD;
        let sig = signing.sign(&payload).to_bytes();
        let key = format_blob(&blob_from_parts(&payload, &sig));

        // Well-formed, well-signed — by someone else. The embedded key says no.
        assert!(matches!(activate(&key), Err(LicenseError::Signature)));
    }

    /// The classification the settings page speaks: verified is active,
    /// coverage-end is its own state (it asks for a renewal, not for a key),
    /// and every other failure reads as "not activated" without crashing.
    #[test]
    fn classification_keeps_expired_separate_from_not_activated() {
        let license = License {
            edition: EDITION_STANDARD,
            issued_on: chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap(),
            updates_until: None,
            serial: 1,
            licensee: [0u8; 8],
        };
        assert!(matches!(
            classify(Ok(license.clone())),
            LicenseStatus::Active(_)
        ));

        let until = chrono::NaiveDate::from_ymd_opt(2027, 9, 29).unwrap();
        assert_eq!(
            classify(Err(LicenseError::UpdatesExpired {
                until,
                build: until
            })),
            LicenseStatus::Expired { until }
        );

        for failure in [
            LicenseError::Encoding,
            LicenseError::Signature,
            LicenseError::Payload,
        ] {
            assert!(matches!(
                classify(Err(failure)),
                LicenseStatus::NotActivated
            ));
        }
    }

    /// The gate's decision matrix: below the cap an unlicensed install
    /// imports, at exactly the cap it refuses (the library is full), past it
    /// too — and an active license ignores the cap entirely. The count side
    /// (`Library::asset_count`) is tested against a real store in trove-core.
    #[test]
    fn the_free_cap_refuses_imports_only_at_and_past_the_cap() {
        let free = LicenseGate {
            licensed: false,
            cap: FREE_ASSET_CAP,
        };
        assert!(free.permits_import(0));
        assert!(free.permits_import((FREE_ASSET_CAP - 1) as u64));
        assert!(
            !free.permits_import(FREE_ASSET_CAP as u64),
            "the library is full"
        );
        assert!(!free.permits_import(FREE_ASSET_CAP as u64 + 7));

        let licensed = LicenseGate {
            licensed: true,
            cap: FREE_ASSET_CAP,
        };
        assert!(licensed.permits_import(FREE_ASSET_CAP as u64));
        assert!(licensed.permits_import(u64::MAX));
    }
}
