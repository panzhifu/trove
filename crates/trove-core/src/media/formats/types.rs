//! Shared geometry types for the 3D model pipeline.
//!
//! Everything the model viewport, the two renderers and the off-screen path
//! see is the [`Mesh`] type defined here — the format-specific parsers in the
//! sibling modules normalise their output into it.

use std::collections::HashMap;

/// What a mesh's triangle winding says about it, for back-face culling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Winding {
    /// An open shell, or winding the file does not keep consistent. Both faces
    /// are visible: culling one would punch holes in it.
    #[default]
    TwoSided,
    /// A closed surface whose triangles face outward, counter-clockwise seen
    /// from outside.
    ClosedOutward,
    /// A closed surface wound inside-out. Culling its back faces would show
    /// the inside of the model, so the winding has to be flipped before
    /// culling anything.
    ClosedInward,
}

/// Triangles above which [`Mesh::winding`] gives up and reports two-sided.
const MAX_WINDING_CHECK_TRIANGLES: usize = 2_000_000;

/// Axis-aligned bounds of a mesh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Bounds {
    /// Bounds of an empty mesh.
    pub(crate) fn empty() -> Self {
        Self {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }

    /// Whether any vertex was folded in.
    pub fn is_empty(&self) -> bool {
        self.min[0] > self.max[0]
    }

    pub(crate) fn extend(&mut self, p: [f32; 3]) {
        for ((min, max), v) in self.min.iter_mut().zip(self.max.iter_mut()).zip(p) {
            *min = (*min).min(v);
            *max = (*max).max(v);
        }
    }

    /// Size along each axis.
    pub fn size(&self) -> [f32; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }

    /// Center of the box.
    pub fn center(&self) -> [f32; 3] {
        if self.is_empty() {
            return [0.0; 3];
        }
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    /// Longest edge, used to frame the model in the viewport.
    pub fn longest_edge(&self) -> f32 {
        let size = self.size();
        size[0].max(size[1]).max(size[2])
    }
}

impl Default for Bounds {
    fn default() -> Self {
        Self::empty()
    }
}

/// The per-point scalar channels a file carries beyond geometry — what
/// CloudCompare colours by when it is not colouring by height.
///
/// Both are parallel to the vertex arrays and both are optional: a PLY written
/// by a scanner has them, an OBJ or a simplified LOD level has not, and the
/// renderers treat a missing channel as "this model cannot be painted by it"
/// rather than as zeros, which would paint a convincing and wrong picture.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CloudFields {
    /// Raw intensity, unscaled: what the file says. Kept as read, so a 12-bit
    /// and a 16-bit cloud keep their own range and the colouring normalises
    /// against it.
    pub intensities: Vec<f32>,
    /// ASPRS-style classification, one byte per point. An index into a
    /// categorical palette, not a value to interpolate.
    pub classes: Vec<u8>,
}

impl CloudFields {
    fn is_empty(&self) -> bool {
        self.intensities.is_empty() && self.classes.is_empty()
    }

    /// The channels as they read after `remap` dropped vertices, each channel
    /// gone entirely if it did not cover every vertex to begin with — the same
    /// rule the normals follow.
    fn filtered(mut self, remap: &[u32]) -> Self {
        fn keep<T: Copy>(channel: &mut Vec<T>, remap: &[u32]) {
            if channel.len() != remap.len() {
                channel.clear();
                return;
            }
            let source = std::mem::take(channel);
            source
                .into_iter()
                .zip(remap)
                .filter(|(_, to)| **to != u32::MAX)
                .for_each(|(value, _)| channel.push(value));
        }
        keep(&mut self.intensities, remap);
        keep(&mut self.classes, remap);
        self
    }
}

