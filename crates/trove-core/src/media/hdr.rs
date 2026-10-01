//! The display transform for scene-linear stills (OpenEXR, Radiance HDR).
//!
//! A float image is not a picture yet. Its samples are linear scene radiance,
//! often well outside `0.0..=1.0`, while a screen takes 8-bit sRGB — so
//! something has to fold the highlights into the visible range and apply the
//! transfer curve. Handing the buffer to `image`'s plain conversion instead
//! treats the samples as if they were already normalised, which turns a
//! rendered EXR into a near-black square with a few clipped lights in it.
//!
//! This is a curve, not a colour-management system: there is no OCIO config
//! here and no per-asset input space. That is the honest size of what a
//! thumbnail cache can use — a card has to look right without being told which
//! space its file was written in, and a scene-linear default is what a rendered
//! EXR actually is.

use std::path::Path;

use image::{DynamicImage, ImageBuffer, ImageResult, Rgb};

/// The exposure range the preview offers, in stops. Ten either way is what a
/// compositing app gives, because a raw light pass and a baked beauty pass
/// differ by more than a narrower range can span.
pub const MIN_STOPS: f32 = -10.0;
pub const MAX_STOPS: f32 = 10.0;

/// Open an image file *for display*: decoded, and with any scene-linear buffer
/// mapped through [`tonemap`].
///
/// The path-based signature and palette helpers go through this rather than
/// [`image::open`] so a high-dynamic-range file cannot quietly contribute a
/// near-black hash or a one-colour palette.
pub fn open_for_display(path: &Path) -> ImageResult<DynamicImage> {
    open_for_display_at(path, 0.0)
}

/// [`open_for_display`] at an explicit exposure: the same display transform
/// with `stops` of gain over the default. This is what the preview's exposure
/// control calls on every committed slider value — a full float re-decode of
/// the original, which is why the control commits on release and decodes off
/// the UI thread rather than continuously.
pub fn open_for_display_at(path: &Path, stops: f32) -> ImageResult<DynamicImage> {
    image::open(path).map(|image| tonemap(image, stops))
}

/// One part of an EXR file, as the part selector lists it.
///
/// "Part" is the OpenEXR term for a whole image inside one file — a render's
/// beauty pass plus its AOVs, one `beauty` and one `depth` and so on. The
/// `image` crate's decoder picks whichever part it likes and offers no
/// selection, so anything beyond default-view decoding goes through the
/// `exr` crate directly (it is in the graph as the decoder behind the
/// feature anyway).
#[derive(Debug, Clone, PartialEq)]
pub struct ExrPartInfo {
    /// The part's own name, or an empty string for an unnamed single part.
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Channel count of the part — a beauty pass carries 3 (RGB) or 4 (RGBA),
    /// a depth pass 1.
    pub channels: usize,
    /// Deep parts carry no flat pixels this decode path can produce; the
    /// selector only offers the flat ones.
    pub displayable: bool,
}

/// Enumerate an EXR file's parts from its headers — a header read, no pixels.
///
/// Empty for anything that is not an `.exr`: Radiance HDR and TGA have no
/// concept of parts, and callers gate on the extension anyway.
pub fn exr_parts(path: &Path) -> ImageResult<Vec<ExrPartInfo>> {
    if path
        .extension()
        .is_none_or(|ext| !ext.eq_ignore_ascii_case("exr"))
    {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path)?;
    let reader = exr::block::read(file, false).map_err(map_exr_error)?;
    Ok(reader
        .headers()
        .iter()
        .map(|header| ExrPartInfo {
            name: header
                .own_attributes
                .layer_name
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            width: header.layer_size.width() as u32,
            height: header.layer_size.height() as u32,
            channels: header.channels.list.len(),
            displayable: !header.deep,
        })
        .collect())
}

