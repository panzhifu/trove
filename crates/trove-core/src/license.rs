//! Offline activation keys: Ed25519-signed payloads the app can verify with
//! nothing but a public key baked into this binary.
//!
//! A key is 85 bytes — a 21-byte payload and a 64-byte signature — spelled in
//! Crockford Base32 (`TROVE-XXXX-…`, 136 characters, no `I L O U`, case- and
//! dash-insensitive to read). The payload:
//!
//! | bytes | field                                                        |
//! |-------|--------------------------------------------------------------|
//! | 0     | product (`0x01` = Trove)                                     |
//! | 1     | signing key id — see [`LICENSE_KEYS`]                        |
//! | 2     | edition (`0x01` = standard)                                  |
//! | 3–4   | issued on, days since 1970-01-01, little-endian              |
//! | 5–6   | updates until, same units; `0xFFFF` = perpetual              |
//! | 7–10  | serial number, little-endian — the ledger's primary key      |
//! | 11–18 | buyer fingerprint: BLAKE3(email), first 8 bytes              |
//! | 19–20 | reserved, must be zero (a newer format fails closed here)    |
//!
//! The **signing** half lives in a private tool (`trove-issuer`, its own
//! repository) that mirrors this layout; only the public keys appear here.
//! Nothing in this module can mint a key, and nothing needs to be secret for
//! that to hold.
//!
//! Policy lives above the signature check: [`verify`] takes the running
//! build's release date and refuses a key whose update coverage ended before
//! it — an *expired* key means "renew to use this build", never "the library
//! is locked". A missing build date (a hand build without the stamp) skips
//! the check rather than guessing.

use std::sync::LazyLock;

use chrono::{Duration, NaiveDate};
use data_encoding::Specification;
use ed25519_dalek::{Signature, VerifyingKey};

/// The product byte this binary accepts.
pub const PRODUCT_TROVE: u8 = 0x01;
/// The first (and, so far, only) issuing key's id.
pub const KEY_ID_1: u8 = 0x01;
/// The standard edition byte.
pub const EDITION_STANDARD: u8 = 0x01;
/// The updates-until sentinel meaning "every build".
pub const PERPETUAL: u16 = u16::MAX;

/// Public keys this binary accepts, by id. A key whose key-id byte is absent
/// here fails closed: rotating the signing key means adding a row, and old
/// builds simply keep honouring the keys they knew.
pub const LICENSE_KEYS: &[(u8, [u8; 32])] = &[(KEY_ID_1, PUBLIC_KEY_1)];

/// The issuing key with id [`KEY_ID_1`], printed by `trove-issuer init`.
const PUBLIC_KEY_1: [u8; 32] = [
    0x51, 0x58, 0x03, 0xe2, 0x59, 0x0a, 0x54, 0x9c, 0x15, 0xca, 0x91, 0xc3, 0xa3, 0xc1, 0x3f, 0x28,
    0x28, 0x65, 0xad, 0xf2, 0x1b, 0x47, 0xad, 0xf3, 0x7e, 0xce, 0xed, 0xfe, 0x77, 0xc5, 0x15, 0xe2,
];

/// Payload 21 bytes + signature 64 bytes.
const BLOB_LEN: usize = 85;
const PAYLOAD_LEN: usize = 21;
/// 85 bytes = 680 bits = exactly 136 Crockford characters.
const KEY_CHARS: usize = 136;
const EPOCH: NaiveDate = match NaiveDate::from_ymd_opt(1970, 1, 1) {
    Some(d) => d,
    None => unreachable!(),
};

/// Crockford Base32 — digits plus 22 letters, `I L O U` absent.
static CROCKFORD: LazyLock<data_encoding::Encoding> = LazyLock::new(|| {
    let mut spec = Specification::new();
    spec.symbols = "0123456789ABCDEFGHJKMNPQRSTVWXYZ".to_string();
    spec.encoding()
        .expect("the Crockford specification is valid")
});

/// What a verified key says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct License {
    /// One of the `EDITION_*` bytes.
    pub edition: u8,
    pub issued_on: NaiveDate,
    /// `None` = perpetual coverage.
    pub updates_until: Option<NaiveDate>,
    /// The issuer ledger's primary key — quote it in support mail.
    pub serial: u32,
    /// Eight bytes of BLAKE3 over the buyer's email; identifies a leaked key
    /// to whoever holds the ledger, and tells nobody else anything.
    pub licensee: [u8; 8],
}