/// Usable geometry in model space: a triangle mesh, or a point cloud when the
/// file carries no faces.
/// One decoded base-colour texture: RGBA8 bytes, sRGB-encoded, the layout
/// image decoders hand over and texture samplers expect.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextureMap {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The texture side of a mesh's materials, when the file carries any: the
/// decoded base-colour textures, the per-vertex UV that reads them, and the
/// slot each vertex samples. Boxed because a mesh without textures — most
/// files — should not pay for three empty `Vec`s inside every geometry the
/// renderers hand around.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextureData {
    /// Per-vertex UV, parallel to `positions`.
    pub uv: Vec<[f32; 2]>,
    /// Per-vertex texture slot, parallel to `positions`. [`NO_TEXTURE`]
    /// marks a vertex whose primitive carries no base-colour texture.
    pub slot: Vec<u16>,
    /// Per-vertex metallic-roughness texture slot, parallel to `positions`
    /// (glTF's G channel is roughness, B is metallic). [`NO_TEXTURE`] when
    /// the primitive has none and the factors stand alone.
    pub mr_slot: Vec<u16>,
    /// Per-vertex `(metallic factor, roughness factor)`, the multipliers the
    /// glTF spec pairs with the two channels above.
    pub factors: Vec<[f32; 2]>,
    /// Per-vertex normal-map texture slot, parallel to `positions`. The map
    /// is tangent-space, glTF's OpenGL convention, sampled with the same UV
    /// as the base colour. [`NO_TEXTURE`] when the primitive carries none and
    /// the geometric normal stands alone.
    pub normal_slot: Vec<u16>,
    /// Per-vertex normal-map strength — the material's `normalTexture.scale`,
    /// applied to the tangent-plane components before the vector normalises.
    pub normal_scale: Vec<f32>,
    /// Per-vertex ambient-occlusion texture slot, parallel to `positions`
    /// (glTF packs occlusion in the R channel). [`NO_TEXTURE`] when the
    /// primitive carries none.
    pub ao_slot: Vec<u16>,
    /// Per-vertex occlusion strength — the material's
    /// `occlusionTexture.strength`, lerping between unoccluded light and the
    /// sampled value.
    pub ao_strength: Vec<f32>,
    /// Per-vertex emissive texture slot, parallel to `positions`.
    /// [`NO_TEXTURE`] when the primitive carries none and the factor stands
    /// alone.
    pub emissive_slot: Vec<u16>,
    /// Per-vertex emissive factor — the material's `emissiveFactor`, the
    /// light the surface gives off before any texture multiplies it.
    pub emissive_factor: Vec<[f32; 3]>,
    /// Per-vertex alpha cutoff — the material's `alphaMode` folded into one
    /// number: negative for `OPAQUE` (no pixel is ever discarded), the
    /// `alphaCutoff` for `MASK`, and 0.5 for `BLEND`. The preview cannot blend
    /// a whole mesh in draw order, so a blended material clips like a mask —
    /// the read every cutout foliage export gets either way.
    pub alpha_cutoff: Vec<f32>,
    /// Per-vertex base-colour alpha factor — `baseColorFactor[3]`, the fourth
    /// multiplier of the texel alpha the cutoff tests against.
    pub alpha_factor: Vec<f32>,
    /// Per-vertex double-sided flag — the material's `doubleSided`. A
    /// double-sided material's back faces are the surface you see from
    /// behind, so the renderers must not cull them even on a mesh whose
    /// winding says closed.
    pub double_sided: Vec<bool>,
    /// The decoded textures, in slot order.
    pub maps: Vec<TextureMap>,
}

/// The slot value a vertex with no base-colour texture carries.
pub const NO_TEXTURE: u16 = u16::MAX;

/// Box-filter an RGBA8 image to new dimensions: each target pixel averages
/// the source pixels its cell covers, so shrinking keeps the average colour
/// rather than sampling a stray one. Same dimensions in and out is a copy.
pub fn resize_rgba(rgba: &[u8], width: u32, height: u32, tw: u32, th: u32) -> Vec<u8> {
    if (width, height) == (tw, th) || width == 0 || height == 0 || tw == 0 || th == 0 {
        return rgba.to_vec();
    }
    let (w, h) = (width as usize, height as usize);
    let (tw, th) = (tw as usize, th as usize);
    let mut out = vec![0u8; tw * th * 4];
    for ty in 0..th {
        let sy0 = ty * h / th;
        let sy1 = ((ty + 1) * h / th).max(sy0 + 1).min(h);
        for tx in 0..tw {
            let sx0 = tx * w / tw;
            let sx1 = ((tx + 1) * w / tw).max(sx0 + 1).min(w);
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            let mut count = 0u32;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let at = (sy * w + sx) * 4;
                    r += rgba[at] as u32;
                    g += rgba[at + 1] as u32;
                    b += rgba[at + 2] as u32;
                    a += rgba[at + 3] as u32;
                    count += 1;
                }
            }
            let at = (ty * tw + tx) * 4;
            out[at] = (r / count) as u8;
            out[at + 1] = (g / count) as u8;
            out[at + 2] = (b / count) as u8;
            out[at + 3] = (a / count) as u8;
        }
    }
    out
}

