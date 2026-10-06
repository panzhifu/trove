//! glTF 2.0 parser (`.gltf` / `.glb`).
//!
//! Uses the `gltf` crate to handle the JSON schema, buffer resolution and
//! accessor types — these are too involved to hand-write. The parsed document
//! is then flattened into a single [`Mesh`] the same way OBJ/STL/PLY are,
//! so the renderer never needs to know the source format.
//!
//! Two things the flattened mesh has to get right: a glTF mesh lives in a
//! node, and a node's transform is what places it (every Blender or GLB
//! export carries at least a Y-up rotation), so the scene graph is walked and
//! each node's transform is applied to its vertices. And a primitive's mode
//! decides how its indices form triangles — reading a point or strip
//! primitive as a triangle list yields garbage, so each mode is converted
//! explicitly.

use std::collections::HashSet;
use std::path::Path;

use image::GenericImageView as _;

use super::types::{Mesh, NO_TEXTURE, TextureData, TextureMap, resize_rgba};

/// Load a glTF or GLB file. Buffers are resolved relative to the file.
///
/// Images are decoded here, and the parse never asks the gltf crate for them:
/// its own import decodes every texture with the codecs it ships with — png
/// and jpeg only — only for this module to decode them again from the raw
/// buffer views, so the crate's pass is a second full decode of every image
/// in the file, and a Sketchfab model's webp textures would fail the whole
/// import and leave the model flat.
pub fn load_gltf(path: &Path) -> Result<Mesh, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("failed to load glTF: {e}"))?;
    // The document is validated when it can be. A file whose
    // `extensionsRequired` names an extension this build of the crate was not
    // compiled with is refused on sight — the refusal covers the whole file,
    // geometry included — but the geometry is still perfectly readable, so
    // the parse is retried without validation: the extension's own material
    // block is read where the crate knows it (the feature is enabled), and
    // ignored where it does not. A file that fails this way too was never
    // going to open, and the validation error — which names the extension —
    // is the more useful one to report.
    let parsed = match gltf::Gltf::from_slice(&bytes) {
        Ok(gltf) => gltf,
        Err(original) => {
            let gltf = gltf::Gltf::from_slice_without_validation(&bytes)
                .map_err(|_| format!("failed to load glTF: {original}"))?;
            tracing::info!(
                path = %path.display(),
                %original,
                "glTF imported without validation"
            );
            gltf
        }
    };
    let gltf::Gltf { document, blob } = parsed;
    let buffers = gltf::import_buffers(&document, Some(path), blob)
        .map_err(|e| format!("failed to load glTF: {e}"))?;
    // Every image the document's textures reference becomes one texture slot,
    // decoded once and shared (two materials on one image share the slot).
    // The per-texture slot table comes out alongside: a texture whose source
    // points past the image table — possible on the validation-free fallback
    // path — maps to no slot rather than stopping the parse.
    // Slots are the document image order — `decode_images` walks exactly
    // that — so a texture names its image index, capped at the table.
    let maps = decode_images(&document, &buffers, path);
    let slot_of_texture: Vec<u16> = document
        .textures()
        .map(|texture| {
            texture_source_index(&texture)
                .and_then(|index| {
                    (index < maps.len()).then(|| index.min(NO_TEXTURE as usize - 1) as u16)
                })
                .unwrap_or(NO_TEXTURE)
        })
        .collect();

    let mut builder = Builder {
        textures: maps,
        ..Default::default()
    };
    for node in root_nodes(&document) {
        builder.add_node(&node, IDENTITY, &buffers, &slot_of_texture);
    }
    builder.finish()
}

/// Decode every image the document declares into a texture slot, in document
/// order. A decode failure costs that image its slot, never the model. The
/// decodes run across the rayon pool: a file of 4K PNGs spends most of its
/// parse inside the decoders, and they are embarrassingly parallel.
fn decode_images(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    path: &Path,
) -> Vec<TextureMap> {
    use rayon::prelude::*;

    let images: Vec<gltf::Image<'_>> = document.images().collect();
    images
        .par_iter()
        .filter_map(|image| {
            let bytes = match image.source() {
                gltf::image::Source::View { view, .. } => {
                    let buffer = buffers.get(view.buffer().index())?;
                    let start = view.offset();
                    Some(buffer[start..start + view.length()].to_vec())
                }
                gltf::image::Source::Uri { uri, .. } => {
                    std::fs::read(path.parent()?.join(uri)).ok()
                }
            }?;
            let decoded = image::load_from_memory(&bytes).ok()?;
            let (width, height) = decoded.dimensions();
            let rgba = decoded.to_rgba8().into_raw();
            Some(finish_texture(rgba, width, height))
        })
        .collect()
}

/// The nodes the default scene draws — or, for a document that names no
/// scene, the first one, or finally every node no other node adopts.
///
/// Walking *all* nodes instead would draw each child a second time under its
/// own identity, without its parent's transform.
fn root_nodes(document: &gltf::Document) -> Vec<gltf::Node<'_>> {
    if let Some(scene) = document.default_scene() {
        return scene.nodes().collect();
    }
    if let Some(scene) = document.scenes().next() {
        return scene.nodes().collect();
    }
    let children: HashSet<usize> = document
        .nodes()
        .flat_map(|node| node.children().map(|child| child.index()))
        .collect();
    document
        .nodes()
        .filter(|node| !children.contains(&node.index()))
        .collect()
}

