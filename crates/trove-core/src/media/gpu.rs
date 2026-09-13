//! CPU-side data preparation for the GPU (wgpu) renderer.
//!
//! Nothing here touches a graphics device: this is the layout work that has to
//! agree byte for byte with the shader and with wgpu's copy rules, kept apart
//! from the app's pipeline so it can be tested on a machine with no GPU.
//!
//! Three things live here:
//!
//! * [`Uniforms`] — the block the WGSL shader reads, packing the same framing
//!   and shading values the CPU rasterizer uses;
//! * [`padded_row_bytes`] — the 256-byte row alignment a texture→buffer copy
//!   requires;
//! * [`unpack_bgra`] — stripping that padding back off and swizzling to the
//!   BGRA order the UI expects.

use super::render3d::{
    AMBIENT, BG_BOTTOM, BG_TOP, DIFFUSE, EDL_STRENGTH, Framing, KEY_LIGHT, MATERIAL, POINT_RADIUS,
    SHININESS, SPECULAR, VIGNETTE,
};

/// Bytes of [`Uniforms`]: one `mat4x4<f32>` plus eight `vec4<f32>`.
pub const UNIFORM_SIZE: usize = 64 + 8 * 16;

/// The uniform block read by the model and backdrop shaders.
///
/// Field order is the WGSL declaration order — the byte packing in
/// [`Uniforms::to_bytes`] walks the fields in exactly this sequence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Uniforms {
    /// Model space → clip space, column-major.
    pub view_proj: [[f32; 4]; 4],
    /// Key light direction (normalised), `w` unused.
    pub light: [f32; 4],
    /// Base surface colour, `w` = ambient.
    pub material: [f32; 4],
    /// `x` = diffuse amount, `y` = specular amount, `z` = shininess.
    pub params: [f32; 4],
    /// `x` = point sprite radius in pixels, `y` = eye-dome lighting strength.
    /// `z` and `w` are the depth-to-log-depth constants (`z_scale`, `z_bias`)
    /// the point-cloud post pass uses to rebuild view depth from the depth
    /// buffer, so it can light the creases the CPU rasteriser lights.
    pub params2: [f32; 4],
    /// Camera position in model space, so shading can work where the normals
    /// live; `w` unused.
    pub eye: [f32; 4],
    /// Viewport size in pixels, for the backdrop's gradient and vignette.
    pub viewport: [f32; 4],
    /// Backdrop gradient, top then bottom.
    pub background: [[f32; 4]; 2],
}

impl Uniforms {
    /// Build the block for one frame.
    pub fn new(framing: &Framing, viewport: (u32, u32)) -> Self {
        let light = normalize3(KEY_LIGHT);
        let eye = framing.eye_in_model_space();
        // Same constants `render3d`'s projection is built from: depth is affine
        // in `1/w`, so these turn a depth-buffer value back into a view
        // distance for the eye-dome lighting pass.
        let (near, far) = framing.depth_range();
        let z_scale = far / (far - near);
        let z_bias = -far * near / (far - near);
        Self {
            view_proj: framing.view_projection(),
            light: [light[0], light[1], light[2], 0.0],
            material: [MATERIAL[0], MATERIAL[1], MATERIAL[2], AMBIENT],
            params: [DIFFUSE, SPECULAR, SHININESS, VIGNETTE],
            params2: [POINT_RADIUS, EDL_STRENGTH, z_scale, z_bias],
            eye: [eye[0], eye[1], eye[2], 0.0],
            viewport: [viewport.0.max(1) as f32, viewport.1.max(1) as f32, 0.0, 0.0],
            background: [
                [BG_TOP[0], BG_TOP[1], BG_TOP[2], 0.0],
                [BG_BOTTOM[0], BG_BOTTOM[1], BG_BOTTOM[2], 0.0],
            ],
        }
    }

    /// Flatten to the byte layout wgpu uploads. Every field is 16-byte aligned,
    /// so the floats simply follow one another.
    pub fn to_bytes(&self) -> [u8; UNIFORM_SIZE] {
        let mut floats = [0f32; UNIFORM_SIZE / 4];
        let mut at = 0;
        let mut push = |values: &[f32], at: &mut usize| {
            floats[*at..*at + values.len()].copy_from_slice(values);
            *at += values.len();
        };
        for column in &self.view_proj {
            push(column, &mut at);
        }
        for vector in [
            self.light,
            self.material,
            self.params,
            self.params2,
            self.eye,
            self.viewport,
            self.background[0],
            self.background[1],
        ] {
            push(&vector, &mut at);
        }
        debug_assert_eq!(at, floats.len());

        let mut bytes = [0u8; UNIFORM_SIZE];
        for (index, value) in floats.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }
}

/// Row stride a texture→buffer copy needs: wgpu requires a multiple of 256.
pub fn padded_row_bytes(width: u32) -> u32 {
    let tight = width.saturating_mul(4);
    tight.div_ceil(256) * 256
}