/// The mip chain of a square RGBA8 image: level 0 is the input itself, each
/// following level halves the edge with a 2×2 box filter, down to 1×1.
///
/// The filter averages in *scene-linear* space — each texel is decoded, the
/// decoded values averaged, and the result re-encoded — because the bytes are
/// display-encoded and averaging encoded values darkens a high-contrast edge
/// the way averaging log values would. This is what a GPU blit between sRGB
/// views does, and what the hardware expects a mip to hold, since the sampler
/// linearly interpolates the stored (encoded) bytes. Alpha averages as the raw
/// byte: it is coverage, not colour, and carries no transfer curve.
///
/// The chain is what the GPU preview's trilinear/anisotropic sampling reads;
/// the software path keeps its nearest-neighbour lookups and never touches it.
/// Non-power-of-two sizes floor-halve (`1023 → 511 → …`), matching the level
/// sizes `(dim >> level).max(1)` the upload names for every level.
pub fn mip_chain(rgba: &[u8], dim: u32) -> Vec<Vec<u8>> {
    let lut = decode_lut();
    let mut chain = vec![rgba.to_vec()];
    let mut size = dim.max(1);
    while size > 1 {
        let prev = chain.last().expect("the chain is never empty");
        let next = (size / 2).max(1);
        let (prev_w, next_w) = (size as usize, next as usize);
        let mut out = vec![0u8; next_w * next_w * 4];
        for ty in 0..next {
            for tx in 0..next {
                // The 2×2 cell, clamped at the edge of an odd-sized parent: a
                // doubled pixel is the area weight the halving deserves.
                let at = |x: u32, y: u32| {
                    ((y.min(size - 1) as usize) * prev_w + (x.min(size - 1) as usize)) * 4
                };
                let corners = [
                    at(tx * 2, ty * 2),
                    at(tx * 2 + 1, ty * 2),
                    at(tx * 2, ty * 2 + 1),
                    at(tx * 2 + 1, ty * 2 + 1),
                ];
                let mut texel = [0u8; 4];
                for channel in 0..3 {
                    let sum: f32 = corners
                        .iter()
                        .map(|&i| lut[prev[i + channel] as usize])
                        .sum();
                    texel[channel] = encode_byte(sum / 4.0);
                }
                let alpha: u32 = corners.iter().map(|&i| prev[i + 3] as u32).sum();
                texel[3] = (alpha / 4) as u8;
                let o = (ty as usize * next_w + tx as usize) * 4;
                out[o..o + 4].copy_from_slice(&texel);
            }
        }
        chain.push(out);
        size = next;
    }
    chain
}