/// A glTF document flattened into one mesh.
#[derive(Default)]
struct Builder {
    positions: Vec<[f32; 3]>,
    /// Parallel to `positions`; placeholder zeros where a primitive carried
    /// none, dropped as a set by `finish` when `normals_incomplete` is set.
    normals: Vec<[f32; 3]>,
    /// Some vertex so far came without a normal.
    normals_incomplete: bool,
    /// Parallel to `positions`; the material colour each vertex is painted
    /// with — `COLOR_0` times the base colour factor, or the factor alone.
    /// Placeholder zeros where a primitive carried neither, dropped as a set
    /// by `finish` when no primitive carried a real material colour: shading
    /// half a model by its materials and half by the flat default reads as a
    /// bug, so the set stands or falls together.
    colors: Vec<[f32; 3]>,
    /// Any primitive carried a real material colour — a `COLOR_0`, a
    /// base-colour texture, or a non-default base colour factor. `finish`
    /// keeps the colour set only when this is set: a document with no
    /// material anywhere keeps the flat default it always had, while one
    /// with materials paints *every* vertex, the default-white ones
    /// included (the spec gives each material a total base colour factor —
    /// white is a material, not a missing one).
    material_colored: bool,
    /// The decoded base-colour textures, in slot order; empty when the
    /// document carries none.
    textures: Vec<TextureMap>,
    /// Per-vertex UV, parallel to `positions`.
    uv: Vec<[f32; 2]>,
    /// Per-vertex texture slot, parallel to `positions`.
    slots: Vec<u16>,
    /// Per-vertex metallic-roughness texture slot, parallel to `positions`.
    mr_slots: Vec<u16>,
    /// Per-vertex `(metallic factor, roughness factor)`, parallel to
    /// `positions`.
    factors: Vec<[f32; 2]>,
    /// Per-vertex normal-map texture slot, parallel to `positions`.
    normal_slot: Vec<u16>,
    /// Per-vertex normal-map strength, parallel to `positions`.
    normal_scale: Vec<f32>,
    /// Per-vertex ambient-occlusion texture slot, parallel to `positions`.
    ao_slot: Vec<u16>,
    /// Per-vertex occlusion strength, parallel to `positions`.
    ao_strength: Vec<f32>,
    /// Per-vertex emissive texture slot, parallel to `positions`.
    emissive_slot: Vec<u16>,
    /// Per-vertex emissive factor, parallel to `positions`.
    emissive_factors: Vec<[f32; 3]>,
    /// Per-vertex alpha cutoff and base-colour alpha factor, parallel to
    /// `positions`. Negative cutoff = opaque.
    alpha_cutoff: Vec<f32>,
    alpha_factor: Vec<f32>,
    /// Per-vertex double-sided flag, parallel to `positions`.
    double_sided: Vec<bool>,
    /// Vertices of `POINTS` primitives. Used only when the document has no
    /// triangles at all: a mesh is either a surface or a cloud.
    points: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
}

impl Builder {
    fn add_node(
        &mut self,
        node: &gltf::Node<'_>,
        parent: [[f32; 4]; 4],
        buffers: &[gltf::buffer::Data],
        slot_of_texture: &[u16],
    ) {
        // A node's placement is its own transform ON TOP OF every ancestor's:
        // the walk composes them, so a part nested under a rotated, scaled
        // root — the shape every Sketchfab or Blender export has — lands
        // where the scene puts it, not where its local transform alone would
        // leave it.
        let combined = mul4(parent, node.transform().matrix());
        if let Some(mesh) = node.mesh() {
            for primitive in mesh.primitives() {
                self.add_primitive(&primitive, combined, buffers, slot_of_texture);
            }
        }
        for child in node.children() {
            self.add_node(&child, combined, buffers, slot_of_texture);
        }
    }

    fn add_primitive(
        &mut self,
        primitive: &gltf::Primitive<'_>,
        transform: [[f32; 4]; 4],
        buffers: &[gltf::buffer::Data],
        slot_of_texture: &[u16],
    ) {
        let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(|data| &data[..]));
        let Some(positions) = reader.read_positions() else {
            return; // A primitive with no positions is not drawable.
        };
        let local: Vec<[f32; 3]> = positions
            .map(|position| transform_point(transform, position))
            .collect();
        if local.is_empty() {
            return;
        }

        if primitive.mode() == gltf::mesh::Mode::Points {
            self.points.extend(local);
            return;
        }

        let base = self.positions.len() as u32;
        let count = local.len();
        let normals: Option<Vec<[f32; 3]>> = reader.read_normals().map(|normals| {
            normals
                .map(|normal| transform_normal(transform, normal))
                .collect()
        });
        match normals {
            Some(normals) if normals.len() == local.len() => {
                self.normals.extend(normals);
            }
            _ => {
                // One primitive without normals drops the whole set: shading
                // half a model smooth and half flat looks broken, and a
                // half-length array would index out of bounds.
                self.normals_incomplete = true;
                self.normals
                    .extend(std::iter::repeat_n([0.0, 0.0, 0.0], local.len()));
            }
        }
        self.positions.extend(local);

        // The material colour each corner is painted with: the `COLOR_0`
        // attribute times the primitive's base colour factor, per the glTF
        // spec — the factor multiplies everything the material draws, the
        // texture included. A specular-glossiness material has no
        // metallic-roughness block — the crate hands back the default for it —
        // so its diffuse is the colour to read when the extension is there.
        let material = primitive.material();
        let base_texture = material
            .pbr_metallic_roughness()
            .base_color_texture()
            .or_else(|| {
                material
                    .pbr_specular_glossiness()
                    .and_then(|sg| sg.diffuse_texture())
            });
        let factor = material
            .pbr_specular_glossiness()
            .map(|sg| sg.diffuse_factor())
            .unwrap_or_else(|| material.pbr_metallic_roughness().base_color_factor());
        match reader.read_colors(0) {
            Some(colors) => {
                for color in colors.into_rgb_f32() {
                    self.colors.push([
                        color[0] * factor[0],
                        color[1] * factor[1],
                        color[2] * factor[2],
                    ]);
                }
                self.material_colored = true;
                debug_assert_eq!(self.colors.len() - base as usize, count);
            }
            // A textured primitive's texture carries its colour, so white is
            // the multiply that leaves it alone. A factor that is not the
            // spec default is a material statement of its own: the paint the
            // primitive wears, as read by every importer.
            None if base_texture.is_some() || factor[..3] != [1.0, 1.0, 1.0] => {
                self.material_colored = true;
                self.colors.extend(std::iter::repeat_n(
                    [factor[0], factor[1], factor[2]],
                    count,
                ));
            }
            // No `COLOR_0` and the default factor: the primitive paints
            // white — which is a *colour*, not a missing one. The factor is
            // total per the spec, so the set fills rather than turning
            // incomplete: the old poison here let one default-material part
            // (a white bulb on a black lamp) erase every other part's colour
            // and drop the whole model back to bare clay.
            None => {
                self.colors.extend(std::iter::repeat_n(
                    [factor[0], factor[1], factor[2]],
                    count,
                ));
            }
        }