/// [`open_for_display_at`] for one specific part of an EXR file.
///
/// The decode goes through the `exr` crate's all-flat-layers reader, which
/// accepts any channel layout — a depth AOV (one `Z` channel) renders as
/// grayscale, an RGBA part as colour — where `image`'s decoder would refuse
/// the file outright the moment any part fails its RGB expectation. Parts
/// with deep data are the one thing this path cannot produce; such a file
/// errors here and the caller falls back to the default view. Part indices
/// match [`exr_parts`] positions, and `part` 0 through this function is
/// *this* decoder's first flat layer — not necessarily what
/// [`open_for_display_at`] picks, whose part choice is `image`'s own.
pub fn open_for_display_part(path: &Path, stops: f32, part: usize) -> ImageResult<DynamicImage> {
    let image = exr::prelude::read_all_flat_layers_from_file(path).map_err(map_exr_error)?;
    let layer = image.layer_data.get(part).ok_or_else(|| {
        image::ImageError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("EXR part {part} does not exist in {}", path.display()),
        ))
    })?;
    let (width, height) = (layer.size.width() as u32, layer.size.height() as u32);
    let channels = &layer.channel_data.list;

    // Sample lookup by name, case-insensitive — the spec fixes the names but
    // real files have been seen with lowercase. A missing R/G/B composition
    // falls back to the first channel as grayscale, which is what a depth
    // pass is for the eye anyway.
    let sample = |name: &str| {
        channels
            .iter()
            .find(|c| c.name.to_string().eq_ignore_ascii_case(name))
            .map(|c| &c.sample_data)
    };
    let pick = |s: &exr::image::FlatSamples, i: usize| match s {
        exr::image::FlatSamples::F16(v) => v.get(i).map(|x| x.to_f32()).unwrap_or(0.0),
        exr::image::FlatSamples::F32(v) => v.get(i).copied().unwrap_or(0.0),
        exr::image::FlatSamples::U32(v) => v.get(i).map(|x| *x as f32).unwrap_or(0.0),
    };
    let (r, g, b) = match (sample("R"), sample("G"), sample("B")) {
        (Some(r), Some(g), Some(b)) => (r, g, b),
        _ => {
            let first = channels.first().ok_or_else(|| {
                image::ImageError::IoError(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("EXR part {part} of {} carries no channels", path.display()),
                ))
            })?;
            let s = &first.sample_data;
            (s, s, s)
        }
    };
    let alpha = sample("A");

    let mut buffer = ImageBuffer::<image::Rgba<f32>, Vec<f32>>::new(width, height);
    for (i, pixel) in buffer.pixels_mut().enumerate() {
        let a = alpha.map(|s| pick(s, i)).unwrap_or(1.0);
        *pixel = image::Rgba([pick(r, i), pick(g, i), pick(b, i), a]);
    }
    Ok(tonemap(DynamicImage::ImageRgba32F(buffer), stops))
}

/// The `exr` crate's errors arrive as `ImageError`s the same way `image`'s
/// own OpenEXR decoder reports them, so callers need no second error shape.
fn map_exr_error(error: exr::error::Error) -> image::ImageError {
    image::ImageError::Decoding(image::error::DecodingError::new(
        image::error::ImageFormatHint::Exact(image::ImageFormat::OpenExr),
        error,
    ))
}

/// Whether `ext` decodes into a scene-linear float buffer — the only inputs
/// [`tonemap`] actually transforms. Every other image comes back from it
/// untouched, so the exposure control has nothing to offer them.
pub fn is_scene_linear_ext(ext: &str) -> bool {
    matches!(ext, "exr" | "hdr")
}

/// Fold a decoded image into 8-bit display values at `stops` over the default
/// exposure.
///
/// Anything already encoded (8- or 16-bit PNG, JPEG, TIFF, …) comes back
/// untouched: the transform is for linear floats, and applying it twice would
/// wash out every ordinary picture. `stops` is clamped to
/// [`MIN_STOPS`]`..=`[`MAX_STOPS`].
pub fn tonemap(image: DynamicImage, stops: f32) -> DynamicImage {
    match image {
        DynamicImage::ImageRgb32F(ref buffered) => map_float(
            buffered.width(),
            buffered.height(),
            &to_rgba(buffered),
            stops,
            false,
        ),
        DynamicImage::ImageRgba32F(ref buffered) => map_float(
            buffered.width(),
            buffered.height(),
            buffered.as_raw(),
            stops,
            true,
        ),
        other => other,
    }
}