impl License {
    /// The fingerprint as the short hex string the settings page shows.
    pub fn licensee_hex(&self) -> String {
        self.licensee.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A stable identifier for the edition byte; the app layer translates it
    /// for display, and unknown bytes stay unknown rather than pretending to
    /// be the standard tier.
    pub fn edition_name(&self) -> String {
        match self.edition {
            EDITION_STANDARD => "standard".to_string(),
            _ => format!("edition {}", self.edition),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LicenseError {
    /// Not 136 Crockford characters (after the looseness below), or wrong
    /// length once decoded.
    #[error("not a valid Trove activation key")]
    Encoding,
    /// The signature does not verify, the key id is unknown, the product byte
    /// is wrong, or the reserved bytes are not zero. One message on purpose:
    /// none of these is the user's fault, and none is worth distinguishing.
    #[error("this activation key was not issued for this product")]
    Signature,
    /// A well-formed, well-signed payload that still cannot be read as a
    /// license — dates out of range, and the like.
    #[error("this activation key is malformed")]
    Payload,
    /// The build is newer than the key's update coverage. Renew to run it;
    /// the build the key covered keeps working.
    #[error("this key covers updates through {until}, and this build ({build}) is newer")]
    UpdatesExpired { until: NaiveDate, build: NaiveDate },
}

/// Parse and verify a pasted key. `build_date` is the running build's release
/// date (`None` when unknown — see the module docs): a key whose coverage
/// ended before it is refused with [`LicenseError::UpdatesExpired`].
///
/// The error that comes back is the *decisive* one: a shape failure is
/// [`LicenseError::Encoding`] before any key is tried, and once a key's
/// signature verifies, its verdict — expired or accepted — outranks the
/// signature failures the other embedded keys reported. A well-formed key
/// that no embedded key signs is [`LicenseError::Signature`], never
/// "not a key".
pub fn verify(key_text: &str, build_date: Option<NaiveDate>) -> Result<License, LicenseError> {
    let blob = decode(key_text)?;
    let mut worst = LicenseError::Signature;
    for (id, public) in LICENSE_KEYS {
        match verify_blob(&blob, *id, public, build_date) {
            Ok(license) => return Ok(license),
            Err(error @ LicenseError::UpdatesExpired { .. }) => return Err(error),
            Err(error) => worst = error,
        }
    }
    Err(worst)
}

/// [`verify`] against one explicit key — the shape every embedded key goes
/// through, and what the tests drive with throwaway keypairs.
pub fn verify_with_key(
    key_text: &str,
    key_id: u8,
    public: &[u8; 32],
    build_date: Option<NaiveDate>,
) -> Result<License, LicenseError> {
    verify_blob(&decode(key_text)?, key_id, public, build_date)
}

fn verify_blob(
    blob: &[u8; BLOB_LEN],
    key_id: u8,
    public: &[u8; 32],
    build_date: Option<NaiveDate>,
) -> Result<License, LicenseError> {
    let (payload, sig) = blob.split_at(PAYLOAD_LEN);
    let verifying = VerifyingKey::from_bytes(public).expect("an embedded key is a valid key");
    verifying
        .verify_strict(payload, &Signature::from_bytes(sig.try_into().unwrap()))
        .map_err(|_| LicenseError::Signature)?;
    if payload[0] != PRODUCT_TROVE || payload[1] != key_id {
        return Err(LicenseError::Signature);
    }
    if payload[19] != 0 || payload[20] != 0 {
        // Reserved bytes a newer writer used: this build must not guess at
        // what they mean, and half-reading a license is worse than refusing.
        return Err(LicenseError::Signature);
    }
    let license = License {
        edition: payload[2],
        issued_on: date_from_days(u16::from_le_bytes([payload[3], payload[4]]))
            .ok_or(LicenseError::Payload)?,
        updates_until: match u16::from_le_bytes([payload[5], payload[6]]) {
            PERPETUAL => None,
            days => Some(date_from_days(days).ok_or(LicenseError::Payload)?),
        },
        serial: u32::from_le_bytes([payload[7], payload[8], payload[9], payload[10]]),
        licensee: payload[11..19].try_into().expect("eight fixed bytes"),
    };
    match (license.updates_until, build_date) {
        (Some(until), Some(build)) if build > until => {
            return Err(LicenseError::UpdatesExpired { until, build });
        }
        _ => {}
    }
    Ok(license)
}

/// The 136-character `TROVE-` form of a signed blob. The issuer formats; this
/// public twin exists for the round-trip tests and nothing else needs it.
pub fn format_blob(blob: &[u8; BLOB_LEN]) -> String {
    let encoded = CROCKFORD.encode(blob);
    assert_eq!(encoded.len(), KEY_CHARS);
    let mut out = String::with_capacity(KEY_CHARS + 34);
    out.push_str("TROVE-");
    for (i, chunk) in encoded.as_bytes().chunks(4).enumerate() {
        if i > 0 {
            out.push('-');
        }
        out.push_str(std::str::from_utf8(chunk).expect("base32 is ascii"));
    }
    out
}

/// Assemble a blob from a payload and its signature — the inverse of what
/// [`decode`] hands [`verify_with_key`].
pub fn blob_from_parts(payload: &[u8; PAYLOAD_LEN], signature: &[u8; 64]) -> [u8; BLOB_LEN] {
    let mut blob = [0u8; BLOB_LEN];
    blob[..PAYLOAD_LEN].copy_from_slice(payload);
    blob[PAYLOAD_LEN..].copy_from_slice(signature);
    blob
}

/// Normalize what the user pasted and decode it. Uppercased, `-` and blanks
/// dropped, an optional `TROVE` prefix dropped (its `O` is not in the
/// alphabet, so it can never be payload data), and the Crockford confusables
/// folded — `O`→`0`, `I`/`L`→`1` — before a strict decode.
fn decode(key_text: &str) -> Result<[u8; BLOB_LEN], LicenseError> {
    let upper = key_text.to_uppercase();
    let stripped: String = upper
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .collect();
    let body = stripped.strip_prefix("TROVE").unwrap_or(&stripped);
    let folded: String = body
        .chars()
        .map(|c| match c {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect();
    if folded.len() != KEY_CHARS {
        return Err(LicenseError::Encoding);
    }
    let blob = CROCKFORD
        .decode(folded.as_bytes())
        .map_err(|_| LicenseError::Encoding)?;
    blob.try_into().map_err(|_| LicenseError::Encoding)
}

fn date_from_days(days: u16) -> Option<NaiveDate> {
    EPOCH.checked_add_signed(Duration::days(i64::from(days)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    /// One test keypair signs every fixture here; the embedded production key
    /// only differs by which bytes sit in `LICENSE_KEYS`.
    fn test_signer() -> (SigningKey, VerifyingKey) {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    fn payload_for(serial: u32, updates_until: u16) -> [u8; PAYLOAD_LEN] {
        let mut p = [0u8; PAYLOAD_LEN];
        p[0] = PRODUCT_TROVE;
        p[1] = KEY_ID_1;
        p[2] = EDITION_STANDARD;
        p[3..5].copy_from_slice(&11u16.to_le_bytes());
        p[5..7].copy_from_slice(&updates_until.to_le_bytes());
        p[7..11].copy_from_slice(&serial.to_le_bytes());
        p[11..19].copy_from_slice(&[0xAA; 8]);
        p
    }

    fn signed_key(payload: &[u8; PAYLOAD_LEN], signing: &SigningKey) -> String {
        let sig = signing.sign(payload).to_bytes();
        format_blob(&blob_from_parts(payload, &sig))
    }

    /// The whole trip: build, sign, format, paste back in, verify.
    #[test]
    fn a_signed_key_survives_format_parse_and_verify() {
        let (signing, verifying) = test_signer();
        let key = signed_key(&payload_for(42, PERPETUAL), &signing);
        assert!(key.starts_with("TROVE-"));
        assert_eq!(key.matches('-').count(), 34);

        let license = verify_with_key(&key, KEY_ID_1, &verifying.to_bytes(), None).unwrap();
        assert_eq!(license.serial, 42);
        assert_eq!(license.edition, EDITION_STANDARD);
        assert_eq!(license.updates_until, None);
        assert_eq!(license.issued_on, date_from_days(11).unwrap());
    }

    /// The paste is forgiving: lower case, no dashes at all, confusables
    /// typed wrong — all one key. The 85-byte shape underneath is not.
    #[test]
    fn the_pasted_key_is_read_through_case_dashes_and_confusables() {
        let (signing, verifying) = test_signer();
        let key = signed_key(&payload_for(7, PERPETUAL), &signing);
        let mangled = key
            .replace("TROVE-", "trove")
            .replace('-', "")
            .replace('0', "O")
            .replace('1', "L");
        assert_ne!(mangled, key);
        let license = verify_with_key(&mangled, KEY_ID_1, &verifying.to_bytes(), None).unwrap();
        assert_eq!(license.serial, 7);
        assert!(
            verify_with_key(&key[..key.len() - 1], KEY_ID_1, &verifying.to_bytes(), None).is_err()
        );
    }

    /// A flipped character anywhere fails closed, and so does a key from a
    /// different issuer, product, or key-id.
    #[test]
    fn tampering_and_foreign_keys_fail() {
        let (signing, verifying) = test_signer();
        let key = signed_key(&payload_for(1, PERPETUAL), &signing);
        let flipped: String = {
            let mut chars: Vec<char> = key.chars().collect();
            let last = chars.len() - 1;
            chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
            chars.into_iter().collect()
        };
        assert_eq!(
            verify_with_key(&flipped, KEY_ID_1, &verifying.to_bytes(), None),
            Err(LicenseError::Signature)
        );

        let mut foreign = payload_for(1, PERPETUAL);
        foreign[0] = 0x02; // another product
        assert_eq!(
            verify_with_key(
                &signed_key(&foreign, &signing),
                KEY_ID_1,
                &verifying.to_bytes(),
                None
            ),
            Err(LicenseError::Signature)
        );

        let mut newer_format = payload_for(1, PERPETUAL);
        newer_format[19] = 1; // reserved bytes in use: a newer writer
        assert_eq!(
            verify_with_key(
                &signed_key(&newer_format, &signing),
                KEY_ID_1,
                &verifying.to_bytes(),
                None
            ),
            Err(LicenseError::Signature)
        );

        let other_key = SigningKey::from_bytes(&[9u8; 32]);
        assert_eq!(
            verify_with_key(
                &signed_key(&payload_for(1, PERPETUAL), &other_key),
                KEY_ID_1,
                &verifying.to_bytes(),
                None
            ),
            Err(LicenseError::Signature)
        );
    }

    /// Coverage is checked against the *build's* date, not the wall clock: a
    /// perpetual key never expires, a covered build stays valid forever, and
    /// an unknown build date skips the check instead of guessing.
    #[test]
    fn update_coverage_is_measured_against_the_build() {
        let (signing, verifying) = test_signer();
        // Coverage ends on day 100.
        let key = signed_key(&payload_for(3, 100), &signing);
        let vk = verifying.to_bytes();

        assert!(verify_with_key(&key, KEY_ID_1, &vk, Some(date_from_days(100).unwrap())).is_ok());
        assert_eq!(
            verify_with_key(&key, KEY_ID_1, &vk, Some(date_from_days(101).unwrap())),
            Err(LicenseError::UpdatesExpired {
                until: date_from_days(100).unwrap(),
                build: date_from_days(101).unwrap(),
            })
        );
        assert!(verify_with_key(&key, KEY_ID_1, &vk, None).is_ok());

        let perpetual = signed_key(&payload_for(4, PERPETUAL), &signing);
        assert!(
            verify_with_key(
                &perpetual,
                KEY_ID_1,
                &vk,
                Some(date_from_days(65_000).unwrap())
            )
            .is_ok()
        );
    }

    /// Every embedded public key must be a key Ed25519 would accept, or the
    /// first real-world verify would be the one to find out.
    #[test]
    fn the_embedded_keys_are_valid_points() {
        for (_, public) in LICENSE_KEYS {
            VerifyingKey::from_bytes(public).expect("embedded key is a valid Ed25519 point");
        }
    }

    /// The end-to-end hook: a key produced by the *real* issuer verifies
    /// against the *real* embedded key. Silent unless the caller supplies one:
    /// `TROVE_E2E_LICENSE="TROVE-…" cargo test -p trove-core license`.
    #[test]
    fn an_issuer_signed_key_verifies_against_the_embedded_key() {
        let Ok(key) = std::env::var("TROVE_E2E_LICENSE") else {
            return;
        };
        let license = verify(&key, None).expect("the issued key verifies end to end");
        assert_eq!(license.edition, EDITION_STANDARD);
        println!(
            "e2e: serial {}, licensee {}",
            license.serial,
            license.licensee_hex()
        );
    }
}