        // The texture this primitive samples, and the UV it samples with. A
        // primitive with neither carries the no-texture marker — the arrays
        // stay parallel to the positions either way.
        let slot = base_texture
            .as_ref()
            .map(|info| {
                slot_of_texture
                    .get(info.texture().index())
                    .copied()
                    .unwrap_or(NO_TEXTURE)
            })
            .unwrap_or(NO_TEXTURE);
        // The metallic-roughness texture: its G channel carries roughness and
        // its B channel metallic, both scaled by the material's factors. The
        // factors default to the spec's (1.0, 1.0) — with the texture present
        // that is the real data; without one they name a full metal at full
        // roughness, which is what Blender's importer reads too.
        let mr_slot = material
            .pbr_metallic_roughness()
            .metallic_roughness_texture()
            .map(|info| {
                slot_of_texture
                    .get(info.texture().index())
                    .copied()
                    .unwrap_or(NO_TEXTURE)
            })
            .unwrap_or(NO_TEXTURE);
        // A specular-glossiness material has no metallic-roughness block —
        // the crate hands back the spec defaults for it, full metal at full
        // roughness — so its own numbers are read instead: glossiness is
        // roughness inverted, and the specular factor is a dielectric tint
        // rather than a metal, the way Blender's importer converts it. Read
        // as the defaults, the GPU preview multiplies the whole diffuse away
        // and paints the file black.
        let factors = material
            .pbr_specular_glossiness()
            .map(|sg| [0.0, (1.0 - sg.glossiness_factor()).clamp(0.0, 1.0)])
            .unwrap_or_else(|| {
                let pbr = material.pbr_metallic_roughness();
                [pbr.metallic_factor(), pbr.roughness_factor()]
            });
        // The normal map relights the surface and the occlusion map darkens
        // the lighting where the file says it is shadowed; both ride the same
        // per-vertex slot table as the base colour, with their per-material
        // strengths alongside.
        let normal_texture = material.normal_texture();
        let normal_slot = normal_texture
            .as_ref()
            .and_then(|info| slot_of_texture.get(info.texture().index()).copied())
            .unwrap_or(NO_TEXTURE);
        let normal_scale = normal_texture.map(|info| info.scale()).unwrap_or(1.0);
        let ao_texture = material.occlusion_texture();
        let ao_slot = ao_texture
            .as_ref()
            .and_then(|info| slot_of_texture.get(info.texture().index()).copied())
            .unwrap_or(NO_TEXTURE);
        let ao_strength = ao_texture.map(|info| info.strength()).unwrap_or(1.0);
        // Emissive is the light the surface gives off on its own — the glow
        // that survives a dark scene. The texture, when there is one,
        // multiplies the factor.
        let emissive_texture = material.emissive_texture();
        let emissive_slot = emissive_texture
            .as_ref()
            .and_then(|info| slot_of_texture.get(info.texture().index()).copied())
            .unwrap_or(NO_TEXTURE);
        // KHR_materials_emissive_strength scales the factor; without the
        // extension the spec's default multiplier is 1.
        let emissive_strength = material.emissive_strength().unwrap_or(1.0);
        let emissive_factor = material
            .emissive_factor()
            .map(|channel| channel * emissive_strength);
        // The material's alpha, folded to the two numbers the renderers pack
        // per vertex: the cutoff a texel's alpha is tested against (negative
        // = never discard) and the base-colour factor's alpha that multiplies
        // the texel into the value tested. A blended material needs per-pixel
        // blending in draw order, which an off-screen preview has no sort
        // for — clipping it at the default cutoff keeps the cutout silhouette
        // a leaf card is meant to have instead of a sheet of black.
        let alpha_cutoff = match material.alpha_mode() {
            gltf::material::AlphaMode::Mask => material.alpha_cutoff().unwrap_or(0.5),
            gltf::material::AlphaMode::Blend => 0.5,
            gltf::material::AlphaMode::Opaque => -1.0,
        };
        let alpha_factor = factor[3];
        // The material's doubleSided: its back faces are the surface seen
        // from behind, and no renderer may cull them — the flag rides the
        // per-vertex table so the decision lands on the face that owns it.
        let double_sided = material.double_sided();
        // KHR_texture_transform: the affine map one texture's uv go through
        // (scale, then a counter-clockwise rotation, then the offset). The
        // preview carries ONE uv per vertex, shared by every map the
        // primitive samples, so the base-colour texture's transform is the
        // one applied — exporters write the same transform on every map of a
        // material, and where a file genuinely disagrees, the base colour is
        // the one that owns the picture. A transform naming a different
        // TEXCOORD set is ignored: only set 0 is read.
        let uv_transform = base_texture
            .as_ref()
            .and_then(|info| info.texture_transform())
            .or_else(|| {
                material
                    .pbr_metallic_roughness()
                    .metallic_roughness_texture()
                    .as_ref()
                    .and_then(|info| info.texture_transform())
            });
        match reader.read_tex_coords(0) {
            Some(uv) => {
                for uv in uv.into_f32() {
                    self.uv.push(transform_uv(uv, &uv_transform));
                    self.slots.push(slot);
                    self.mr_slots.push(mr_slot);
                    self.factors.push(factors);
                    self.normal_slot.push(normal_slot);
                    self.normal_scale.push(normal_scale);
                    self.ao_slot.push(ao_slot);
                    self.ao_strength.push(ao_strength);
                    self.emissive_slot.push(emissive_slot);
                    self.emissive_factors.push(emissive_factor);
                    self.alpha_cutoff.push(alpha_cutoff);
                    self.alpha_factor.push(alpha_factor);
                    self.double_sided.push(double_sided);
                }
            }
            None => {
                self.uv.extend(std::iter::repeat_n([0.0, 0.0], count));
                self.slots.extend(std::iter::repeat_n(slot, count));
                self.mr_slots.extend(std::iter::repeat_n(mr_slot, count));
                self.factors.extend(std::iter::repeat_n(factors, count));
                self.normal_slot
                    .extend(std::iter::repeat_n(normal_slot, count));
                self.normal_scale
                    .extend(std::iter::repeat_n(normal_scale, count));
                self.ao_slot.extend(std::iter::repeat_n(ao_slot, count));
                self.ao_strength
                    .extend(std::iter::repeat_n(ao_strength, count));
                self.emissive_slot
                    .extend(std::iter::repeat_n(emissive_slot, count));
                self.emissive_factors
                    .extend(std::iter::repeat_n(emissive_factor, count));
                self.alpha_cutoff
                    .extend(std::iter::repeat_n(alpha_cutoff, count));
                self.alpha_factor
                    .extend(std::iter::repeat_n(alpha_factor, count));
                self.double_sided
                    .extend(std::iter::repeat_n(double_sided, count));
            }
        }

