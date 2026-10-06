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

use super::types::{MaterialSlot, Mesh, NO_TEXTURE, TextureData, TextureMap, resize_rgba};

/// File size above which a glTF/GLB parse maps the file instead of reading it.
///
/// Below this a plain read is cheaper than a mapping's syscall and page
/// faults; above it, mapping avoids a full second copy of the file beside the
/// `gltf` crate's own copy of the binary chunk.
const GLTF_MMAP_THRESHOLD: u64 = 8 << 20;

/// Load a glTF or GLB file. Buffers are resolved relative to the file.
///
/// Images are decoded here, and the parse never asks the gltf crate for them:
/// its own import decodes every texture with the codecs it ships with — png
/// and jpeg only — only for this module to decode them again from the raw
/// buffer views, so the crate's pass is a second full decode of every image
/// in the file, and a Sketchfab model's webp textures would fail the whole
/// import and leave the model flat.
pub fn load_gltf(path: &Path) -> Result<Mesh, String> {
    // A GLB carries its binary chunk inline, and the `gltf` crate copies that
    // chunk into an owned buffer itself — so reading the whole file into a
    // `Vec` here would put a second full copy of it in memory at peak. Above
    // the threshold the file is mapped and the parser borrows it; below it a
    // plain read is cheaper than the mapping. The map is read-only and lives
    // only until the parse returns.
    let file = std::fs::File::open(path).map_err(|e| format!("failed to load glTF: {e}"))?;
    let len = file
        .metadata()
        .map_err(|e| format!("failed to load glTF: {e}"))?
        .len();
    let mapped;
    let read;
    let bytes: &[u8] = if len >= GLTF_MMAP_THRESHOLD {
        // SAFETY: a read-only mapping of a file this process just opened. The
        // parser borrows it for the length of the call and copies anything it
        // keeps; a file truncated underneath is a read the parser reports as
        // an error, the same contract `chunked.rs` takes for its maps.
        mapped = unsafe { memmap2::MmapOptions::new().map(&file) }
            .map_err(|e| format!("failed to load glTF: {e}"))?;
        &mapped[..]
    } else {
        read = std::fs::read(path).map_err(|e| format!("failed to load glTF: {e}"))?;
        &read[..]
    };
    // The document is validated when it can be. A file whose
    // `extensionsRequired` names an extension this build of the crate was not
    // compiled with is refused on sight — the refusal covers the whole file,
    // geometry included — but the geometry is still perfectly readable, so
    // the parse is retried without validation: the extension's own material
    // block is read where the crate knows it (the feature is enabled), and
    // ignored where it does not. A file that fails this way too was never
    // going to open, and the validation error — which names the extension —
    // is the more useful one to report.
    let parsed = match gltf::Gltf::from_slice(bytes) {
        Ok(gltf) => gltf,
        Err(original) => {
            let gltf = gltf::Gltf::from_slice_without_validation(bytes)
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
    // Only the images a texture names are decoded, and each success gets a
    // slot. The document image index → slot map comes back alongside, so a
    // texture resolves to its own image rather than to a positional guess:
    // a compacted table indexed by the raw image index would mis-assign every
    // texture after the first decode failure.
    let (maps, slot_of_image) = decode_images(&document, &buffers, path);
    let slot_of_texture: Vec<u16> = document
        .textures()
        .map(|texture| {
            texture_source_index(&texture)
                .and_then(|index| slot_of_image.get(index).copied())
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

/// Decode the images the document's textures reference into texture slots,
/// and return them with a map from document image index to slot.
///
/// An image no texture points at is never decoded. A decode failure costs that
/// image its slot, never the model — and, because the slot table is keyed by
/// image index rather than by position in the result, never the slot of any
/// other image. The decodes run across the rayon pool: a file of 4K PNGs
/// spends most of its parse inside the decoders, and they are embarrassingly
/// parallel.
fn decode_images(
    document: &gltf::Document,
    buffers: &[gltf::buffer::Data],
    path: &Path,
) -> (Vec<TextureMap>, Vec<u16>) {
    use rayon::prelude::*;

    let images: Vec<gltf::Image<'_>> = document.images().collect();
    // The distinct image indices any texture names, first reference first.
    let mut referenced: Vec<usize> = Vec::new();
    let mut seen = vec![false; images.len()];
    for index in document
        .textures()
        .filter_map(|texture| texture_source_index(&texture))
    {
        if let Some(seen) = seen.get_mut(index)
            && !*seen
        {
            *seen = true;
            referenced.push(index);
        }
    }
    // Decode them, keeping each image index so a slot is only assigned to the
    // ones that actually decoded.
    let decoded: Vec<(usize, Option<TextureMap>)> = referenced
        .par_iter()
        .map(|&index| (index, decode_image(&images[index], buffers, path)))
        .collect();
    let mut maps = Vec::with_capacity(decoded.len());
    let mut slot_of_image = vec![NO_TEXTURE; images.len()];
    for (index, map) in decoded {
        let Some(map) = map else { continue };
        if maps.len() >= NO_TEXTURE as usize {
            break; // more images than a `u16` slot can name
        }
        slot_of_image[index] = maps.len() as u16;
        maps.push(map);
    }
    (maps, slot_of_image)
}

/// Decode one image, borrowing an embedded image straight out of its buffer
/// rather than copying it first — the decoder needs the bytes only for the
/// length of the call, and a GLB's images are the bulk of what it carries.
/// A `uri` is resolved (an inlined `data:` URI, or a file beside the model)
/// into an owned buffer.
fn decode_image(
    image: &gltf::Image<'_>,
    buffers: &[gltf::buffer::Data],
    path: &Path,
) -> Option<TextureMap> {
    let bytes: std::borrow::Cow<'_, [u8]> = match image.source() {
        gltf::image::Source::View { view, .. } => {
            let buffer = buffers.get(view.buffer().index())?;
            let start = view.offset();
            std::borrow::Cow::Borrowed(&buffer[start..start + view.length()])
        }
        gltf::image::Source::Uri { uri, .. } => std::borrow::Cow::Owned(uri_bytes(uri, path)?),
    };
    let decoded = image::load_from_memory(&bytes).ok()?;
    let (width, height) = decoded.dimensions();
    let rgba = decoded.to_rgba8().into_raw();
    Some(finish_texture(rgba, width, height))
}

/// The bytes an image `uri` names: an inlined `data:` URI decoded, or a file
/// read from beside the model.
///
/// glTF spells URIs the URI way, and the spec calls a `data:` URI out
/// explicitly — which Sketchfab-style `.gltf` exports lean on for their
/// textures. Read as a file path, a `data:` URI is just a name no directory
/// has, and the image vanishes. A relative path, conversely, is
/// percent-decoded first: a file named `my texture.png` arrives as
/// `my%20texture.png`, and reading it literally finds nothing.
fn uri_bytes(uri: &str, path: &Path) -> Option<Vec<u8>> {
    if let Some(data) = uri.strip_prefix("data:") {
        return data_uri_bytes(data);
    }
    std::fs::read(path.parent()?.join(percent_decode(uri))).ok()
}

/// The payload of a `data:` URI — everything past the leading `data:`:
/// `<mediatype>[;base64],<data>`. The image decoders sniff the format from the
/// bytes, so the media type is dropped and only the base64 flag matters.
fn data_uri_bytes(data: &str) -> Option<Vec<u8>> {
    let (meta, payload) = data.split_once(',')?;
    if meta.rsplit(';').next() == Some("base64") {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()
    } else {
        // Not base64: RFC 2397's other form spends percent-escapes on the
        // bytes. Images are rarely spelled this way, but it costs a branch.
        Some(percent_decode_bytes(payload))
    }
}

/// Decode `%XX` escapes into the bytes they name.
fn percent_decode_bytes(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// [`percent_decode_bytes`] as a string, for a path. A malformed escape — or a
/// name that was never escaped — passes through as it stands.
fn percent_decode(text: &str) -> String {
    String::from_utf8_lossy(&percent_decode_bytes(text)).into_owned()
}

/// The value of one hex digit, upper or lower case.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
    /// Per-vertex index into `materials`, parallel to `positions`. Every
    /// vertex of a primitive names the same material — the primitive's — so
    /// the material's constants are stored once, not once per vertex.
    material_of_vertex: Vec<u16>,
    /// The materials the primitives carry, in first-use order.
    materials: Vec<MaterialSlot>,
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
        // The normal transform is constant for the whole primitive, so it is
        // derived once here rather than inverted again inside the per-vertex
        // map (which used to compute the same determinant and three cross
        // products for every normal).
        let normal_cols = normal_matrix(transform);
        let normals: Option<Vec<[f32; 3]>> = reader.read_normals().map(|normals| {
            normals
                .map(|normal| apply_normal(normal_cols, normal))
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

        // The material constants — the slots this primitive samples, the
        // factors, the alpha and two-sidedness — are the same for every vertex
        // of the primitive, so they are computed once here and stored once;
        // the vertices below carry only an index into the table.
        let normal_texture = material.normal_texture();
        let ao_texture = material.occlusion_texture();
        let emissive_texture = material.emissive_texture();
        // KHR_materials_emissive_strength scales the emissive factor; without
        // the extension the spec's default multiplier is 1.
        let emissive_strength = material.emissive_strength().unwrap_or(1.0);
        let material_slot = MaterialSlot {
            // The base-colour texture; a primitive with none carries the
            // no-texture marker.
            slot: base_texture
                .as_ref()
                .map(|info| {
                    slot_of_texture
                        .get(info.texture().index())
                        .copied()
                        .unwrap_or(NO_TEXTURE)
                })
                .unwrap_or(NO_TEXTURE),
            // The metallic-roughness texture: its G channel carries roughness
            // and its B channel metallic, both scaled by the factors.
            mr_slot: material
                .pbr_metallic_roughness()
                .metallic_roughness_texture()
                .map(|info| {
                    slot_of_texture
                        .get(info.texture().index())
                        .copied()
                        .unwrap_or(NO_TEXTURE)
                })
                .unwrap_or(NO_TEXTURE),
            // A specular-glossiness material has no metallic-roughness block —
            // the crate hands back the spec defaults for it, full metal at
            // full roughness — so its own numbers are read instead: glossiness
            // is roughness inverted, and the specular factor is a dielectric
            // tint rather than a metal, the way Blender's importer converts
            // it. Read as the defaults, the GPU preview multiplies the whole
            // diffuse away and paints the file black.
            factors: material
                .pbr_specular_glossiness()
                .map(|sg| [0.0, (1.0 - sg.glossiness_factor()).clamp(0.0, 1.0)])
                .unwrap_or_else(|| {
                    let pbr = material.pbr_metallic_roughness();
                    [pbr.metallic_factor(), pbr.roughness_factor()]
                }),
            // The normal map relights the surface and the occlusion map
            // darkens the lighting where the file says it is shadowed; both
            // carry their per-material strengths.
            normal_slot: normal_texture
                .as_ref()
                .and_then(|info| slot_of_texture.get(info.texture().index()).copied())
                .unwrap_or(NO_TEXTURE),
            normal_scale: normal_texture.map(|info| info.scale()).unwrap_or(1.0),
            ao_slot: ao_texture
                .as_ref()
                .and_then(|info| slot_of_texture.get(info.texture().index()).copied())
                .unwrap_or(NO_TEXTURE),
            ao_strength: ao_texture.map(|info| info.strength()).unwrap_or(1.0),
            // Emissive is the light the surface gives off on its own; the
            // texture, when there is one, multiplies the factor.
            emissive_slot: emissive_texture
                .as_ref()
                .and_then(|info| slot_of_texture.get(info.texture().index()).copied())
                .unwrap_or(NO_TEXTURE),
            emissive_factor: material
                .emissive_factor()
                .map(|channel| channel * emissive_strength),
            // The alpha, folded to two numbers: the cutoff a texel's alpha is
            // tested against (negative = never discard) and the base-colour
            // factor's alpha that multiplies the texel into the value tested.
            alpha_cutoff: match material.alpha_mode() {
                gltf::material::AlphaMode::Mask => material.alpha_cutoff().unwrap_or(0.5),
                gltf::material::AlphaMode::Blend => 0.5,
                gltf::material::AlphaMode::Opaque => -1.0,
            },
            alpha_factor: factor[3],
            double_sided: material.double_sided(),
        };
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
        // The UV is per vertex; the material is per primitive, stored once and
        // named by every vertex of the primitive. Only built when the document
        // carries images — `finish` attaches them to the mesh exactly then —
        // so a model with no textures, which is most of them, builds neither.
        if !self.textures.is_empty() {
            let material_index = if self.materials.len() >= NO_TEXTURE as usize {
                NO_TEXTURE // more materials than a slot index can name
            } else {
                self.materials.push(material_slot);
                (self.materials.len() - 1) as u16
            };
            match reader.read_tex_coords(0) {
                Some(uv) => {
                    for uv in uv.into_f32() {
                        self.uv.push(transform_uv(uv, &uv_transform));
                        self.material_of_vertex.push(material_index);
                    }
                }
                None => {
                    self.uv.extend(std::iter::repeat_n([0.0, 0.0], count));
                    self.material_of_vertex
                        .extend(std::iter::repeat_n(material_index, count));
                }
            }
        }

        let indices: Vec<u32> = match reader.read_indices() {
            Some(indices) => indices.into_u32().collect(),
            None => (0..(self.positions.len() - base as usize) as u32).collect(),
        };
        triangles_of(primitive.mode(), &indices, base, &mut self.triangles);
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
                    material: self.material_of_vertex,
                    materials: self.materials,
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

/// Append the triangles a primitive's index list describes, under its draw
/// mode, offset by `base` — the primitive's first vertex in the flattened
/// mesh. Writing into the caller's list directly saves the intermediate `Vec`
/// each primitive used to build and then copy.
///
/// A mode that is not a triangle kind — lines, or points — appends none.
fn triangles_of(mode: gltf::mesh::Mode, indices: &[u32], base: u32, out: &mut Vec<[u32; 3]>) {
    match mode {
        gltf::mesh::Mode::Triangles => {
            for triangle in indices.as_chunks::<3>().0 {
                out.push([base + triangle[0], base + triangle[1], base + triangle[2]]);
            }
        }
        gltf::mesh::Mode::TriangleStrip => {
            // Every window of three, with the winding alternated so the
            // strip's back faces do not flip on every other triangle.
            for (i, window) in indices.windows(3).enumerate() {
                out.push(if i % 2 == 0 {
                    [base + window[0], base + window[1], base + window[2]]
                } else {
                    [base + window[1], base + window[0], base + window[2]]
                });
            }
        }
        gltf::mesh::Mode::TriangleFan => {
            // Every triangle shares the fan's first vertex; a sliding window
            // would produce a strip instead, with the wrong triangles.
            for i in 1..indices.len().saturating_sub(1) {
                out.push([base + indices[0], base + indices[i], base + indices[i + 1]]);
            }
        }
        gltf::mesh::Mode::Points
        | gltf::mesh::Mode::Lines
        | gltf::mesh::Mode::LineLoop
        | gltf::mesh::Mode::LineStrip => {}
    }
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

/// The 3×3 matrix a primitive's normals go through: the inverse transpose of
/// the rotation-and-scale part, which is what keeps a normal perpendicular
/// under a non-uniform scale. Derived once per primitive by the caller — it is
/// constant for every vertex — and applied by [`apply_normal`].
///
/// A degenerate scale has no inverse: the raw rotation-and-scale block stands
/// in, and the normalised result is whatever it comes out as.
fn normal_matrix(m: [[f32; 4]; 4]) -> [[f32; 3]; 3] {
    // The three columns of the rotation-and-scale block.
    let c0 = [m[0][0], m[0][1], m[0][2]];
    let c1 = [m[1][0], m[1][1], m[1][2]];
    let c2 = [m[2][0], m[2][1], m[2][2]];
    let det = dot(c0, cross(c1, c2));
    if det.abs() < 1e-12 {
        return [c0, c1, c2];
    }
    // The inverse's columns are the cross products of the other two, scaled
    // by one determinant; the transpose then mixes equal components across
    // them.
    let one = 1.0 / det;
    [
        scale3(cross(c1, c2), one),
        scale3(cross(c2, c0), one),
        scale3(cross(c0, c1), one),
    ]
}

/// Apply [`normal_matrix`]'s columns to one normal. The length is not
/// restored: both renderers normalise.
fn apply_normal(cols: [[f32; 3]; 3], normal: [f32; 3]) -> [f32; 3] {
    [
        cols[0][0] * normal[0] + cols[1][0] * normal[1] + cols[2][0] * normal[2],
        cols[0][1] * normal[0] + cols[1][1] * normal[1] + cols[2][1] * normal[2],
        cols[0][2] * normal[0] + cols[1][2] * normal[1] + cols[2][2] * normal[2],
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
        // The mapped primitive first, the bare one second. A primitive's
        // vertices share one material, so a vertex names it and reads it back.
        let mapped = texture.material_of(0);
        assert_eq!(mapped.normal_slot, 0);
        assert_eq!(mapped.normal_scale, 2.0);
        assert_eq!(mapped.ao_slot, 0);
        assert_eq!(mapped.ao_strength, 0.5);
        let bare = texture.material_of(3);
        assert_eq!(
            bare.normal_slot, NO_TEXTURE,
            "the unmapped material carries the no-texture marker"
        );
        assert_eq!(bare.ao_slot, NO_TEXTURE);
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
        let masked = texture.material_of(0);
        assert_eq!(masked.alpha_cutoff, 0.25);
        assert_eq!(masked.alpha_factor, 0.8);
        assert_eq!(masked.emissive_factor, [1.5; 3]);
        // The bare material second: opaque, so nothing is ever discarded.
        let bare = texture.material_of(3);
        assert_eq!(bare.alpha_cutoff, -1.0);
        assert_eq!(bare.alpha_factor, 1.0);
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

    /// A normal goes through the inverse transpose of the rotation-and-scale
    /// block, derived once per primitive. This pins the values that
    /// per-primitive helper must produce.
    #[test]
    fn a_normal_is_transformed_by_the_inverse_transpose() {
        // Identity leaves a normal alone.
        let id = normal_matrix(IDENTITY);
        assert_eq!(apply_normal(id, [0.0, 1.0, 0.0]), [0.0, 1.0, 0.0]);

        // Scale x by two: the normal's x component halves, so it stays
        // perpendicular to a surface stretched along x.
        let mut scaled = IDENTITY;
        scaled[0][0] = 2.0;
        let cols = normal_matrix(scaled);
        assert_eq!(apply_normal(cols, [1.0, 0.0, 0.0]), [0.5, 0.0, 0.0]);
        assert_eq!(apply_normal(cols, [0.0, 1.0, 0.0]), [0.0, 1.0, 0.0]);

        // A degenerate scale has no inverse: the raw block stands in, so a
        // zero scale flattens the normal rather than dividing by zero.
        let mut zero = IDENTITY;
        zero[0][0] = 0.0;
        let cols = normal_matrix(zero);
        assert_eq!(apply_normal(cols, [1.0, 1.0, 1.0]), [0.0, 1.0, 1.0]);
    }

    /// A strip alternates winding; reading it as a triangle list instead
    /// yields half the triangles and the wrong ones.
    #[test]
    fn triangle_strips_and_fans_are_converted() {
        let mut strip = Vec::new();
        triangles_of(gltf::mesh::Mode::TriangleStrip, &[0, 1, 2, 3], 0, &mut strip);
        assert_eq!(strip, vec![[0, 1, 2], [2, 1, 3]]);

        // The primitive's `base` offsets every index, so a later primitive's
        // triangles point at its own vertices in the flattened mesh.
        let mut offset = Vec::new();
        triangles_of(
            gltf::mesh::Mode::TriangleStrip,
            &[0, 1, 2, 3],
            10,
            &mut offset,
        );
        assert_eq!(offset, vec![[10, 11, 12], [12, 11, 13]]);

        let mut fan = Vec::new();
        triangles_of(gltf::mesh::Mode::TriangleFan, &[0, 1, 2, 3], 0, &mut fan);
        assert_eq!(fan, vec![[0, 1, 2], [0, 2, 3]]);
        // A list is taken three at a time, and a run that does not divide by
        // three is not a triangle.
        let mut list = Vec::new();
        triangles_of(
            gltf::mesh::Mode::Triangles,
            &[0, 1, 2, 3, 4, 5, 6],
            0,
            &mut list,
        );
        assert_eq!(list, vec![[0, 1, 2], [3, 4, 5]]);
        // Lines and points are not surfaces: neither appends anything.
        let mut none = Vec::new();
        triangles_of(gltf::mesh::Mode::Points, &[0, 1, 2], 0, &mut none);
        triangles_of(gltf::mesh::Mode::LineStrip, &[0, 1, 2], 0, &mut none);
        assert!(none.is_empty());
    }

    /// A `data:` URI yields its payload: base64 as the spec writes it, or the
    /// percent-escaped bytes RFC 2397 also allows.
    #[test]
    fn a_data_uri_yields_its_payload() {
        // "PNG" base64-encodes to "UE5H".
        assert_eq!(data_uri_bytes("image/png;base64,UE5H"), Some(b"PNG".to_vec()));
        assert_eq!(data_uri_bytes("image/png,%41%42"), Some(b"AB".to_vec()));
        // A URI with no comma names nothing.
        assert_eq!(data_uri_bytes("image/png;base64"), None);
    }

    /// Percent escapes decode; a malformed or truncated one stands as written,
    /// so a name that was never escaped survives.
    #[test]
    fn percent_escapes_decode_and_malformed_ones_stand() {
        assert_eq!(percent_decode("my%20texture.png"), "my texture.png");
        assert_eq!(percent_decode("plain.png"), "plain.png");
        assert_eq!(percent_decode("odd%2"), "odd%2");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    /// Write a self-contained `.gltf` (buffers and images inlined as `data:`
    /// URIs) so a test needs no companion files.
    fn gltf_file(json: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-gltf-test-{}.gltf",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, json).unwrap();
        path
    }

    /// An image carried inline as a `data:` URI becomes a texture slot like
    /// any other. Sketchfab-style `.gltf` exports write their textures this
    /// way, and reading the URI as a file path silently dropped every one.
    #[test]
    fn an_image_in_a_data_uri_becomes_a_slot() {
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
            v
        };
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let json = format!(
            r#"{{
            "asset": {{"version": "2.0"}},
            "scenes": [{{"nodes": [0]}}],
            "scene": 0,
            "nodes": [{{"mesh": 0}}],
            "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0}}, "indices": 1, "material": 0}}]}}],
            "materials": [{{"pbrMetallicRoughness": {{"baseColorTexture": {{"index": 0}}}}}}],
            "textures": [{{"source": 0}}],
            "images": [{{"uri": "data:image/png;base64,{}"}}],
            "buffers": [{{"byteLength": 48, "uri": "data:application/octet-stream;base64,{}"}}],
            "bufferViews": [
                {{"buffer": 0, "byteOffset": 0, "byteLength": 36, "target": 34962}},
                {{"buffer": 0, "byteOffset": 36, "byteLength": 12, "target": 34963}}
            ],
            "accessors": [
                {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "max": [1,1,0], "min": [0,0,0]}},
                {{"bufferView": 1, "componentType": 5125, "count": 3, "type": "SCALAR"}}
            ]
        }}"#,
            encode(&png),
            encode(&bin)
        );
        let path = gltf_file(&json);
        let mesh = load_gltf(&path).expect("a data-uri gltf parses");
        std::fs::remove_file(&path).ok();
        let texture = mesh.texture.as_ref().expect("the inline image becomes a slot");
        assert_eq!(texture.maps.len(), 1);
        assert_eq!(texture.material_of(0).slot, 0);
    }
}