/// Scene-linear → sRGB byte, the inverse of [`decode_lut`]: a 4096-entry
/// table, since the mip chain runs it once per texel per level and the
/// quantised output stays within one LSB of the exact curve.
fn encode_byte(linear: f32) -> u8 {
    static LUT: std::sync::OnceLock<[u8; 4097]> = std::sync::OnceLock::new();
    let lut = LUT.get_or_init(|| {
        let mut table = [0u8; 4097];
        for (entry, value) in table.iter_mut().enumerate() {
            let x = entry as f32 / 4096.0;
            let encoded = if x <= 0.0031308 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            };
            *value = (encoded.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        table
    });
    lut[(linear.clamp(0.0, 1.0) * 4096.0) as usize]
}

impl TextureMap {
    /// Sample the texture at UV `(u, v)` and decode to scene-linear.
    ///
    /// glTF's defaults all round: the wrapping repeats (`fract`), the origin
    /// is the image's top-left, and the lookup is nearest-neighbour — the
    /// preview's software path pays per pixel, and at thumbnail sizes the
    /// difference from bilinear is a rounding error.
    pub fn sample(&self, u: f32, v: f32) -> (f32, f32, f32) {
        let (w, h) = (self.width.max(1) as usize, self.height.max(1) as usize);
        let x = (u.rem_euclid(1.0) * w as f32) as usize % w;
        let y = (v.rem_euclid(1.0) * h as f32) as usize % h;
        let at = (y * w + x) * 4;
        let px = &self.rgba[at..at + 3];
        let lut = decode_lut();
        (
            lut[px[0] as usize],
            lut[px[1] as usize],
            lut[px[2] as usize],
        )
    }

    /// The texel's alpha at UV `(u, v)`, in 0..=1.
    ///
    /// Alpha is coverage, not colour: it is read as the raw byte scaled, the
    /// way both the shader and the glTF spec treat it — no sRGB decode.
    pub fn sample_alpha(&self, u: f32, v: f32) -> f32 {
        let (w, h) = (self.width.max(1) as usize, self.height.max(1) as usize);
        let x = (u.rem_euclid(1.0) * w as f32) as usize % w;
        let y = (v.rem_euclid(1.0) * h as f32) as usize % h;
        self.rgba[(y * w + x) * 4 + 3] as f32 / 255.0
    }
}

/// sRGB byte → scene-linear, as a 256-entry table: the texture bytes are
/// display-encoded, the shading is linear, and the conversion sits between —
/// the same transform the display step runs in reverse.
fn decode_lut() -> &'static [f32; 256] {
    static LUT: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut table = [0f32; 256];
        for (entry, value) in table.iter_mut().enumerate() {
            let x = entry as f32 / 255.0;
            *value = if x <= 0.04045 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            };
        }
        table
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    /// Per-vertex normals; empty (or a different length than `positions`)
    /// means the file carries none and the renderer shades per face.
    pub normals: Vec<[f32; 3]>,
    /// Per-vertex colour in 0..=1, empty when the file carries none. Read by
    /// the point renderer; a triangle mesh shades from the material instead.
    pub colors: Vec<[f32; 3]>,
    /// Triangles; empty for a point cloud, which is drawn vertex by vertex.
    pub triangles: Vec<[u32; 3]>,
    pub bounds: Bounds,
    /// The scalar channels, for a file that carries any. Boxed because a mesh
    /// without them — which is most files — should not pay for two empty `Vec`s
    /// inside every geometry the renderers hand around.
    pub fields: Option<Box<CloudFields>>,
    /// Base-colour texture mapping, when the file carries one.
    pub texture: Option<Box<TextureData>>,
}

impl Mesh {
    /// Number of triangles.
    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// Number of vertices.
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    /// A cloud of points rather than a surface: there is nothing to
    /// rasterise, so the renderers draw one sprite per vertex.
    pub fn is_point_cloud(&self) -> bool {
        self.triangles.is_empty() && !self.positions.is_empty()
    }

    /// Whether the mesh carries usable base-colour textures: a texture table
    /// plus a UV per vertex, lining up with `positions`.
    pub fn has_textures(&self) -> bool {
        matches!(&self.texture, Some(t)
            if !t.maps.is_empty() && t.uv.len() == self.positions.len())
    }

    /// Whether any material on the mesh declared itself double-sided.
    ///
    /// The GPU culls back faces per pipeline, not per material, so one
    /// double-sided material anywhere in the file takes the whole mesh off
    /// the culled pipeline — the single-sided materials get their culling in
    /// the fragment stage instead, from this same per-vertex flag.
    pub fn has_double_sided_material(&self) -> bool {
        self.texture
            .as_ref()
            .is_some_and(|t| t.double_sided.iter().any(|doubled| *doubled))
    }

    /// What the frame-size heuristics count: triangles for a mesh, points for
    /// a cloud. Both grow the work per frame, so they share one knob.
    pub fn primitive_count(&self) -> usize {
        if self.is_point_cloud() {
            self.positions.len()
        } else {
            self.triangles.len()
        }
    }

