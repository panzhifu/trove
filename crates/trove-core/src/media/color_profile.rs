//! ICC colour-profile handling: name a file's profile, and fold its pixels
//! into sRGB where a thumbnail is produced.
//!
//! Trove's colour management is deliberately one decision deep: **the
//! thumbnail is the display truth**. The main preview, the palette, the
//! visual signature, AI analysis and the clipboard all read the thumbnail,
//! so transforming to sRGB once, at decode, fixes every consumer at once —
//! and nothing ever writes back to the original file. The full-bleed zoom
//! path decodes the original through gpui (unmanaged), which is the one
//! accepted divergence; see docs/COLOR-MANAGEMENT.md.
//!
//! The transform engine is qcms (Mozilla's pure-Rust CMM). Descriptions are
//! parsed here because qcms keeps its profiles opaque: the `desc`
//! (ICC v2 `textDescriptionType`) and `mluc` (v4 `multiLocalizedUnicodeType`)
//! tag shapes are fixed by the spec, so reading the name is a few dozen
//! lines, not a second CMM.

use image::DynamicImage;

use qcms::{DataType, Intent, Profile, Transform};

/// The colour space a file declares, as far as a thumbnail decode cares.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceColorSpace {
    /// The file carries an ICC profile qcms does not identify as sRGB: the
    /// pixels are transformed to sRGB and `description` is the profile's
    /// own name for display.
    Profile(String),
    /// The file carries a profile that *is* sRGB (by qcms's comparison, not
    /// by name): pixels pass through untouched.
    Srgb,
    /// No profile channel. RAW already develops to sRGB display values and
    /// the HEIC/JXL/PSD/SVG decoders expose no profile, so these pixels are
    /// taken as sRGB.
    Unmanaged,
}

impl SourceColorSpace {
    /// The name the facts record and the inspector shows. A profile without
    /// a readable description still counts as *a* profile — "ICC" is the
    /// honest shorthand for "named profile, unnamed space".
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Profile(description) => Some(description.clone()),
            Self::Srgb => Some("sRGB".to_string()),
            Self::Unmanaged => None,
        }
    }
}

/// The profile bytes an image decoder surfaces, if any. Must be called on
/// the decoder *before* it is consumed by `DynamicImage::from_decoder`.
pub fn from_decoder(decoder: &mut impl image::ImageDecoder) -> Option<Vec<u8>> {
    decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|profile| !profile.is_empty())
}

/// Header-only read of the profile name a file carries: no pixels decode.
/// `None` when the container has no profile channel.
pub fn description_of(path: &std::path::Path) -> Option<String> {
    let profile = profile_of_file(path)?;
    space_of(&profile).label()
}

/// Header-only read of the raw profile bytes, for re-embedding them after a
/// re-encode ([`super::edit`], [`super::convert`]): the pixels may be
/// transformed, but the file's own colour claim travels with them.
pub fn profile_of_file(path: &std::path::Path) -> Option<Vec<u8>> {
    let mut decoder = image::ImageReader::open(path).ok()?.into_decoder().ok()?;
    from_decoder(&mut decoder)
}

/// What the thumbnail path does with a decoded image and its profile:
/// sRGB profiles pass through, everything else is transformed in place.
/// A profile qcms cannot parse leaves the pixels untouched — an unreadable
/// claim is not a licence to mangle pixels.
pub fn to_srgb(image: DynamicImage, profile: &[u8]) -> DynamicImage {
    let Some(source) = Profile::new_from_slice(profile, false) else {
        return image;
    };
    if source.is_sRGB() {
        return image;
    }
    let srgb = Profile::new_sRGB();
    // Higher-bit-depth and float buffers land in the catch-all: the
    // thumbnail is 8-bit JPEG in the end, so collapsing early and
    // transforming the collapsed buffer is the honest order.
    match image {
        DynamicImage::ImageRgb8(buf) => {
            DynamicImage::ImageRgb8(transform_rgb8(&source, &srgb, buf))
        }
        DynamicImage::ImageRgba8(buf) => {
            DynamicImage::ImageRgba8(transform_rgba8(&source, &srgb, buf))
        }
        other => DynamicImage::ImageRgb8(transform_rgb8(&source, &srgb, other.to_rgb8())),
    }
}

