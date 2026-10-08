//! Background removal on the CPU: a U²-Net saliency graph run through
//! `candle-onnx`.
//!
//! The model is one 168 MB ONNX file (the same weights `rembg` fetches), and
//! candle is the only thing executing it — no Python, no ONNX Runtime, no
//! subprocess. The graph eats a fixed 320×320 tensor and answers with a
//! 320×320 confidence map; the source's own pixels are kept and the map is
//! scaled back up to them as alpha, so a 6000×4000 photo comes out at
//! 6000×4000.
//!
//! One thing about [`candle_onnx`] is load-bearing enough to belong in the
//! doc comment: its `Resize` implements nearest-neighbour only, and it refuses
//! a node that carries both `scales` and `sizes`. Torch exports do exactly
//! that, and they leave out the two attributes candle-onnx then defaults to
//! values it rejects. [`repair_resize`] is the pass that makes the graph run
//! at all, and what it costs is the decoder's bilinear refinement — measured
//! on a real photo the silhouette is right and the edge is one pixel coarser
//! than the reference implementation's.

use std::path::Path;

use candle_core::{DType, Tensor};
use candle_onnx::onnx::attribute_proto::AttributeType;
use candle_onnx::onnx::{AttributeProto, GraphProto};
use image::{GrayImage, ImageEncoder as _, Luma, RgbaImage};

use crate::error::{Error, Result};

/// The side the graph is fed and answers on. U²-Net is not fully
/// convolutional-with-arbitrary-input: the checkpoint was trained at 320 and
/// that is what its statistics assume.
const BUCKET: usize = 320;

/// The checkpoint maps pixels from `[0,1]` to `[-1,1]`. The ImageNet mean and
/// standard deviation most vision models expect would shift every channel and
/// the mask comes back reporting the whole frame as background — measured, and
/// it looks exactly like a broken pipeline.
const SHIFT: f32 = 1.0;

/// A loaded saliency graph, ready to run. Loading parses 168 MB of protobuf,
/// so a batch holds one of these and reuses it across every asset.
pub struct SaliencyModel {
    model: candle_onnx::onnx::ModelProto,
    input: String,
    output: String,
    /// How many upsample nodes [`repair_resize`] had to rewrite. Zero means
    /// the export was already in the shape candle-onnx accepts.
    repaired: usize,
}

impl SaliencyModel {
    /// Read an ONNX U²-Net checkpoint and put it into the shape the
    /// interpreter accepts.
    pub fn load(path: &Path) -> Result<Self> {
        let mut model = candle_onnx::read_file(path).map_err(|e| Error::External {
            program: "candle-onnx".into(),
            message: format!("read {}: {e}", path.display()),
        })?;
        let graph = model
            .graph
            .as_mut()
            .ok_or_else(|| Error::Validation("the model carries no graph".into()))?;
        let repaired = repair_resize(graph);

        // Weights ride along as initializers, which older exports also list
        // among the inputs — the real input is the one that is not a weight.
        let initializers: Vec<&str> = graph
            .initializer
            .iter()
            .map(|t| t.name.as_str())
            .filter(|n| !n.is_empty())
            .collect();
        let input = graph
            .input
            .iter()
            .map(|i| i.name.as_str())
            .find(|name| !initializers.contains(name))
            .ok_or_else(|| Error::Validation("the model declares no input".into()))?
            .to_string();
        // U²-Net answers with the full-resolution prediction first and the
        // deep-supervision side maps after it. Reading a side map instead is
        // how a working pipeline ends up reporting an empty mask.
        let output = graph
            .output
            .first()
            .map(|o| o.name.clone())
            .ok_or_else(|| Error::Validation("the model declares no output".into()))?;

        Ok(Self {
            model,
            input,
            output,
            repaired,
        })
    }

    /// The number of upsample nodes the loader had to rewrite. A log line
    /// material, and a test assertion that the repair actually fires.
    pub fn repaired_resizes(&self) -> usize {
        self.repaired
    }

    /// The saliency map of `source`, at the source's own resolution, one byte
    /// per pixel.
    pub fn alpha(&self, source: &RgbaImage) -> Result<GrayImage> {
        let (w, h) = (source.width(), source.height());
        if w == 0 || h == 0 {
            return Err(Error::Validation("image has no pixels".into()));
        }
        let (device, _) = crate::ai::local_device::select_device();
        let input = Tensor::from_vec(input_pixels(source), (1, 3, BUCKET, BUCKET), &device)
            .map_err(candle_error)?;
        let answers = candle_onnx::simple_eval(
            &self.model,
            std::collections::HashMap::from([(self.input.clone(), input)]),
        )
        .map_err(candle_error)?;
        let mask = answers.get(&self.output).ok_or_else(|| Error::External {
            program: "candle-onnx".into(),
            message: "the model returned no main output".into(),
        })?;
        let values = mask
            .to_dtype(DType::F32)
            .and_then(|m| m.flatten_all())
            .and_then(|m| m.to_vec1::<f32>())
            .map_err(candle_error)?;
        Ok(alpha_map(&values, w, h))
    }