    /// What the triangles' winding says about this mesh.
    ///
    /// A closed, consistently wound surface is the only case where a back face
    /// is guaranteed to be hidden: culling it then halves the fragments a
    /// renderer has to shade, and does it without changing the picture. An
    /// open shell or a file with inconsistent winding keeps both faces, or a
    /// single-sided scan would develop holes seen from behind.
    pub fn winding(&self) -> Winding {
        self.winding_capped(MAX_WINDING_CHECK_TRIANGLES)
    }

    /// [`Mesh::winding`] with the work cap made explicit, for tests.
    ///
    /// The check is a pass over the triangles with a hash map of edges, so it
    /// is only worth doing while the map stays smaller than the geometry it
    /// describes. Above the cap the mesh is reported as two-sided, which is
    /// always safe.
    pub(crate) fn winding_capped(&self, cap: usize) -> Winding {
        if self.triangles.is_empty() || self.triangles.len() > cap {
            return Winding::TwoSided;
        }
        // Closed and consistent: every directed edge occurs once and its
        // reverse occurs once. Anything else is a boundary or a fold.
        let mut edges: HashMap<(u32, u32), u32> = HashMap::with_capacity(self.triangles.len() * 3);
        for triangle in &self.triangles {
            for (from, to) in [
                (triangle[0], triangle[1]),
                (triangle[1], triangle[2]),
                (triangle[2], triangle[0]),
            ] {
                *edges.entry((from, to)).or_insert(0) += 1;
            }
        }
        for (&(from, to), &count) in &edges {
            if count != 1 || edges.get(&(to, from)) != Some(&1) {
                return Winding::TwoSided;
            }
        }
        // Which way it faces: the signed volume of a closed surface is
        // positive when its triangles wind counter-clockwise as seen from
        // outside.
        let mut volume = 0.0f64;
        for triangle in &self.triangles {
            let a = self.positions[triangle[0] as usize];
            let b = self.positions[triangle[1] as usize];
            let c = self.positions[triangle[2] as usize];
            volume += (a[0] as f64 * (b[1] as f64 * c[2] as f64 - b[2] as f64 * c[1] as f64)
                + a[1] as f64 * (b[2] as f64 * c[0] as f64 - b[0] as f64 * c[2] as f64)
                + a[2] as f64 * (b[0] as f64 * c[1] as f64 - b[1] as f64 * c[0] as f64))
                / 6.0;
        }
        if volume >= 0.0 {
            Winding::ClosedOutward
        } else {
            Winding::ClosedInward
        }
    }

    /// Whether the model has usable per-vertex normals.
    pub fn has_vertex_normals(&self) -> bool {
        self.normals.len() == self.positions.len() && !self.normals.is_empty()
    }

    /// Whether the model has a usable colour per vertex.
    pub fn has_vertex_colors(&self) -> bool {
        self.colors.len() == self.positions.len() && !self.colors.is_empty()
    }

    /// Whether intensity can be painted on this model, and the range it spans.
    ///
    /// Only a point cloud qualifies: the channels ride the point instance buffer,
    /// which the triangle pipeline does not read, so a mesh that somehow carried
    /// one would have nothing to draw it with.
    pub fn intensity_range(&self) -> Option<(f32, f32)> {
        let intensities = &self.fields.as_ref()?.intensities;
        if !(self.is_point_cloud() && intensities.len() == self.positions.len()) {
            return None;
        }
        let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
        for value in intensities.iter().filter(|v| v.is_finite()) {
            min = min.min(*value);
            max = max.max(*value);
        }
        // Nothing finite is as good as no channel at all: a range between two
        // infinities would be a colour painted out of nothing.
        (min <= max).then_some((min, max))
    }

    /// The highest classification present, plus one: the count a categorical
    /// palette has to cover. `None` when there is no channel to read.
    pub fn class_count(&self) -> Option<usize> {
        let classes = &self.fields.as_ref()?.classes;
        (self.is_point_cloud() && classes.len() == self.positions.len()).then(|| {
            classes
                .iter()
                .copied()
                .max()
                .map_or(1, |max| max as usize + 1)
        })
    }

