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
    AMBIENT, BG_BOTTOM, BG_TOP, DIFFUSE, Framing, KEY_LIGHT, MATERIAL, SHININESS, SPECULAR,
    VIGNETTE,
};

/// Bytes of [`Uniforms`]: one `mat4x4<f32>` plus seven `vec4<f32>`.
pub const UNIFORM_SIZE: usize = 64 + 7 * 16;

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
        Self {
            view_proj: framing.view_projection(),
            light: [light[0], light[1], light[2], 0.0],
            material: [MATERIAL[0], MATERIAL[1], MATERIAL[2], AMBIENT],
            params: [DIFFUSE, SPECULAR, SHININESS, VIGNETTE],
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

/// Strip the row padding from a read-back frame and swizzle RGBA → BGRA.
///
/// The UI hands gpui the same BGRA buffer for both renderers, so this is what
/// makes a GPU frame and a CPU frame interchangeable. The alpha channel is
/// forced opaque: the viewport paints over the background itself.
pub fn unpack_bgra(padded: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let row = padded_row_bytes(width) as usize;
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let start = y * row;
        for x in 0..w {
            let i = start + x * 4;
            match padded.get(i..i + 4) {
                Some(pixel) => out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]),
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
            crate::media::mesh::Bounds {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 1.0, 1.0],
            },
            16.0 / 9.0,
        )
    }

    #[test]
    fn the_uniform_block_is_the_size_the_shader_expects() {
        // 64 bytes of matrix + seven vec4 = 176, a multiple of 16.
        assert_eq!(UNIFORM_SIZE, 176);
        assert_eq!(UNIFORM_SIZE % 16, 0);
        assert_eq!(Uniforms::new(&framing(), (800, 600)).to_bytes().len(), 176);
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

        let out = unpack_bgra(&padded, width, height);
        assert_eq!(out.len(), (width * height * 4) as usize);
        // Red RGBA becomes 0,0,255,255 in BGRA.
        assert_eq!(&out[0..4], &[0, 0, 255, 255]);
        assert_eq!(&out[4..8], &[0, 255, 0, 255]);
        assert_eq!(&out[8..12], &[255, 0, 0, 255]);
        // Row two repeats it, with no padding bytes leaking through.
        assert_eq!(&out[12..16], &[0, 0, 255, 255]);
        assert!(!out.contains(&0xEE));
    }

    #[test]
    fn a_short_readback_pads_instead_of_panicking() {
        let out = unpack_bgra(&[1, 2, 3, 4], 2, 2);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..4], &[3, 2, 1, 255]);
        // The missing pixels come back opaque black rather than absent.
        assert_eq!(&out[4..8], &[0, 0, 0, 255]);
    }
}