/// Copy an RGB float buffer into RGBA so the two cases share one loop.
fn to_rgba(buffered: &ImageBuffer<Rgb<f32>, Vec<f32>>) -> Vec<f32> {
    let mut rgba = Vec::with_capacity(buffered.pixels().len() * 4);
    for pixel in buffered.pixels() {
        rgba.extend_from_slice(&pixel.0);
        rgba.push(1.0);
    }
    rgba
}

/// The per-sample work: gain, ACES fit, sRGB encode, quantise.
fn map_float(width: u32, height: u32, samples: &[f32], stops: f32, alpha: bool) -> DynamicImage {
    let gain = 2f32.powf(stops.clamp(MIN_STOPS, MAX_STOPS));
    let mut bytes = Vec::with_capacity(samples.len());
    for pixel in samples.as_chunks::<4>().0 {
        let [r, g, b, a] = [pixel[0], pixel[1], pixel[2], pixel[3]];
        bytes.push(encode(aces_fit(r * gain)));
        bytes.push(encode(aces_fit(g * gain)));
        bytes.push(encode(aces_fit(b * gain)));
        // Alpha is a coverage weight, not light: it is already 0..1 and must
        // not be exposed or tone-mapped with the colour.
        bytes.push(encode_linear(a.clamp(0.0, 1.0)));
    }
    if alpha {
        return DynamicImage::ImageRgba8(
            ImageBuffer::from_raw(width, height, bytes).expect("one rgba pixel per sample group"),
        );
    }
    // A buffer that never had alpha keeps none, or every card of a rendered
    // pass would carry an opaque channel it does not need.
    let rgb = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect::<Vec<u8>>();
    DynamicImage::ImageRgb8(
        ImageBuffer::from_raw(width, height, rgb).expect("three bytes per pixel"),
    )
}

/// The ACES filmic fit (Narkowicz' approximation of the ACES output transform)
/// on one linear channel: highlights roll off towards white instead of
/// clipping, which is the part of a display transform a card cannot do without.
fn aces_fit(x: f32) -> f32 {
    const A: f32 = 2.51;
    const B: f32 = 0.03;
    const C: f32 = 2.43;
    const D: f32 = 0.59;
    const E: f32 = 0.14;
    if x <= 0.0 {
        return 0.0;
    }
    ((x * (A * x + B)) / (x * (C * x + D) + E)).clamp(0.0, 1.0)
}

/// Linear → 8-bit through the sRGB transfer curve.
fn encode(linear: f32) -> u8 {
    let transfer = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    encode_transfer(transfer)
}

/// A value already in display space, quantised without a curve.
fn encode_linear(value: f32) -> u8 {
    encode_transfer(value)
}