    /// Assemble a mesh from geometry that was not read from a file — a
    /// simplified LOD level, above all.
    ///
    /// The bounds are computed here and the per-vertex arrays are validated,
    /// so a caller cannot hand the renderers normals that do not line up with
    /// its positions. Note what is *not* checked: passing no triangles builds
    /// a point cloud ([`Mesh::is_point_cloud`]), which is a legitimate cloud
    /// but also what a forgotten `triangles` argument looks like.
    pub fn from_parts(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    ) -> Option<Self> {
        Self::assemble(positions, normals, colors, triangles)
    }

    /// Build a mesh from raw positions/triangles, dropping degenerate
    /// triangles and recomputing the bounds. Returns `None` when nothing
    /// usable is left.
    pub(crate) fn finish(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    ) -> Option<Self> {
        let mesh = Self::assemble(positions, normals, colors, triangles)?;
        (!mesh.is_point_cloud()).then_some(mesh)
    }

    /// Build a point cloud: positions only, no triangles to filter.
    pub(crate) fn finish_points(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
    ) -> Option<Self> {
        Self::assemble(positions, normals, colors, Vec::new())
    }

    /// A point cloud that also carried intensity and/or classification. The
    /// point-cloud readers' path; every other reader keeps [`finish_points`].
    pub(crate) fn finish_points_with(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        fields: CloudFields,
    ) -> Option<Self> {
        Self::assemble_with(positions, normals, colors, Vec::new(), fields)
    }

    /// Geometry with no scalar channels, which is what every format reader that
    /// has none hands over.
    fn assemble(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    ) -> Option<Self> {
        Self::assemble_with(
            positions,
            normals,
            colors,
            triangles,
            CloudFields::default(),
        )
    }

    /// Shared tail of the constructors: checks the vertices are usable, drops
    /// degenerate triangles and computes the bounds.
    fn assemble_with(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        colors: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
        fields: CloudFields,
    ) -> Option<Self> {
        // Normalise non-finite coordinates away first: nothing downstream can
        // use them, and several things (the bounds, the winding check, the
        // point-cloud octree) misbehave in ways that run from wrong to
        // non-terminating. Doing it once here covers every format reader.
        let (geometry, remap) = drop_non_finite((positions, normals, colors, triangles));
        let (positions, normals, colors, triangles) = geometry;
        let fields = fields.filtered(&remap);
        if positions.is_empty() {
            return None;
        }
        let count = positions.len() as u32;
        let triangles: Vec<[u32; 3]> = triangles
            .into_iter()
            .filter(|t| t[0] < count && t[1] < count && t[2] < count)
            .filter(|t| t[0] != t[1] && t[1] != t[2] && t[0] != t[2])
            .collect();
        let mut bounds = Bounds::empty();
        for p in &positions {
            bounds.extend(*p);
        }
        if bounds.is_empty() {
            return None;
        }
        let normals = if normals.len() == positions.len() {
            normals
        } else {
            Vec::new()
        };
        let colors = if colors.len() == positions.len() {
            colors
        } else {
            Vec::new()
        };
        Some(Self {
            positions,
            normals,
            colors,
            triangles,
            bounds,
            fields: (!fields.is_empty()).then(|| Box::new(fields)),
            texture: None,
        })
    }
}

/// The four parallel arrays a [`Mesh`] is assembled from.
///
/// Named so the filtering below can hand all four back without tripping
/// clippy's complexity lint on a four-tuple return.
type MeshArrays = (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[u32; 3]>);