/// Size of the staging buffer for a `width`×`height` RGBA frame.
pub fn staging_len(width: u32, height: u32) -> u64 {
    u64::from(padded_row_bytes(width)) * u64::from(height)
}

/// Pixel order a read-back frame arrives in.
///
/// The UI hands gpui BGRA bytes, so an `Rgba8Unorm` target has to be
/// swizzled on the way out. Rendering into `Bgra8Unorm` instead — which most
/// adapters support — removes that per-pixel work from every interactive
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelOrder {
    /// RGBA in the buffer, BGRA wanted: swizzle.
    Rgba,
    /// Already BGRA: only the row padding has to come off.
    Bgra,
}

/// Strip the row padding from a read-back frame and swizzle to the BGRA
/// order the UI expects.
///
/// The UI hands gpui the same BGRA buffer for both renderers, so this is what
/// makes a GPU frame and a CPU frame interchangeable. The alpha channel is
/// forced opaque: the viewport paints over the background itself.
///
/// A width whose rows are already 256-byte aligned (`width * 4 % 256 == 0`,
/// i.e. a multiple of 64 pixels) has no padding to strip, so the pixels are
/// one contiguous run and `order == Bgra` copies them in one go.
pub fn unpack_bgra(padded: &[u8], width: u32, height: u32, order: PixelOrder) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let row = padded_row_bytes(width) as usize;
    let tight = w * 4;

    // Nothing to unpad and nothing to swizzle: one copy, then the alpha pass.
    if order == PixelOrder::Bgra && row == tight && padded.len() >= tight * h {
        let mut out = padded[..tight * h].to_vec();
        for pixel in out.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        return out;
    }

    let mut out = Vec::with_capacity(tight * h);
    for y in 0..h {
        let start = y * row;
        for x in 0..w {
            let i = start + x * 4;
            match padded.get(i..i + 4) {
                Some(pixel) => match order {
                    PixelOrder::Rgba => out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]),
                    PixelOrder::Bgra => out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
                },
                // A short buffer means the copy failed; fill instead of
                // returning a ragged frame.
                None => out.extend_from_slice(&[0, 0, 0, 255]),
            }
        }
    }
    out
}