        let indices: Vec<u32> = match reader.read_indices() {
            Some(indices) => indices.into_u32().collect(),
            None => (0..(self.positions.len() - base as usize) as u32).collect(),
        };
        for triangle in triangles_of(primitive.mode(), &indices) {
            self.triangles
                .push([base + triangle[0], base + triangle[1], base + triangle[2]]);
        }
    }

    fn finish(self) -> Result<Mesh, String> {
        if !self.triangles.is_empty() {
            let normals = if !self.normals_incomplete && self.normals.len() == self.positions.len()
            {
                self.normals
            } else {
                Vec::new()
            };
            let colors = if self.material_colored && self.colors.len() == self.positions.len() {
                self.colors
            } else {
                Vec::new()
            };
            let mut mesh = Mesh::finish(self.positions, normals, colors, self.triangles)
                .ok_or_else(|| "the glTF file contains no drawable geometry".to_string())?;
            if !self.textures.is_empty() && self.uv.len() == mesh.positions.len() {
                mesh.texture = Some(Box::new(TextureData {
                    uv: self.uv,
                    slot: self.slots,
                    mr_slot: self.mr_slots,
                    factors: self.factors,
                    normal_slot: self.normal_slot,
                    normal_scale: self.normal_scale,
                    ao_slot: self.ao_slot,
                    ao_strength: self.ao_strength,
                    emissive_slot: self.emissive_slot,
                    emissive_factor: self.emissive_factors,
                    alpha_cutoff: self.alpha_cutoff,
                    alpha_factor: self.alpha_factor,
                    double_sided: self.double_sided,
                    maps: self.textures,
                }));
            }
            return Ok(mesh);
        }
        // No triangles: a `POINTS` document is a cloud, and the renderers
        // already know how to draw one.
        Mesh::finish_points(self.points, Vec::new(), Vec::new())
            .ok_or_else(|| "the glTF file contains no drawable geometry".to_string())
    }
}

/// The triangles a primitive's index list describes, under its draw mode.
///
/// A mode that is not a triangle kind — lines, or points — contributes none.
fn triangles_of(mode: gltf::mesh::Mode, indices: &[u32]) -> Vec<[u32; 3]> {
    let mut triangles = Vec::new();
    match mode {
        gltf::mesh::Mode::Triangles => {
            for triangle in indices.as_chunks::<3>().0 {
                triangles.push(*triangle);
            }
        }
        gltf::mesh::Mode::TriangleStrip => {
            // Every window of three, with the winding alternated so the
            // strip's back faces do not flip on every other triangle.
            for (i, window) in indices.windows(3).enumerate() {
                triangles.push(if i % 2 == 0 {
                    [window[0], window[1], window[2]]
                } else {
                    [window[1], window[0], window[2]]
                });
            }
        }
        gltf::mesh::Mode::TriangleFan => {
            // Every triangle shares the fan's first vertex; a sliding window
            // would produce a strip instead, with the wrong triangles.
            for i in 1..indices.len().saturating_sub(1) {
                triangles.push([indices[0], indices[i], indices[i + 1]]);
            }
        }
        gltf::mesh::Mode::Points
        | gltf::mesh::Mode::Lines
        | gltf::mesh::Mode::LineLoop
        | gltf::mesh::Mode::LineStrip => {}
    }
    triangles
}

/// The longest edge a decoded texture may keep: a preview does not need the
/// 4K original, and the CPU renderer holds the decoded pixels in memory.
const TEXTURE_MAX_EDGE: u32 = 1024;

/// The document image a texture names: the standard `source` field, falling
/// back to `EXT_texture_webp`, whose source lives in the extension block the
/// gltf crate lists but does not resolve. Sketchfab exports lean on it.
fn texture_source_index(texture: &gltf::Texture<'_>) -> Option<usize> {
    texture.source().map(|image| image.index()).or_else(|| {
        let value = texture
            .extensions()?
            .get("EXT_texture_webp")?
            .get("source")?;
        value.as_u64().map(|index| index as usize)
    })
}