fn encode_transfer(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float(pixels: Vec<[f32; 3]>) -> DynamicImage {
        let flat = pixels.iter().flat_map(|p| p.iter().copied()).collect();
        DynamicImage::ImageRgb32F(
            ImageBuffer::from_raw(pixels.len() as u32, 1, flat).expect("test pixels"),
        )
    }

    fn rgb(image: &DynamicImage) -> Vec<[u8; 3]> {
        let buffer = image.to_rgb8();
        buffer
            .pixels()
            .map(|p| [p[0], p[1], p[2]])
            .collect::<Vec<_>>()
    }

    /// A rendered pass has most of its energy below 1.0, so a plain cast would
    /// land it in the bottom few codes. The fit has to lift the mid-grey of a
    /// linear pass into the middle of the visible range.
    #[test]
    fn a_linear_mid_grey_becomes_a_visible_grey() {
        let mapped = rgb(&tonemap(float(vec![[0.18, 0.18, 0.18]]), 0.0));
        let [r, g, b] = mapped[0];
        assert_eq!(r, g, "a neutral stays neutral");
        assert_eq!(g, b);
        assert!((70..=160).contains(&r), "0.18 linear mapped to {r}");
    }

    /// Black is black and white is white, and the curve is monotonic between
    /// them: the property that makes a highlight roll-off legible rather than a
    /// grey mush.
    #[test]
    fn the_curve_is_ordered_and_ends_at_the_ends() {
        let mapped = rgb(&tonemap(
            float(
                (0..=10)
                    .map(|i| [i as f32 / 10.0 * 4.0; 3])
                    .collect::<Vec<_>>(),
            ),
            0.0,
        ));
        assert_eq!(mapped[0], [0, 0, 0], "zero radiance is black");
        assert!(
            mapped[mapped.len() - 1][0] > 240,
            "4× white is near white: {:?}",
            mapped.last()
        );
        for pair in mapped.windows(2) {
            assert!(pair[1][0] >= pair[0][0], "not monotonic: {mapped:?}");
        }
    }

    /// One stop doubles the light handed to the curve, so every channel must
    /// get brighter and never dimmer.
    #[test]
    fn exposure_stops_only_add_light() {
        let base = float(vec![[0.05, 0.2, 0.8]]);
        let dark = rgb(&tonemap(base.clone(), -2.0));
        let lit = rgb(&tonemap(base, 2.0));
        for channel in 0..3 {
            assert!(lit[0][channel] >= dark[0][channel], "channel {channel}");
        }
        assert!(lit != dark, "the slider has to do something");
    }

    /// An ordinary 8-bit picture is not linear scene data. Passing it through
    /// unchanged is what keeps this safe to call on every decode.
    #[test]
    fn encoded_images_pass_through() {
        let plain = DynamicImage::new_rgb8(2, 1);
        assert!(matches!(tonemap(plain, 3.0), DynamicImage::ImageRgb8(_)));
    }

    /// Alpha is coverage, not light: it survives the transform as it stands,
    /// and a buffer that had none does not gain one.
    #[test]
    fn alpha_is_carried_not_exposed() {
        let with_alpha = DynamicImage::ImageRgba32F(
            ImageBuffer::from_raw(1, 1, vec![0.5, 0.5, 0.5, 0.25]).unwrap(),
        );
        let mapped = tonemap(with_alpha, 0.0);
        assert_eq!(mapped.to_rgba8().get_pixel(0, 0)[3], 64, "0.25 alpha");
        let without = tonemap(float(vec![[0.5, 0.5, 0.5]]), 0.0);
        assert!(!without.color().has_alpha());
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-hdr-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The load-bearing claim of the whole feature: an OpenEXR file on disk
    /// becomes a card. `image` reads EXR but cannot write one, so the fixture is
    /// generated by the `exr` crate — which is already in the graph as the
    /// decoder behind the feature, so the test writes with the same library the
    /// app reads with.
    #[test]
    fn an_exr_file_becomes_a_card() {
        let dir = scratch("exr");
        let path = dir.join("beauty.exr");
        exr::image::write::write_rgb_file(&path, 2, 1, |x, _| {
            // One pixel of zero radiance, one of a bright light: what a render
            // layer actually looks like.
            let v = if x == 0 { 0.0f32 } else { 4.0 };
            (v, v, v)
        })
        .expect("the fixture writes");

        let dims = crate::media::probe::image_dimensions(&path).expect("header read");
        assert_eq!((dims.width, dims.height), (2, 1), "no pixel decode needed");

        let decoded = crate::media::thumb::decode_image(&path)
            .expect("the exr decodes through the display transform");
        assert!(
            matches!(decoded, DynamicImage::ImageRgb8(_)),
            "a card is 8-bit, not a float buffer: {decoded:?}"
        );
        let pixels = decoded.to_rgb8();
        assert!(
            pixels.get_pixel(0, 0)[0] < 8,
            "zero radiance stays black: {}",
            pixels.get_pixel(0, 0)[0]
        );
        assert!(
            pixels.get_pixel(1, 0)[0] > 200,
            "4× white lands near white, not clipped to a third: {}",
            pixels.get_pixel(1, 0)[0]
        );

        // And the import path can write it: the JPEG encoder takes 8-bit, which
        // is the whole reason the transform is inside the decode.
        let sha = "ab".repeat(32);
        let card = crate::media::thumb::ensure(&dir, &sha, crate::model::AssetKind::Image, &path)
            .expect("the card is written");
        assert!(card.is_file());
        assert!(image::open(&card).is_ok(), "the card is a readable JPEG");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The part selector's claims, end to end: a multi-part file names its
    /// parts, and each named part decodes to its own picture. The fixture is
    /// written through the `exr` crate's layer API — two named RGB parts, one
    /// black and one white, so a wrong selection cannot pass by accident.
    #[test]
    fn an_exr_part_selector_names_and_decodes_parts() {
        let dir = scratch("exr-parts");
        let path = dir.join("multipart.exr");

        let layer = |name: &str, v: f32| {
            exr::image::Layer::new(
                exr::math::Vec2(2usize, 1usize),
                exr::meta::header::LayerAttributes::named(name),
                exr::image::Encoding::default(),
                exr::image::SpecificChannels::rgb(move |_: exr::math::Vec2<usize>| (v, v, v)),
            )
        };
        use exr::prelude::WritableImage;
        exr::image::Image::from_layers(
            exr::meta::header::ImageAttributes::new(exr::meta::attribute::IntegerBounds::new(
                (0i32, 0i32),
                (2usize, 1usize),
            )),
            vec![layer("beauty", 0.0), layer("key", 1.0)],
        )
        .write()
        .to_file(&path)
        .expect("the multi-part fixture writes");

        let parts = exr_parts(&path).expect("headers read");
        assert_eq!(parts.len(), 2, "both parts are listed: {parts:?}");
        assert_eq!(parts[0].name, "beauty");
        assert_eq!(parts[1].name, "key");
        assert_eq!((parts[0].width, parts[0].height), (2, 1));
        assert_eq!(parts[0].channels, 3);
        assert!(
            parts.iter().all(|p| p.displayable),
            "both are flat: {parts:?}"
        );

        let dark = open_for_display_part(&path, 0.0, 0).expect("part 0 decodes");
        let lit = open_for_display_part(&path, 0.0, 1).expect("part 1 decodes");
        assert_eq!(dark.to_rgba8().get_pixel(0, 0)[0], 0, "beauty stays black");
        assert!(
            lit.to_rgba8().get_pixel(0, 0)[0] > 200,
            "the key pass lands near white: {}",
            lit.to_rgba8().get_pixel(0, 0)[0]
        );
        assert!(
            open_for_display_part(&path, 0.0, 2).is_err(),
            "a part index past the file is refused, not wrapped"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The exposure control is offered exactly where the transform answers:
    /// the two float containers. An encoded image would show the slider but
    /// the picture would never move — the lie the list exists to prevent.
    #[test]
    fn only_the_float_containers_offer_exposure() {
        for ext in ["exr", "hdr"] {
            assert!(is_scene_linear_ext(ext), "{ext} maps");
        }
        for ext in ["png", "jpg", "tga", "tif", "webp", "exrs", ""] {
            assert!(!is_scene_linear_ext(ext), "{ext} must not map");
        }
    }

    /// A TGA gets its size from a branch that names the format, because the
    /// header read guesses from content and TGA has no magic bytes to guess. The
    /// pixel decode needs no such branch — it takes the format from the file
    /// name — so both are asserted here, in the directions they differ.
    #[test]
    fn a_tga_reports_its_size_though_it_has_no_signature() {
        let dir = scratch("tga");
        let path = dir.join("ramp.tga");
        // 18-byte header: no id field, no colour map, image type 2
        // (uncompressed truecolour), 2×1 pixels, 32 bits, top-left origin with
        // 8 alpha bits — then two BGRA pixels.
        let mut bytes = vec![0u8; 18];
        bytes[2] = 2;
        bytes[12] = 2;
        bytes[14] = 1;
        bytes[16] = 32;
        bytes[17] = 0x28;
        bytes.extend_from_slice(&[0, 0, 200, 255, 200, 0, 0, 255]);
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(
            crate::media::probe::image_dimensions(&path).map(|d| (d.width, d.height)),
            Some((2, 1)),
            "the named-format arm is what reads a TGA's header"
        );
        let decoded = crate::media::thumb::decode_image(&path).expect("the tga decodes");
        assert_eq!(
            decoded.to_rgba8().get_pixel(0, 0).0,
            [200, 0, 0, 255],
            "blue-first storage is swapped back"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