/// Drop every vertex whose coordinate is not finite, remapping the triangles
/// onto what is left.
///
/// A NaN or infinite coordinate is not a position: it can never lie inside a
/// box, every comparison against it is false, and it poisons the bounds, the
/// winding check and the point-cloud octree in turn. Scans write NaN for
/// unobserved points often enough that normalising once here — for every
/// format — is cheaper than teaching each reader about it. The all-finite
/// case, which is the common one, moves the arrays through untouched.
fn drop_non_finite(geometry: MeshArrays) -> (MeshArrays, Vec<u32>) {
    let (positions, normals, colors, triangles) = geometry;
    if positions.iter().all(|p| p.iter().all(|v| v.is_finite())) {
        // Nothing dropped, so the identity map lets the scalar channels fall
        // through the same filter as everything else.
        let identity: Vec<u32> = (0..positions.len() as u32).collect();
        return ((positions, normals, colors, triangles), identity);
    }
    let mut remap = vec![u32::MAX; positions.len()];
    let mut kept = Vec::with_capacity(positions.len());
    for (old, position) in positions.iter().enumerate() {
        if position.iter().all(|v| v.is_finite()) {
            remap[old] = kept.len() as u32;
            kept.push(*position);
        }
    }
    // A parallel array is only meaningful when it covers the same vertices;
    // one that does not is dropped, exactly as the caller would have done.
    let filter_parallel = |data: Vec<[f32; 3]>| -> Vec<[f32; 3]> {
        if data.len() != remap.len() {
            return Vec::new();
        }
        data.into_iter()
            .enumerate()
            .filter(|(index, _)| remap[*index] != u32::MAX)
            .map(|(_, value)| value)
            .collect()
    };
    let normals = filter_parallel(normals);
    let colors = filter_parallel(colors);
    let triangles = triangles
        .into_iter()
        .filter_map(|t| {
            let a = *remap.get(t[0] as usize).unwrap_or(&u32::MAX);
            let b = *remap.get(t[1] as usize).unwrap_or(&u32::MAX);
            let c = *remap.get(t[2] as usize).unwrap_or(&u32::MAX);
            (a != u32::MAX && b != u32::MAX && c != u32::MAX).then_some([a, b, c])
        })
        .collect();
    ((kept, normals, colors, triangles), remap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh(positions: Vec<[f32; 3]>, triangles: Vec<[u32; 3]>) -> Mesh {
        Mesh::from_parts(positions, Vec::new(), Vec::new(), triangles).expect("mesh builds")
    }

    /// A cube, wound counter-clockwise as seen from outside.
    fn cube() -> Mesh {
        mesh(
            vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
                [1.0, 0.0, 1.0],
                [1.0, 1.0, 1.0],
                [0.0, 1.0, 1.0],
            ],
            vec![
                // z = 0 face, seen from outside (-z): clockwise in xy, so the
                // corners must be listed in that order.
                [0, 2, 1],
                [0, 3, 2],
                // z = 1 face, seen from outside (+z).
                [4, 5, 6],
                [4, 6, 7],
                // y = 0 face, outside is -y.
                [0, 1, 5],
                [0, 5, 4],
                // y = 1 face, outside is +y.
                [3, 7, 6],
                [3, 6, 2],
                // x = 0 face, outside is -x.
                [0, 4, 7],
                [0, 7, 3],
                // x = 1 face, outside is +x.
                [1, 2, 6],
                [1, 6, 5],
            ],
        )
    }

    #[test]
    fn a_closed_cube_winds_outward() {
        assert_eq!(cube().winding(), Winding::ClosedOutward);
    }

    /// The same surface with every triangle flipped is still closed, but it
    /// faces inward — culling its back faces would show the model's inside,
    /// so the renderer has to reverse it on the way into its buffers.
    #[test]
    fn a_flipped_cube_winds_inward() {
        let mut inside_out = cube();
        for triangle in &mut inside_out.triangles {
            triangle.swap(1, 2);
        }
        assert_eq!(inside_out.winding(), Winding::ClosedInward);
        assert_eq!(inside_out.bounds, cube().bounds);
    }

    /// An open shell has to keep both faces, or it disappears when seen from
    /// behind.
    #[test]
    fn an_open_shell_is_two_sided() {
        let quad = mesh(
            vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            vec![[0, 1, 2], [0, 2, 3]],
        );
        assert_eq!(quad.winding(), Winding::TwoSided);
    }

    /// A cube with one face wound the other way is not consistently wound: it
    /// is neither closed nor safe to cull.
    #[test]
    fn inconsistent_winding_is_two_sided() {
        let mut broken = cube();
        broken.triangles[4].swap(1, 2);
        assert_eq!(broken.winding(), Winding::TwoSided);
    }

    /// The check is a pass with a hash map over the edges, so it is capped:
    /// past the cap the mesh is simply reported as two-sided.
    #[test]
    fn the_winding_check_gives_up_past_its_cap() {
        let cube = cube();
        assert_eq!(cube.winding_capped(11), Winding::TwoSided);
        assert_eq!(cube.winding_capped(12), Winding::ClosedOutward);
    }

    /// A point cloud has no winding to speak of, and must not be culled.
    #[test]
    fn a_point_cloud_is_two_sided() {
        let cloud = mesh(vec![[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]], Vec::new());
        assert!(cloud.is_point_cloud());
        assert_eq!(cloud.winding(), Winding::TwoSided);
    }

    /// A coordinate that is not finite is not a vertex: it is dropped, its
    /// triangles go with it, and the survivors keep their colours.
    #[test]
    fn non_finite_vertices_are_dropped_with_their_triangles() {
        let mesh = Mesh::from_parts(
            vec![
                [0.0, 0.0, 0.0],
                [f32::NAN, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [f32::INFINITY, 0.0, 0.0],
                [0.0, 1.0, 0.0],
            ],
            Vec::new(),
            vec![
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
                [1.0, 1.0, 1.0],
                [0.5, 0.5, 0.5],
            ],
            vec![[0, 2, 4], [0, 1, 2]],
        )
        .expect("the finite vertices still build a mesh");
        assert_eq!(
            mesh.positions,
            vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
        );
        // The colours travel with their own vertices.
        assert_eq!(
            mesh.colors,
            vec![[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.5, 0.5, 0.5]]
        );
        // The triangle that referenced a dropped vertex is gone; the one made
        // of survivors is remapped onto their new indices.
        assert_eq!(mesh.triangles, vec![[0, 1, 2]]);
        assert!(mesh.bounds.min.iter().all(|v| v.is_finite()));
        assert!(mesh.bounds.max.iter().all(|v| v.is_finite()));
    }

    /// A cloud of only non-finite points is empty, not a mesh with NaN
    /// bounds: nothing about it is drawable.
    #[test]
    fn a_cloud_of_only_non_finite_points_is_empty() {
        assert!(
            Mesh::from_parts(
                vec![[f32::NAN; 3], [f32::INFINITY; 3]],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .is_none()
        );
    }

    /// The mip chain halves a square image down to 1×1, and averages in
    /// scene-linear space: a texel that is a quarter white does not average
    /// the encoded bytes (64), it encodes the average linear value — that is
    /// what the sampler's linear interpolation of the stored mip expects,
    /// and what a GPU blit between sRGB views would bake.
    #[test]
    fn the_mip_chain_halves_in_linear_space() {
        // 2×2: three black texels and one white one, alpha 255 everywhere.
        let mut base = vec![0u8; 16];
        for pixel in base.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        base[12..15].copy_from_slice(&[255, 255, 255]);
        let chain = mip_chain(&base, 2);
        assert_eq!(chain.len(), 2, "a 2×2 image carries base and 1×1");
        assert_eq!(chain[1].len(), 4);
        // decode(0) = 0 and decode(255) = 1; the average linear value is
        // 0.25, which the sRGB curve encodes back to byte 137.
        let grey = chain[1][0];
        assert_eq!(
            grey, 137,
            "a quarter-white texel is linear 0.25 re-encoded, got {grey}"
        );
        assert_eq!(chain[1][3], 255, "alpha averages raw, no transfer curve");
    }

    /// An odd-sized edge floors its way down (`3 → 1`), matching the level
    /// sizes `(dim >> level).max(1)` the GPU upload names — a chain the
    /// texture does not agree with would sample garbage — and a uniform
    /// image survives every level unchanged, the round trip through the
    /// transfer curve landing back on its own byte.
    #[test]
    fn the_mip_chain_floors_odd_sizes() {
        let mut base = vec![128u8; 3 * 3 * 4];
        for pixel in base.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        let chain = mip_chain(&base, 3);
        let texels: Vec<usize> = chain.iter().map(|level| level.len() / 4).collect();
        assert_eq!(texels, vec![9, 1], "3 floors to 1, one level below base");
        assert!(
            chain
                .iter()
                .all(|level| level.chunks(4).all(|px| px == [128, 128, 128, 255])),
            "a uniform image re-encodes to itself"
        );
    }
}