/// Transform one RGB8 buffer in place. The `into_raw`/`from_raw` round-trip
/// is length-preserving by construction — same pixel count in, same bytes
/// out — so the `expect` cannot fire.
fn transform_rgb8(source: &Profile, srgb: &Profile, buf: image::RgbImage) -> image::RgbImage {
    let (width, height) = (buf.width(), buf.height());
    let mut raw = buf.into_raw();
    transform(source, srgb, &mut raw, DataType::RGB8);
    image::RgbImage::from_raw(width, height, raw).expect("same buffer, same length")
}

fn transform_rgba8(source: &Profile, srgb: &Profile, buf: image::RgbaImage) -> image::RgbaImage {
    let (width, height) = (buf.width(), buf.height());
    let mut raw = buf.into_raw();
    transform(source, srgb, &mut raw, DataType::RGBA8);
    image::RgbaImage::from_raw(width, height, raw).expect("same buffer, same length")
}

fn transform(source: &Profile, srgb: &Profile, pixels: &mut [u8], ty: DataType) {
    if let Some(xfm) = Transform::new_to(source, srgb, ty, ty, Intent::Perceptual) {
        xfm.apply(pixels);
    }
}

/// Name the space a profile claims: qcms's own sRGB comparison first, then
/// the profile's description tag, then the generic ICC label.
pub fn space_of(profile: &[u8]) -> SourceColorSpace {
    match Profile::new_from_slice(profile, false) {
        Some(parsed) if parsed.is_sRGB() => SourceColorSpace::Srgb,
        Some(_) | None => {
            SourceColorSpace::Profile(description(profile).unwrap_or_else(|| "ICC".to_string()))
        }
    }
}

/// The profile's own name from its `desc` (v2) or `mluc` (v4) tag.
pub fn description(profile: &[u8]) -> Option<String> {
    if profile.len() < 132 {
        return None;
    }
    let count = be_u32(profile, 128)? as usize;
    for i in 0..count {
        let entry = 132 + i * 12;
        if entry + 12 > profile.len() {
            return None;
        }
        let signature = &profile[entry..entry + 4];
        let offset = be_u32(profile, entry + 4)? as usize;
        let size = be_u32(profile, entry + 8)? as usize;
        if offset > profile.len() || offset + size > profile.len() {
            continue;
        }
        let tag = &profile[offset..offset + size];
        match signature {
            b"desc" => {
                if let Some(name) = parse_desc_tag(tag) {
                    return Some(name);
                }
            }
            b"mluc" => {
                if let Some(name) = parse_mluc_tag(tag) {
                    return Some(name);
                }
            }
            _ => {}
        }
    }
    None
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().expect("4 bytes")))
}

/// ICC v2 `textDescriptionType`: a reserved word, the ASCII count and string,
/// then Unicode and Macintosh duplicates Trove does not need — the ASCII
/// record is what every v2 profile writes.
fn parse_desc_tag(tag: &[u8]) -> Option<String> {
    if tag.len() < 12 || &tag[0..4] != b"desc" {
        return None;
    }
    let count = be_u32(tag, 8)? as usize;
    let text = tag.get(12..12 + count)?;
    Some(
        String::from_utf8_lossy(text)
            .trim_end_matches('\0')
            .trim()
            .to_string(),
    )
    .filter(|s| !s.is_empty())
}