    /// The source's pixels with the model's alpha pasted onto them. The RGB
    /// is the source's own — the model only ever decides opacity.
    pub fn cutout(&self, source: &RgbaImage) -> Result<RgbaImage> {
        let alpha = self.alpha(source)?;
        let (w, h) = (source.width(), source.height());
        Ok(RgbaImage::from_fn(w, h, |x, y| {
            let mut px = *source.get_pixel(x, y);
            px[3] = alpha.get_pixel(x, y)[0];
            px
        }))
    }

    /// Decode `source`, cut it out, and write a PNG to `dest`.
    ///
    /// The file's colour claim travels with the pixels, exactly as an in-place
    /// edit carries it: dropping the profile here would repaint the cutout
    /// relative to the asset it came from.
    pub fn write_cutout(&self, source: &Path, dest: &Path) -> Result<()> {
        let decoded = image::open(source)
            .map_err(|e| Error::Validation(format!("decode failed: {e}")))?
            .to_rgba8();
        let cutout = self.cutout(&decoded)?;
        let profile = crate::media::color_profile::profile_of_file(source);

        let mut out = std::io::Cursor::new(Vec::new());
        let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
        if let Some(profile) = &profile {
            let _ = encoder.set_icc_profile(profile.clone());
        }
        encoder
            .write_image(
                cutout.as_raw(),
                cutout.width(),
                cutout.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| Error::Validation(format!("encode failed: {e}")))?;

        std::fs::write(dest, out.into_inner())?;
        Ok(())
    }
}

fn candle_error(e: candle_core::Error) -> Error {
    Error::External {
        program: "candle-onnx".into(),
        message: e.to_string(),
    }
}

/// Rewrite every `Resize` node into the one shape candle-onnx accepts:
/// nearest-neighbour with `asymmetric`/`floor` geometry, and a target given
/// either by scales or by sizes — never both. Returns how many nodes changed.
fn repair_resize(graph: &mut GraphProto) -> usize {
    let mut touched = 0;
    for node in graph.node.iter_mut() {
        if node.op_type != "Resize" {
            continue;
        }
        touched += 1;
        // The exported graphs carry the target dims explicitly; the scales
        // that ride along are the redundant half.
        if node.input.len() > 3 && !node.input[2].is_empty() && !node.input[3].is_empty() {
            node.input[2] = String::new();
        }
        set_string_attribute(node, "mode", b"nearest");
        set_string_attribute(node, "nearest_mode", b"floor");
        set_string_attribute(node, "coordinate_transformation_mode", b"asymmetric");
    }
    touched
}

/// Set a string attribute, adding it when the export left it out — which is
/// the common case, and the one that otherwise makes candle-onnx default to a
/// value it then refuses.
fn set_string_attribute(node: &mut candle_onnx::onnx::NodeProto, name: &str, value: &[u8]) {
    if let Some(existing) = node.attribute.iter_mut().find(|a| a.name == name) {
        existing.s = value.to_vec();
        existing.r#type = AttributeType::String as i32;
        return;
    }
    node.attribute.push(AttributeProto {
        name: name.to_string(),
        s: value.to_vec(),
        r#type: AttributeType::String as i32,
        ..Default::default()
    });
}

/// The graph's input plane: 320×320, planar RGB, `[-1,1]`.
fn input_pixels(source: &RgbaImage) -> Vec<f32> {
    let small = image::imageops::resize(
        source,
        BUCKET as u32,
        BUCKET as u32,
        image::imageops::FilterType::CatmullRom,
    );
    let mut chw = vec![0f32; 3 * BUCKET * BUCKET];
    for y in 0..BUCKET {
        for x in 0..BUCKET {
            let px = small.get_pixel(x as u32, y as u32);
            for c in 0..3 {
                let v = f32::from(px[c]) / 255.0;
                chw[c * BUCKET * BUCKET + y * BUCKET + x] = v * 2.0 - SHIFT;
            }
        }
    }
    chw
}

