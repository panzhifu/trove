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

/// Where the settings page's "how to get a license" button sends people. The
/// LICENSE notice points commercial inquiries at this repository; aim it at
/// the store page once one exists.
pub const PURCHASE_URL: &str = "https://github.com/panzhifu/trove";

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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use trove_core::license::{blob_from_parts, format_blob, EDITION_STANDARD, KEY_ID_1, PRODUCT_TROVE};

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
            classify(Err(LicenseError::UpdatesExpired { until, build: until })),
            LicenseStatus::Expired { until }
        );

        for failure in [LicenseError::Encoding, LicenseError::Signature, LicenseError::Payload] {
            assert!(matches!(
                classify(Err(failure)),
                LicenseStatus::NotActivated
            ));
        }
    }
}