/// ICC v4 `multiLocalizedUnicodeType`: fixed-size records of (language,
/// country, length, offset-from-tag-start) into a UTF-16BE string pool.
/// `en-US` wins when present; otherwise the first record speaks.
fn parse_mluc_tag(tag: &[u8]) -> Option<String> {
    if tag.len() < 16 || &tag[0..4] != b"mluc" {
        return None;
    }
    let count = be_u32(tag, 8)? as usize;
    let record_size = be_u32(tag, 12)? as usize;
    if count == 0 || record_size < 12 {
        return None;
    }
    let mut best: Option<String> = None;
    for i in 0..count {
        let record = 16 + i * record_size;
        if record + 12 > tag.len() {
            return None;
        }
        let language = &tag[record..record + 2];
        let length = be_u32(tag, record + 4)? as usize;
        let offset = be_u32(tag, record + 8)? as usize;
        let Some(bytes) = tag.get(offset..offset + length) else {
            continue;
        };
        let name = utf16be(bytes).trim().to_string();
        if name.is_empty() {
            continue;
        }
        if language == b"en" && &tag[record + 2..record + 4] == b"US" {
            return Some(name);
        }
        best = best.or(Some(name));
    }
    best
}

fn utf16be(bytes: &[u8]) -> String {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .filter(|unit| !is_surrogate(*unit))
        .map(|unit| char::from_u32(unit as u32).unwrap_or('\u{fffd}'))
        .collect()
}

fn is_surrogate(unit: u16) -> bool {
    (0xd800..=0xdfff).contains(&unit)
}

/// The linear-RGB fixture, for sibling modules' tests (`edit`, `convert`)
/// that need a profile bytes round-trip with known semantics.
#[cfg(test)]
pub(crate) mod test_icc {
    pub fn linear_profile() -> Vec<u8> {
        super::tests::linear_profile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ICC v2 matrix-shaper profile: D50 white point, Rec.709
    /// primaries (sRGB's), **identity TRCs**. qcms reads it as a real
    /// profile — which makes it a *linear-RGB* space, and transforming it
    /// to sRGB must apply the sRGB encoding curve: mid-gray 128 walks out
    /// around 187. That is a real colour change, not an identity round-trip,
    /// so the transform test proves the engine actually ran.
    pub(crate) fn linear_profile() -> Vec<u8> {
        linear_profile_with_desc(b"desc", {
            let description = "Trove Linear RGB";
            let mut tag = b"desc".to_vec();
            tag.extend_from_slice(&[0; 4]);
            tag.extend_from_slice(&(description.len() as u32 + 1).to_be_bytes());
            tag.extend_from_slice(description.as_bytes());
            tag.push(0);
            tag
        })
    }

    fn linear_profile_with_desc(signature: &[u8; 4], desc: Vec<u8>) -> Vec<u8> {
        // s15Fixed16 = value × 65536.
        let fixed = |v: f64| (v * 65536.0).round() as i32;
        // D50 white point, and the sRGB primaries as ICC colorants — one
        // XYZ *column* per primary (rXYZ is the red primary's (X,Y,Z)).
        const D50: [f64; 3] = [0.964_2, 1.0, 0.824_9];
        const R: [f64; 3] = [0.436_074_7, 0.222_504_5, 0.013_932_2];
        const G: [f64; 3] = [0.385_064_9, 0.716_878_6, 0.097_104_5];
        const B: [f64; 3] = [0.143_080_4, 0.060_616_9, 0.714_173_3];

        fn xyz_tag(signature: &[u8; 4], values: &[i32; 3]) -> Vec<u8> {
            let mut tag = signature.to_vec();
            tag.extend_from_slice(&[0; 4]); // reserved
            for v in values {
                tag.extend_from_slice(&v.to_be_bytes());
            }
            tag
        }

        // curveType with entry count 0 = identity.
        let trc = {
            let mut tag = b"curv".to_vec();
            tag.extend_from_slice(&[0; 4]);
            tag.extend_from_slice(&0u32.to_be_bytes());
            tag
        };

        let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (signature, desc),
            (
                b"wtpt",
                xyz_tag(b"XYZ ", &[fixed(D50[0]), fixed(D50[1]), fixed(D50[2])]),
            ),
            (
                b"rXYZ",
                xyz_tag(b"XYZ ", &[fixed(R[0]), fixed(R[1]), fixed(R[2])]),
            ),
            (
                b"gXYZ",
                xyz_tag(b"XYZ ", &[fixed(G[0]), fixed(G[1]), fixed(G[2])]),
            ),
            (
                b"bXYZ",
                xyz_tag(b"XYZ ", &[fixed(B[0]), fixed(B[1]), fixed(B[2])]),
            ),
            (b"rTRC", trc.clone()),
            (b"gTRC", trc.clone()),
            (b"bTRC", trc),
        ];

        // 128-byte header, tag table, then the tag bodies.
        let mut table = Vec::new();
        let mut bodies = Vec::new();
        let mut offset = 128 + 4 + tags.len() * 12;
        for (signature, body) in &tags {
            table.extend_from_slice(&signature[..]);
            table.extend_from_slice(&(offset as u32).to_be_bytes());
            table.extend_from_slice(&(body.len() as u32).to_be_bytes());
            offset += body.len();
            bodies.push(body);
        }

        let mut profile = vec![0u8; 128];
        profile[12..16].copy_from_slice(b"mntr"); // device class
        profile[16..20].copy_from_slice(b"RGB ");
        profile[20..24].copy_from_slice(b"XYZ "); // the PCS
        profile[36..40].copy_from_slice(b"acsp"); // the profile magic, offset fixed by the spec
        profile.extend_from_slice(&(tags.len() as u32).to_be_bytes());
        profile.extend_from_slice(&table);
        for body in bodies {
            profile.extend_from_slice(body);
        }
        // The spec wants the total length in the header; qcms bounds-checks
        // against it.
        let len = profile.len() as u32;
        profile[0..4].copy_from_slice(&len.to_be_bytes());
        profile
    }