/// Cap a decoded image at the texture edge limit: a preview does not need
/// the 4K original, and the CPU renderer holds the decoded pixels in memory.
fn finish_texture(rgba: Vec<u8>, width: u32, height: u32) -> TextureMap {
    let longest = width.max(height);
    if longest <= TEXTURE_MAX_EDGE {
        return TextureMap {
            rgba,
            width,
            height,
        };
    }
    let scale = TEXTURE_MAX_EDGE as f32 / longest as f32;
    let tw = ((width as f32 * scale).round() as u32).max(1);
    let th = ((height as f32 * scale).round() as u32).max(1);
    TextureMap {
        rgba: resize_rgba(&rgba, width, height, tw, th),
        width: tw,
        height: th,
    }
}

/// Apply a `KHR_texture_transform` to one uv, the spec's affine map: scale,
/// then a counter-clockwise rotation, then the offset. `None` leaves the uv
/// untouched.
fn transform_uv(uv: [f32; 2], transform: &Option<gltf::texture::TextureTransform<'_>>) -> [f32; 2] {
    let Some(transform) = transform else {
        return uv;
    };
    let scale = transform.scale();
    let rotation = transform.rotation();
    let offset = transform.offset();
    let (sin, cos) = rotation.sin_cos();
    let (u, v) = (uv[0] * scale[0], uv[1] * scale[1]);
    [u * cos - v * sin + offset[0], u * sin + v * cos + offset[1]]
}

/// Apply a composed node transform/// Apply a composed node transform to a point. The matrix is glTF's own
/// column-major layout: `m[column][row]`, translation in the last column.
fn transform_point(m: [[f32; 4]; 4], point: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * point[0] + m[1][0] * point[1] + m[2][0] * point[2] + m[3][0],
        m[0][1] * point[0] + m[1][1] * point[1] + m[2][1] * point[2] + m[3][1],
        m[0][2] * point[0] + m[1][2] * point[1] + m[2][2] * point[2] + m[3][2],
    ]
}

/// Apply a composed node transform to a normal: the inverse transpose of the
/// rotation-and-scale part, which is what keeps a normal perpendicular under a
/// non-uniform scale. The length is not restored here: both renderers
/// normalise. A degenerate scale has no inverse — the raw matrix is applied
/// instead, and the normalised result is whatever it comes out as.
fn transform_normal(m: [[f32; 4]; 4], normal: [f32; 3]) -> [f32; 3] {
    // The three columns of the rotation-and-scale block.
    let c0 = [m[0][0], m[0][1], m[0][2]];
    let c1 = [m[1][0], m[1][1], m[1][2]];
    let c2 = [m[2][0], m[2][1], m[2][2]];
    let det = dot(c0, cross(c1, c2));
    if det.abs() < 1e-12 {
        return [
            m[0][0] * normal[0] + m[1][0] * normal[1] + m[2][0] * normal[2],
            m[0][1] * normal[0] + m[1][1] * normal[1] + m[2][1] * normal[2],
            m[0][2] * normal[0] + m[1][2] * normal[1] + m[2][2] * normal[2],
        ];
    }
    // The inverse's columns are the cross products of the other two, scaled
    // by one determinant; the transpose then mixes equal components across
    // them.
    let one = 1.0 / det;
    let i0 = scale3(cross(c1, c2), one);
    let i1 = scale3(cross(c2, c0), one);
    let i2 = scale3(cross(c0, c1), one);
    [
        i0[0] * normal[0] + i1[0] * normal[1] + i2[0] * normal[2],
        i0[1] * normal[0] + i1[1] * normal[1] + i2[1] * normal[2],
        i0[2] * normal[0] + i1[2] * normal[1] + i2[2] * normal[2],
    ]
}

/// Compose two column-major transforms: `mul4(parent, local)` places a node
/// inside its parent, the parent's frame applied after the node's own.
fn mul4(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            out[column][row] = (0..4).map(|k| a[k][row] * b[column][k]).sum();
        }
    }
    out
}

/// The walk's starting frame: the first node's transform composes against it.
pub(crate) const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