/// `[f32; 3]` normalise, mirroring the renderer's own helper.
fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if length > 1e-12 {
        [v[0] / length, v[1] / length, v[2] / length]
    } else {
        [0.0; 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::render3d::Camera;

    fn framing() -> Framing {
        Camera::default().framing(
            crate::media::formats::types::Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            16.0 / 9.0,
        )
    }

    #[test]
    fn the_uniform_block_is_the_size_the_shader_expects() {
        // 64 bytes of matrix + eight vec4 = 192, a multiple of 16.
        assert_eq!(UNIFORM_SIZE, 192);
        assert_eq!(UNIFORM_SIZE % 16, 0);
        assert_eq!(Uniforms::new(&framing(), (800, 600)).to_bytes().len(), 192);
    }

    #[test]
    fn the_matrix_lands_at_the_front_of_the_block() {
        let framing = framing();
        let uniforms = Uniforms::new(&framing, (800, 600));
        let bytes = uniforms.to_bytes();
        let matrix = framing.view_projection();
        for (column, values) in matrix.iter().enumerate() {
            for (row, value) in values.iter().enumerate() {
                let at = (column * 4 + row) * 4;
                let stored = f32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
                assert_eq!(stored, *value, "matrix [{column}][{row}]");
            }
        }
        // Followed immediately by the light, then the material's rgb + ambient.
        let light_at = 64;
        assert_eq!(
            f32::from_ne_bytes(bytes[light_at..light_at + 4].try_into().unwrap()),
            uniforms.light[0]
        );
        let ambient_at = 64 + 16 + 12;
        assert_eq!(
            f32::from_ne_bytes(bytes[ambient_at..ambient_at + 4].try_into().unwrap()),
            AMBIENT
        );
    }

    #[test]
    fn shading_values_are_normalised_and_shared_with_the_cpu_path() {
        let uniforms = Uniforms::new(&framing(), (320, 240));
        let length =
            (uniforms.light[0].powi(2) + uniforms.light[1].powi(2) + uniforms.light[2].powi(2))
                .sqrt();
        assert!((length - 1.0).abs() < 1e-5, "light must be a unit vector");
        assert_eq!(uniforms.params, [DIFFUSE, SPECULAR, SHININESS, VIGNETTE]);
        assert_eq!(uniforms.params2[0], POINT_RADIUS);
        assert_eq!(uniforms.params2[1], EDL_STRENGTH);
        // The GPU post pass rebuilds view depth from the depth buffer with
        // these two, so they have to invert the projection's own map
        // `ndc = z_scale + z_bias / w`.
        let (near, far) = framing().depth_range();
        for distance in [near, (near + far) * 0.5, far] {
            let ndc = uniforms.params2[2] + uniforms.params2[3] / distance;
            let back = uniforms.params2[3] / (ndc - uniforms.params2[2]);
            assert!((back - distance).abs() < 1e-4, "{distance} -> {back}");
        }
        assert_eq!(uniforms.material[3], AMBIENT);
        assert_eq!(uniforms.viewport[0], 320.0);
        assert_eq!(uniforms.viewport[1], 240.0);
        assert_eq!(uniforms.background[0][..3], BG_TOP);
        assert_eq!(uniforms.background[1][..3], BG_BOTTOM);
        // The eye is handed over in model space, which is where normals live.
        let back = framing().to_unit(uniforms.eye[..3].try_into().unwrap());
        assert!((back[0] - framing().eye[0]).abs() < 1e-4);
    }

    #[test]
    fn rows_are_padded_to_the_required_stride() {
        // Already aligned.
        assert_eq!(padded_row_bytes(64), 256);
        assert_eq!(padded_row_bytes(256), 1024);
        // Just over a boundary rounds up.
        assert_eq!(padded_row_bytes(65), 512);
        assert_eq!(padded_row_bytes(100), 512);
        assert_eq!(padded_row_bytes(1), 256);
        assert_eq!(staging_len(100, 10), 512 * 10);
    }

    #[test]
    fn readback_drops_the_padding_and_swizzles_to_bgra() {
        let (width, height) = (3u32, 2u32);
        let row = padded_row_bytes(width) as usize;
        assert!(row > width as usize * 4, "the test needs real padding");
        // Two rows of opaque red/green/blue, each padded out with 0xEE.
        let mut padded = vec![0xEEu8; row * height as usize];
        for y in 0..height as usize {
            for (x, rgb) in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]].iter().enumerate() {
                let at = y * row + x * 4;
                padded[at..at + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }

        let out = unpack_bgra(&padded, width, height, PixelOrder::Rgba);
        assert_eq!(out.len(), (width * height * 4) as usize);
        // Red RGBA becomes 0,0,255,255 in BGRA.
        assert_eq!(&out[0..4], &[0, 0, 255, 255]);
        assert_eq!(&out[4..8], &[0, 255, 0, 255]);
        assert_eq!(&out[8..12], &[255, 0, 0, 255]);
        // Row two repeats it, with no padding bytes leaking through.
        assert_eq!(&out[12..16], &[0, 0, 255, 255]);
        assert!(!out.contains(&0xEE));
    }

    /// A `Bgra8Unorm` target needs no swizzle, and the result has to be
    /// exactly what the RGBA path produces for the same picture — this is the
    /// fast path an interactive frame takes.
    #[test]
    fn a_bgra_target_produces_the_same_frame_as_an_rgba_one() {
        let (width, height) = (3u32, 2u32);
        let row = padded_row_bytes(width) as usize;
        let colours = [
            [255u8, 64, 8],
            [0, 255, 0],
            [16, 32, 255],
            [7, 7, 7],
            [200, 1, 1],
            [1, 2, 3],
        ];
        let mut rgba = vec![0xEEu8; row * height as usize];
        let mut bgra = vec![0xEEu8; row * height as usize];
        for (index, colour) in colours.iter().enumerate() {
            let (y, x) = (index / width as usize, index % width as usize);
            let at = y * row + x * 4;
            rgba[at..at + 4].copy_from_slice(&[colour[0], colour[1], colour[2], 1]);
            bgra[at..at + 4].copy_from_slice(&[colour[2], colour[1], colour[0], 1]);
        }

        let from_rgba = unpack_bgra(&rgba, width, height, PixelOrder::Rgba);
        let from_bgra = unpack_bgra(&bgra, width, height, PixelOrder::Bgra);
        assert_eq!(from_rgba, from_bgra);
        assert_eq!(&from_bgra[0..4], &[8, 64, 255, 255]);
    }

    /// When the row stride is already tight, the pixels are one run and the
    /// BGRA path copies them whole — still forcing the alpha opaque and still
    /// ignoring anything after the last row.
    #[test]
    fn a_tightly_packed_bgra_frame_is_copied_whole() {
        let width = 64u32;
        assert_eq!(
            padded_row_bytes(width),
            width * 4,
            "this width has no padding"
        );
        let mut frame = vec![9u8; width as usize * 4 * 2 + 32];
        for pixel in frame.as_chunks_mut::<4>().0 {
            pixel[3] = 0; // the copy must make these opaque
        }
        let out = unpack_bgra(&frame, width, 2, PixelOrder::Bgra);
        assert_eq!(
            out.len(),
            width as usize * 4 * 2,
            "the tail is not part of the frame"
        );
        assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        assert!(out.as_chunks::<4>().0.iter().all(|p| p[0] == 9));
    }

    #[test]
    fn a_short_readback_pads_instead_of_panicking() {
        let out = unpack_bgra(&[1, 2, 3, 4], 2, 2, PixelOrder::Rgba);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..4], &[3, 2, 1, 255]);
        // The missing pixels come back opaque black rather than absent.
        assert_eq!(&out[4..8], &[0, 0, 0, 255]);
    }
}