    #[test]
    fn description_reads_the_v2_desc_tag() {
        assert_eq!(
            description(&linear_profile()).as_deref(),
            Some("Trove Linear RGB")
        );
        assert_eq!(description(b"not a profile"), None);
    }

    #[test]
    fn a_v4_mluc_description_prefers_en_us() {
        fn utf16(s: &str) -> Vec<u8> {
            s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
        }
        let (japanese, english) = (utf16("カラープロファイル"), utf16("Trove Linear RGB"));
        let mut mluc = b"mluc".to_vec();
        mluc.extend_from_slice(&[0; 4]); // reserved
        mluc.extend_from_slice(&2u32.to_be_bytes()); // record count
        mluc.extend_from_slice(&12u32.to_be_bytes()); // record size
        let mut body = Vec::new();
        // Offsets are from the tag's start: 16-byte header + 2×12-byte
        // records come before the string pool.
        let pool = (16 + 2 * 12) as u32;
        for (lang, country, text) in [
            (&b"ja"[..], &b"JP"[..], &japanese),
            (&b"en"[..], &b"US"[..], &english),
        ] {
            mluc.extend_from_slice(lang);
            mluc.extend_from_slice(country);
            mluc.extend_from_slice(&(text.len() as u32).to_be_bytes());
            mluc.extend_from_slice(&(pool + body.len() as u32).to_be_bytes());
            body.extend_from_slice(text);
        }
        mluc.extend_from_slice(&body);

        // `description` reads a whole profile (tag table at offset 128), so
        // the tag rides the fixture with the v2 desc swapped for the mluc.
        let profile = linear_profile_with_desc(b"mluc", mluc);
        assert_eq!(description(&profile).as_deref(), Some("Trove Linear RGB"));
    }

    #[test]
    fn space_of_names_the_linear_profile() {
        assert_eq!(
            space_of(&linear_profile()),
            SourceColorSpace::Profile("Trove Linear RGB".to_string())
        );
    }