fn scale3(v: [f32; 3], k: f32) -> [f32; 3] {
    [v[0] * k, v[1] * k, v[2] * k]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal valid GLB (binary glTF) wrapping the given JSON,
    /// against a binary chunk of one triangle.
    fn glb(json: String) -> std::path::PathBuf {
        let bin: Vec<u8> = {
            let mut v = Vec::new();
            // 3 vertices: (0,0,0), (1,0,0), (0,1,0)
            for &[x, y, z] in &[[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.]] {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
                v.extend_from_slice(&z.to_le_bytes());
            }
            // 3 u32 indices: 0, 1, 2
            for idx in [0u32, 1, 2] {
                v.extend_from_slice(&idx.to_le_bytes());
            }
            v
        };
        glb_bin(json, bin)
    }

    /// [`glb`], with the binary chunk spelled out — tests that pack an image
    /// in beside the geometry build their own layout.
    fn glb_bin(json: String, bin: Vec<u8>) -> std::path::PathBuf {
        let json_bytes = json.into_bytes();
        let json_pad = (4 - (json_bytes.len() % 4)) % 4;
        let bin_pad = (4 - (bin.len() % 4)) % 4;

        let total = 12 + 8 + json_bytes.len() + json_pad + 8 + bin.len() + bin_pad;

        let path =
            std::env::temp_dir().join(format!("trove-gltf-test-{}.glb", uuid::Uuid::new_v4()));
        let mut f = std::fs::File::create(&path).unwrap();

        // GLB header
        f.write_all(b"glTF").unwrap(); // magic
        f.write_all(&2u32.to_le_bytes()).unwrap(); // version
        f.write_all(&(total as u32).to_le_bytes()).unwrap(); // total length

        // JSON chunk
        f.write_all(&((json_bytes.len() + json_pad) as u32).to_le_bytes())
            .unwrap();
        f.write_all(b"JSON").unwrap();
        f.write_all(&json_bytes).unwrap();
        f.write_all(&vec![b' '; json_pad]).unwrap();

        // BIN chunk
        f.write_all(&((bin.len() + bin_pad) as u32).to_le_bytes())
            .unwrap();
        f.write_all(b"BIN\x00").unwrap();
        f.write_all(&bin).unwrap();
        f.write_all(&vec![0u8; bin_pad]).unwrap();

        path
    }

    /// Build a minimal valid GLB containing one triangle, with `node` spliced
    /// into the JSON for the scene's single node.
    fn triangle_glb(node: &str) -> std::path::PathBuf {
        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [{}],
            "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}, "indices": 1}}]}}],
            "buffers": [{{"byteLength": {}}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#,
            node, 48
        );
        glb(json)
    }

    #[test]
    fn triangle_parses_from_glb() {
        let path = triangle_glb(r#"{"mesh": 0}"#);
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        assert_eq!(mesh.vertex_count(), 3);
        assert_eq!(mesh.triangle_count(), 1);
        assert_eq!(mesh.triangles[0], [0, 1, 2]);
        assert_eq!(mesh.bounds.max, [1.0, 1.0, 0.0]);
    }

    /// The primitive's base colour factor is what its vertices are painted
    /// with — the one material property this flat-colour renderer can show.
    #[test]
    fn a_materials_base_colour_becomes_vertex_colours() {
        let json =
            r#"{
            "asset": {"version": "2.0"},
            "scenes": [{"nodes": [0]}],
            "scene": 0,
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}, "indices": 1, "material": 0}]}],
            "materials": [{"pbrMetallicRoughness": {"baseColorFactor": [1.0, 0.5, 0.0, 1.0]}}],
            "buffers": [{"byteLength": 48}],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962},
                {"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]},
                {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}
            ]
        }"#.to_string();
        let path = glb(json);
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        assert!(
            mesh.has_vertex_colors(),
            "the material colour comes through"
        );
        assert_eq!(mesh.colors[0], [1.0, 0.5, 0.0]);
        assert_eq!(mesh.colors[2], [1.0, 0.5, 0.0]);
    }

    /// A mesh mixing a real material with the spec-default one keeps all of
    /// its colours: the default factor is *white*, a material — not a
    /// missing one. The old behaviour dropped the whole set when one
    /// primitive wore the default, which is how a black lamp with a white
    /// bulb rendered as bare clay.
    #[test]
    fn a_default_material_keeps_the_meshs_colours() {
        let json = r#"{
            "asset": {"version": "2.0"},
            "scenes": [{"nodes": [0]}],
            "scene": 0,
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [
                {"attributes": {"POSITION": 0}, "indices": 1, "material": 0},
                {"attributes": {"POSITION": 0}, "indices": 1}
            ]}],
            "materials": [{"pbrMetallicRoughness": {"baseColorFactor": [0.01, 0.01, 0.01, 1.0]}}],
            "buffers": [{"byteLength": 48}],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962},
                {"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]},
                {"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}
            ]
        }"#.to_string();
        let path = glb(json);
        let mesh = load_gltf(&path).expect("glb two-primitive mesh parses");
        std::fs::remove_file(&path).ok();
        assert_eq!(mesh.vertex_count(), 6);
        assert!(
            mesh.has_vertex_colors(),
            "one default-material part must not erase the others' colours"
        );
        // The black part first, the default-white part second.
        assert_eq!(&mesh.colors[0..3], &[[0.01, 0.01, 0.01]; 3]);
        assert_eq!(&mesh.colors[3..6], &[[1.0, 1.0, 1.0]; 3]);
    }

    /// A material's normal and occlusion textures land in the per-vertex slot
    /// table with their strengths, and a material without them carries the
    /// no-texture marker there: the renderers shade relief and shadowed
    /// creases from these, and a slot that missed its image — or a strength
    /// stuck at its default — would show as a flat, uniformly lit surface.
    #[test]
    fn a_materials_normal_and_occlusion_maps_reach_the_slot_table() {
        // A 2×2 opaque PNG rides the binary chunk beside the triangle's
        // geometry: the smallest image the decoder accepts.
        let png = {
            let mut img = image::RgbaImage::new(2, 2);
            for p in img.pixels_mut() {
                *p = image::Rgba([128, 128, 255, 255]);
            }
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let bin = {
            let mut v = Vec::new();
            for &[x, y, z] in &[[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.]] {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
                v.extend_from_slice(&z.to_le_bytes());
            }
            for idx in [0u32, 1, 2] {
                v.extend_from_slice(&idx.to_le_bytes());
            }
            // 4-aligned by construction: 36 bytes of positions, 12 of indices.
            v.extend_from_slice(&png);
            v
        };
        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [{{"mesh": 0}}],
            "meshes": [{{"primitives": [
                {{"attributes": {{"POSITION": 0}}, "indices": 1, "material": 0}},
                {{"attributes": {{"POSITION": 0}}, "indices": 1, "material": 1}}
            ]}}],
            "materials": [
                {{"normalTexture": {{"index": 0, "scale": 2.0}}, "occlusionTexture": {{"index": 0, "strength": 0.5}}}},
                {{"pbrMetallicRoughness": {{"baseColorFactor": [1.0, 1.0, 1.0, 1.0]}}}}
            ],
            "textures": [{{"source": 0}}],
            "images": [{{"bufferView": 2, "mimeType": "image/png"}}],
            "buffers": [{{"byteLength": {}}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}},
                {{"buffer": 0, "byteOffset": 48, "byteLength": {}}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#,
            bin.len(),
            png.len()
        );
        let path = glb_bin(json, bin);
        let mesh = load_gltf(&path).expect("glb with maps parses");
        std::fs::remove_file(&path).ok();
        let texture = mesh.texture.as_ref().expect("the file carries an image");
        assert_eq!(texture.maps.len(), 1);
        // The mapped primitive first, the bare one second.
        assert_eq!(texture.normal_slot[0..3], [0, 0, 0]);
        assert_eq!(texture.normal_scale[0..3], [2.0; 3]);
        assert_eq!(texture.ao_slot[0..3], [0, 0, 0]);
        assert_eq!(texture.ao_strength[0..3], [0.5; 3]);
        assert_eq!(
            texture.normal_slot[3..6],
            [NO_TEXTURE; 3],
            "the unmapped material carries the no-texture marker"
        );
        assert_eq!(texture.ao_slot[3..6], [NO_TEXTURE; 3]);
    }

    /// `KHR_texture_transform` folds the base-colour texture's affine map —
    /// scale, then the counter-clockwise rotation, then the offset — into
    /// the per-vertex uv at load, so both renderers sample the atlas region
    /// the material points at.
    #[test]
    fn a_texture_transform_reaches_the_vertex_uvs() {
        // A 2×2 opaque PNG rides the binary chunk so the document declares
        // an image and the slot table attaches.
        let png = {
            let mut img = image::RgbaImage::new(2, 2);
            for p in img.pixels_mut() {
                *p = image::Rgba([128, 128, 255, 255]);
            }
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let bin = {
            let mut v = Vec::new();
            for &[x, y, z] in &[[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.]] {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
                v.extend_from_slice(&z.to_le_bytes());
            }
            for idx in [0u32, 1, 2] {
                v.extend_from_slice(&idx.to_le_bytes());
            }
            v.extend_from_slice(&png);
            // The uv set the transform runs on: three vec2s behind the image.
            for uv in &[[0.0f32, 0.0], [0.5, 0.0], [0.0, 0.5]] {
                v.extend_from_slice(&uv[0].to_le_bytes());
                v.extend_from_slice(&uv[1].to_le_bytes());
            }
            v
        };
        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "extensionsUsed": ["KHR_texture_transform"],
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [{{"mesh": 0}}],
            "meshes": [{{"primitives": [
                {{"attributes": {{"POSITION": 0, "TEXCOORD_0": 2}}, "indices": 1, "material": 0}}
            ]}}],
            "materials": [{{
                "pbrMetallicRoughness": {{"baseColorTexture": {{"index": 0, "extensions": {{
                    "KHR_texture_transform": {{"offset": [0.25, 0.0], "scale": [2.0, 2.0]}}
                }}}}}}
            }}],
            "textures": [{{"source": 0}}],
            "images": [{{"bufferView": 2, "mimeType": "image/png"}}],
            "buffers": [{{"byteLength": {}}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}},
                {{"buffer": 0, "byteOffset": 48, "byteLength": {}}},
                {{"buffer": 0, "byteOffset": {}, "byteLength": 24, "target": 34962}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}},
                {{"bufferView": 3, "componentType": 5126, "count": 3, "type": "VEC2", "max": [0.5,0.5], "min": [0,0]}}
            ]
        }}"#,
            bin.len(),
            png.len(),
            48 + png.len()
        );
        let path = glb_bin(json, bin);
        let mesh = load_gltf(&path).expect("glb with a transformed texture parses");
        std::fs::remove_file(&path).ok();
        let texture = mesh.texture.as_ref().expect("the file carries an image");
        // scale 2, then offset 0.25: uv (0.5, 0) lands at (1.25, 0), and the
        // untouched (0, 0) takes only the offset.
        assert_eq!(texture.uv[0], [0.25, 0.0]);
        assert_eq!(texture.uv[1], [1.25, 0.0]);
    }

    /// A primitive with neither `COLOR_0` nor a non-default base colour
    /// factor has no material to preview, so the mesh keeps the flat default
    /// it always had.
    #[test]
    fn a_gltf_without_materials_keeps_no_colours() {
        let path = triangle_glb(r#"{"mesh": 0}"#);
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        assert!(!mesh.has_vertex_colors());
        assert!(mesh.colors.is_empty());
    }

    /// The material's alpha test and its emissive strength reach the
    /// per-vertex slot table: a masked material carries its cutoff (and the
    /// base-colour factor's alpha), an opaque one carries the negative
    /// "never discard" marker, and `KHR_materials_emissive_strength`
    /// multiplies the factor at load — the two numbers the renderers clip
    /// and light by.
    #[test]
    fn a_materials_alpha_and_emissive_strength_reach_the_slot_table() {
        // A 2×2 opaque PNG rides the binary chunk beside the triangles, so
        // the document declares an image and the slot table attaches.
        let png = {
            let mut img = image::RgbaImage::new(2, 2);
            for p in img.pixels_mut() {
                *p = image::Rgba([128, 128, 255, 255]);
            }
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let bin = {
            let mut v = Vec::new();
            for &[x, y, z] in &[[0f32, 0., 0.], [1., 0., 0.], [0., 1., 0.]] {
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
                v.extend_from_slice(&z.to_le_bytes());
            }
            for idx in [0u32, 1, 2] {
                v.extend_from_slice(&idx.to_le_bytes());
            }
            v.extend_from_slice(&png);
            v
        };
        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [{{"mesh": 0}}],
            "meshes": [{{"primitives": [
                {{"attributes": {{"POSITION": 0}}, "indices": 1, "material": 0}},
                {{"attributes": {{"POSITION": 0}}, "indices": 1, "material": 1}}
            ]}}],
            "materials": [
                {{
                    "pbrMetallicRoughness": {{"baseColorTexture": {{"index": 0}}, "baseColorFactor": [1.0, 0.5, 0.0, 0.8]}},
                    "alphaMode": "MASK",
                    "alphaCutoff": 0.25,
                    "emissiveFactor": [0.5, 0.5, 0.5],
                    "extensions": {{"KHR_materials_emissive_strength": {{"emissiveStrength": 3.0}}}}
                }},
                {{}}
            ],
            "textures": [{{"source": 0}}],
            "images": [{{"bufferView": 2, "mimeType": "image/png"}}],
            "buffers": [{{"byteLength": {}}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}},
                {{"buffer": 0, "byteOffset": 48, "byteLength": {}}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#,
            bin.len(),
            png.len()
        );
        let path = glb_bin(json, bin);
        let mesh = load_gltf(&path).expect("glb with masked material parses");
        std::fs::remove_file(&path).ok();
        let texture = mesh.texture.as_ref().expect("the file carries an image");
        // The masked primitive first: its cutoff and alpha factor, and the
        // emissive factor times the extension's strength.
        assert_eq!(texture.alpha_cutoff[0..3], [0.25; 3]);
        assert_eq!(texture.alpha_factor[0..3], [0.8; 3]);
        assert_eq!(texture.emissive_factor[0..3], [[1.5; 3]; 3]);
        // The bare material second: opaque, so nothing is ever discarded.
        assert_eq!(texture.alpha_cutoff[3..6], [-1.0; 3]);
        assert_eq!(texture.alpha_factor[3..6], [1.0; 3]);
    }

    /// A node's placement is its own transform on top of every ancestor's.
    /// The walk used to apply only the local transform, which dropped a part
    /// nested under a rotated or scaled root out of the scene entirely — the
    /// shape every Sketchfab export has.
    #[test]
    fn an_ancestors_transform_is_applied_to_a_nested_mesh() {
        let json = r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [
                {{"children": [1], "scale": [2.0, 2.0, 2.0], "translation": [10.0, 0.0, 0.0]}},
                {{"mesh": 0, "translation": [0.0, 1.0, 0.0]}}
            ],
            "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}, "indices": 1}}]}}],
            "buffers": [{{"byteLength": 48}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#;
        let path = glb(json.replace("{{", "{").replace("}}", "}"));
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        // The child moves its triangle to (0,1,0); the parent scales that by
        // two and moves it to x=10: (10,2,0)..(12,4,0). Applied in the other
        // order, or not at all, the bounds say so.
        assert_eq!(mesh.bounds.min, [10.0, 2.0, 0.0]);
        assert_eq!(mesh.bounds.max, [12.0, 4.0, 0.0]);
    }

    /// The node's transform is what places the geometry. Ignoring it — the
    /// old behaviour, which read `document.meshes()` directly — puts every
    /// scaled, rotated or moved model in the wrong place at the wrong size.
    #[test]
    fn a_node_transform_is_applied() {
        let path = triangle_glb(
            r#"{"mesh": 0, "translation": [10.0, 0.0, -2.0], "scale": [2.0, 2.0, 2.0]}"#,
        );
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        // The triangle is (0,0,0), (1,0,0), (0,1,0). Scaled by two it spans
        // x and y over 0..2, then the translation moves it to x 10..12,
        // y 0..2, z = -2.
        assert_eq!(mesh.bounds.min, [10.0, 0.0, -2.0]);
        assert_eq!(mesh.bounds.max, [12.0, 2.0, -2.0]);
    }

    /// A quarter turn about Z maps (1,0,0) to (0,1,0); the height of the
    /// triangle follows it.
    #[test]
    fn a_node_rotation_is_applied() {
        // sin(45°) = cos(45°) = √2/2 gives a 90° rotation about Z.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let path = triangle_glb(&format!(r#"{{"mesh": 0, "rotation": [0, 0, {s}, {s}]}}"#));
        let mesh = load_gltf(&path).expect("glb triangle parses");
        std::fs::remove_file(&path).ok();
        // (0,0,0) stays; (1,0,0) becomes (0,1,0); (0,1,0) becomes (-1,0,0).
        let expected = [[0.0f32, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
        assert_eq!(mesh.positions.len(), expected.len());
        for want in expected {
            assert!(
                mesh.positions
                    .iter()
                    .any(|got| { (0..3).all(|axis| (got[axis] - want[axis]).abs() < 1e-4) }),
                "no vertex near {want:?} in {:?}",
                mesh.positions
            );
        }
    }

    /// A strip alternates winding; reading it as a triangle list instead
    /// yields half the triangles and the wrong ones.
    #[test]
    fn triangle_strips_and_fans_are_converted() {
        let strip = triangles_of(gltf::mesh::Mode::TriangleStrip, &[0, 1, 2, 3]);
        assert_eq!(strip, vec![[0, 1, 2], [2, 1, 3]]);
        let fan = triangles_of(gltf::mesh::Mode::TriangleFan, &[0, 1, 2, 3]);
        assert_eq!(fan, vec![[0, 1, 2], [0, 2, 3]]);
        // A list is taken three at a time, and a run that does not divide by
        // three is not a triangle.
        assert_eq!(
            triangles_of(gltf::mesh::Mode::Triangles, &[0, 1, 2, 3, 4, 5, 6]),
            vec![[0, 1, 2], [3, 4, 5]]
        );
        // Lines and points are not surfaces.
        assert!(triangles_of(gltf::mesh::Mode::Points, &[0, 1, 2]).is_empty());
        assert!(triangles_of(gltf::mesh::Mode::LineStrip, &[0, 1, 2]).is_empty());
    }
}