/// The 320×320 confidence map scaled up to the source. Values are used as the
/// network left them (its last op is a sigmoid), clamped only against a
/// runaway float.
fn alpha_map(values: &[f32], width: u32, height: u32) -> GrayImage {
    debug_assert_eq!(values.len(), BUCKET * BUCKET);
    let mask = GrayImage::from_vec(
        BUCKET as u32,
        BUCKET as u32,
        values
            .iter()
            .map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8)
            .collect(),
    )
    .unwrap_or_else(|| GrayImage::from_pixel(BUCKET as u32, BUCKET as u32, Luma([0u8])));
    image::imageops::resize(
        &mask,
        width,
        height,
        image::imageops::FilterType::CatmullRom,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn resize_node(mode: &str, scales: &str, sizes: &str) -> candle_onnx::onnx::NodeProto {
        let mut node = candle_onnx::onnx::NodeProto {
            op_type: "Resize".into(),
            name: "r".into(),
            input: vec!["x".into(), String::new(), scales.into(), sizes.into()],
            ..Default::default()
        };
        set_string_attribute(&mut node, "mode", mode.as_bytes());
        node
    }

    /// The whole reason the loader exists: an export that arrives in the shape
    /// torch writes has to leave in the one shape candle-onnx reads, or the
    /// first upsample aborts the run.
    #[test]
    fn resize_nodes_leave_in_the_shape_the_interpreter_accepts() {
        let mut graph = GraphProto {
            node: vec![
                resize_node("linear", "", "/Concat_5_output_0"),
                resize_node("linear", "/Constant_output_0", "/Concat_9_output_0"),
                candle_onnx::onnx::NodeProto {
                    op_type: "Conv".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert_eq!(repair_resize(&mut graph), 2, "only the Resize nodes count");

        let accepted = |node: &candle_onnx::onnx::NodeProto| -> Vec<String> {
            ["mode", "nearest_mode", "coordinate_transformation_mode"]
                .iter()
                .map(|name| {
                    node.attribute
                        .iter()
                        .find(|a| a.name == *name)
                        .map(|a| String::from_utf8_lossy(&a.s).into_owned())
                        .unwrap_or_default()
                })
                .collect()
        };
        for node in &graph.node[..2] {
            assert_eq!(accepted(node), ["nearest", "floor", "asymmetric"]);
        }
        assert!(
            graph.node[0].input[2].is_empty(),
            "the node that only had sizes keeps them"
        );
        assert!(
            graph.node[1].input[2].is_empty(),
            "scales are dropped where sizes are present"
        );
        assert_eq!(graph.node[1].input[3], "/Concat_9_output_0");
    }

    /// Black maps to -1, white to +1, and the three planes are planar, not
    /// interleaved.
    #[test]
    fn input_planes_are_planar_and_span_minus_one_to_one() {
        let plane = RgbaImage::from_pixel(2, 2, Rgba([0, 128, 255, 255]));
        let chw = input_pixels(&plane);
        assert_eq!(chw.len(), 3 * BUCKET * BUCKET);
        let stride = BUCKET * BUCKET;
        assert!(
            chw[..stride].iter().all(|v| (*v - -1.0).abs() < 0.01),
            "the red plane follows black"
        );
        assert!(
            chw[stride..2 * stride]
                .iter()
                .all(|v| (v - (128.0 / 255.0 * 2.0 - 1.0)).abs() < 0.01),
            "the green plane sits mid-range"
        );
        assert!(
            chw[2 * stride..].iter().all(|v| (*v - 1.0).abs() < 0.01),
            "the blue plane follows white"
        );
    }

    /// A flat map stays flat through the scale-up — the geometry that puts
    /// the cutout's edge where the source's edge is.
    #[test]
    fn a_flat_map_scales_to_the_source_size() {
        let values = vec![1.0f32; BUCKET * BUCKET];
        let alpha = alpha_map(&values, 900, 640);
        assert_eq!((alpha.width(), alpha.height()), (900, 640));
        assert!(
            alpha.pixels().all(|p| p[0] > 250),
            "a fully salient map must not punch holes"
        );

        let empty = vec![0.0f32; BUCKET * BUCKET];
        assert!(alpha_map(&empty, 64, 64).pixels().all(|p| p[0] < 5));
    }

    /// The end-to-end claim, against the real checkpoint. Needs the 168 MB
    /// model on disk, so it is a gate to run by hand rather than a test the
    /// suite pays for.
    ///
    /// ```text
    /// TROVE_U2NET=/path/to/u2net.onnx cargo test -p trove-core --lib matting -- --ignored
    /// ```
    #[test]
    #[ignore = "needs the u2net checkpoint on disk"]
    fn the_checkpoint_segments_a_real_photo() {
        let path = std::env::var("TROVE_U2NET").expect("set TROVE_U2NET to a u2net.onnx");
        let model = SaliencyModel::load(std::path::Path::new(&path)).unwrap();
        assert!(
            model.repaired_resizes() > 0,
            "the export is expected to need the resize repair"
        );
        let source = RgbaImage::from_fn(640, 480, |x, y| {
            let inside = (x as i32 - 320).abs() < 120 && (y as i32 - 240).abs() < 90;
            if inside {
                Rgba([230, 200, 40, 255])
            } else {
                let v = 40 + (x as u8) / 4;
                Rgba([v, v, v, 255])
            }
        });
        let alpha = model.alpha(&source).unwrap();
        assert_eq!((alpha.width(), alpha.height()), (640, 480));
        let centre = alpha.get_pixel(320, 240)[0];
        let corner = alpha.get_pixel(10, 10)[0];
        assert!(
            centre > 200 && corner < 80,
            "the block should read as the subject: centre {centre}, corner {corner}"
        );
    }
}