    /// The point of the fixture: identity TRCs mean the transform has real
    /// work — linear mid-gray comes out sRGB-encoded.
    #[test]
    fn to_srgb_applies_the_encoding_curve_to_linear_pixels() {
        let profile = linear_profile();
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(4, 1, |x, _| {
            image::Rgb([128 + x as u8, 255, 0])
        }));
        let out = to_srgb(image, &profile).to_rgb8();
        // sRGB(128/255) = 0.7354 → 187; every transformed channel must sit
        // near its expectation, with slack for the 8-bit table.
        let expected = |linear: u8| (srgb_encode(f64::from(linear) / 255.0) * 255.0) as i32;
        for x in 0..4u8 {
            let got = out.get_pixel(u32::from(x), 0).0;
            assert!(
                (i32::from(got[0]) - expected(128 + x)).abs() <= 3,
                "linear {linear_in} came out {}",
                got[0],
                linear_in = 128 + x
            );
        }
        // Pure sRGB white under a *linear* interpretation is not white:
        // 255 stays 255 (1.0 encodes to 1.0), so the fixture's green
        // channel survives at full value while the others move.
        assert_eq!(out.get_pixel(1, 0).0[1], 255);
        assert_eq!(out.get_pixel(3, 0).0[2], 0);
    }

    fn srgb_encode(linear: f64) -> f64 {
        if linear <= 0.003_130_8 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        }
    }

    #[test]
    fn an_unparseable_profile_leaves_pixels_alone() {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(2, 1, |_, _| {
            image::Rgb([10, 128, 250])
        }));
        let reference = image.clone();
        assert_eq!(to_srgb(image, b"garbage"), reference);
    }

    // -- end-to-end: the thumbnail decode path, bytes to transformed pixels --

    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xffff_ffff;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    /// A zlib stream of stored (uncompressed) deflate blocks — all the PNG
    /// side needs and no compressor dependency.
    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[]]
        } else {
            data.chunks(65_535).collect()
        };
        for (i, chunk) in chunks.iter().enumerate() {
            let final_block = usize::from(i == chunks.len() - 1);
            out.push(final_block as u8);
            out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&adler32(data).to_be_bytes());
        out
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_input = kind.to_vec();
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        out
    }

    /// An 8×1 RGB PNG carrying the linear-RGB profile in an iCCP chunk and
    /// mid-gray pixels: the whole thumbnail decode path in one fixture.
    fn png_with_iccp(profile: &[u8]) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&8u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));

        let mut iccp = b"trove-test".to_vec();
        iccp.push(0); // profile name terminator
        iccp.push(0); // compression method: zlib
        iccp.extend_from_slice(&zlib_stored(profile));
        png.extend_from_slice(&png_chunk(b"iCCP", &iccp));

        // One row, filter byte 0, mid-gray pixels.
        let raw: Vec<u8> = std::iter::once(0u8)
            .chain(std::iter::repeat_n(128u8, 24))
            .collect();
        png.extend_from_slice(&png_chunk(b"IDAT", &zlib_stored(&raw)));
        png.extend_from_slice(&png_chunk(b"IEND", &[]));
        png
    }

    #[test]
    fn the_decode_path_folds_an_iccp_png_into_srgb() {
        let dir = std::env::temp_dir().join(format!("trove-icc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("linear.png");
        std::fs::write(&path, png_with_iccp(&linear_profile())).unwrap();

        let decoded = crate::media::thumb::decode_image_tracked(&path).expect("the PNG decodes");
        assert_eq!(
            decoded.color_space.as_deref(),
            Some("Trove Linear RGB"),
            "the profile's own name rides the decode"
        );
        // Linear mid-gray came out sRGB-encoded — the transform ran on the
        // way into the thumbnail.
        let pixel = decoded.image.to_rgb8().get_pixel(3, 0).0;
        assert!(
            (i32::from(pixel[0]) - 187).abs() <= 4,
            "linear 128 came out {}",
            pixel[0]
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
