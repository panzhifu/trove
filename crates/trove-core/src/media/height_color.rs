//! Painting a model by a per-point scalar field, instead of by a flat material
//! colour.
//!
//! This is Trove's version of CloudCompare's colour-by-field path
//! (`ccPointCloud::setRGBColorByHeight` and `setRGBColorByBanding`, driven by
//! `ccColorGradientDlg` and `ccColorScalesManager`), with three decisions of
//! its own:
//!
//! * The colour is resolved **in the shader**, from a stop table that rides in
//!   the uniform block, rather than baked into the vertex colour table. That is
//!   what lets a streamed cloud of any size change its look for one buffer
//!   write, and it is why [`HeightField::tint_at`] and `surface_color` in
//!   `gpu3d.wgsl` are the same function twice.
//! * The field's range is normalised to 0..=1. For [`Field::Height`] it comes
//!   from the model's own bounding box, so a three-unit tabletop and a
//!   five-kilometre terrain both fill their colour scale; a model flat along
//!   the chosen axis gets the scale's first colour, as CloudCompare's
//!   flat-cloud branch does. [`Field::Slope`] and [`Field::Aspect`] carry
//!   CloudCompare's *absolute* scales instead — 0..=90° and 0..=360° — because a
//!   dip of 30° means the same thing in every file.
//! * Banding is measured, not counted: a cycle is a number of field units, the
//!   way CloudCompare's is, because stripes are a ruler laid across the surface
//!   and only read as one if their period is a known distance.
//!
//! The five fields split by where their values come from: positions give the
//! height, normals give dip and its direction, and the two scanner attributes —
//! return intensity and ASPRS classification — need the file to have carried
//! that channel, which is the one thing a model can be missing. A [`Field`]
//! whose channel is absent is not painted at all rather than painted zero, and
//! it is the viewport's job to say so.

use serde::{Deserialize, Serialize};

use super::formats::types::Bounds;

/// How many anchors one colour scale may carry, which is also how many fit the
/// uniform block the shader reads: 32 because CloudCompare's ASPRS
/// classification scale has 23 anchors, and a categorical scale is only exact
/// when every class gets one of its own.
pub const RAMP_STOPS: usize = 32;
/// Field units per cycle of the banding stripes when nothing has been chosen
/// yet — CloudCompare's own default, whose dialog labels the row *Period* while
/// the widget behind it is still named `bandingFreqSpinBox`.
pub const DEFAULT_PERIOD: f32 = 5.0;
/// The stepper's bounds. A period of zero would divide the circle by nothing —
/// CloudCompare refuses it outright — and the ceiling is its spin box maximum.
pub const MIN_PERIOD: f32 = 1e-6;
pub const MAX_PERIOD: f32 = 1e6;
/// How far behind the red channel the green and blue ones start: a third and
/// two thirds of the way round the cycle.
const BAND_PHASE_2: f32 = 2.094_4;
const BAND_PHASE_3: f32 = 4.188_8;
/// A span this flat carries no information to normalise against. CloudCompare
/// compares against an epsilon for the same reason: dividing by it would put
/// every point at some wild index of the scale.
const FLAT_SPAN: f32 = 1e-6;
/// A normal shorter than this carries no direction to read a dip off.
const DEGENERATE_NORMAL: f32 = 1e-6;
/// The prefix a config's `height_scale` carries when it names a user-made
/// scale rather than a built-in one, so the two namespaces cannot collide.
pub const CUSTOM_PREFIX: &str = "custom:";
/// The pair a custom scale starts with: CloudCompare's own `s_firstColor` and
/// `s_secondColor` statics, black to white.
pub const DEFAULT_COLOUR_LOW: [u8; 3] = [0, 0, 0];
pub const DEFAULT_COLOUR_HIGH: [u8; 3] = [255, 255, 255];

/// One anchor of a colour scale.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorStop {
    /// Position along the scale, 0..=1, ascending.
    pub at: f32,
    /// Colour there.
    pub rgb: [u8; 3],
}

const fn stop(at: f32, rgb: [u8; 3]) -> ColorStop {
    ColorStop { at, rgb }
}

impl ColorStop {
    /// One anchor, for a caller building a scale of its own — the viewport's
    /// colour-scale editor is the only one, but it needs this to exist.
    pub const fn new(at: f32, rgb: [u8; 3]) -> Self {
        Self { at, rgb }
    }
}

/// A scale the user made: the anchors, and the id the config and the per-asset
/// look refer to it by.
///
/// The reference is the point — CloudCompare stores a scale's UUID on a scalar
/// field rather than a copy of it — so editing one here changes every model
/// that names it, and deleting one leaves those models falling back to the
/// default rather than holding a stale copy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomScale {
    pub id: String,
    pub stops: Vec<ColorStop>,
}

impl CustomScale {
    /// A new one: the two ends of the first scale on offer, which is what
    /// CloudCompare's colour buttons start with.
    pub fn seed(id: String) -> Self {
        Self {
            id,
            stops: Ramp::defaults().stops().to_vec(),
        }
    }

    /// The id as it appears in a config's `height_scale`.
    pub fn key(&self) -> String {
        format!("{CUSTOM_PREFIX}{}", self.id)
    }

    /// The id behind a `custom:<id>` key.
    pub fn id_of(key: &str) -> Option<&str> {
        key.strip_prefix(CUSTOM_PREFIX)
    }

    /// Anchors cleaned up for use: sorted, ends pinned to 0 and 1, duplicates
    /// gone, and no more than a uniform's worth.
    pub fn ramp(&self) -> Ramp {
        // Never bins: a scale the user assembled from anchors means what it
        // says between them, which is the point of moving one.
        Ramp::custom(&self.stops, false)
    }
}

/// A named scale: the stops, linearly interpolated between, exactly as
/// `ccColorScale::update` resamples them into its lookup table.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorScale {
    /// The id persisted in the config. Stable, so reordering [`COLOR_SCALES`]
    /// cannot strand a saved choice on another scale.
    pub id: &'static str,
    /// Suffix of the `viewport.height_scale_*` label.
    pub name_key: &'static str,
    pub stops: &'static [ColorStop],
}

/// The built-in scales whose anchors are *bins* rather than points on a
/// gradient: a classification scale's colour belongs to its class and to
/// nothing between two classes.
///
/// A list of ids rather than a field on every literal, because exactly one of
/// the sixteen tables is categorical and the other fifteen would each have had
/// to say so.
pub const CATEGORICAL_SCALES: [&str; 1] = ["asprs"];

impl ColorScale {
    /// Whether values land *on* an anchor rather than between two.
    pub fn categorical(&self) -> bool {
        CATEGORICAL_SCALES.contains(&self.id)
    }

    /// The scale drawn as `steps` discrete colours, start to end.
    pub fn gradient(&self, steps: usize) -> Vec<[u8; 3]> {
        self.ramp().gradient(steps)
    }

    fn ramp(&self) -> Ramp {
        Ramp::from_stops(self.stops, self.categorical())
    }
}

/// The scales the viewport offers, in the order it lists them. The first is the
/// one a model gets when colouring is switched on and no scale was ever picked,
/// so it is CloudCompare's default (`BGYR`, its `GetDefaultScale`).
pub static COLOR_SCALES: [ColorScale; 16] = [
    ColorScale {
        id: "bgyr",
        name_key: "bgyr",
        stops: &[
            stop(0.0, [0, 0, 255]),
            stop(1.0 / 3.0, [0, 255, 0]),
            stop(2.0 / 3.0, [255, 255, 0]),
            stop(1.0, [255, 0, 0]),
        ],
    },
    ColorScale {
        id: "bwr",
        name_key: "bwr",
        stops: &[
            stop(0.0, [0, 0, 255]),
            stop(0.5, [255, 255, 255]),
            stop(1.0, [255, 0, 0]),
        ],
    },
    ColorScale {
        id: "grey",
        name_key: "grey",
        stops: &[stop(0.0, [0, 0, 0]), stop(1.0, [255, 255, 255])],
    },
    ColorScale {
        id: "grey-inverse",
        name_key: "grey_inverse",
        stops: &[stop(0.0, [255, 255, 255]), stop(1.0, [0, 0, 0])],
    },
    ColorScale {
        id: "red-yellow",
        name_key: "red_yellow",
        stops: &[stop(0.0, [255, 0, 0]), stop(1.0, [255, 255, 0])],
    },
    ColorScale {
        id: "red-white",
        name_key: "red_white",
        stops: &[stop(0.0, [255, 0, 0]), stop(1.0, [255, 255, 255])],
    },
    ColorScale {
        // CloudCompare's `Brown>Yellow`, and `Yellow>Brown` turned round.
        id: "brown-yellow",
        name_key: "brown_yellow",
        stops: &[
            stop(0.0, [153, 51, 3]),
            stop(0.25, [217, 91, 13]),
            stop(0.5, [254, 151, 41]),
            stop(0.75, [254, 217, 142]),
            stop(1.0, [255, 255, 212]),
        ],
    },
    ColorScale {
        id: "yellow-brown",
        name_key: "yellow_brown",
        stops: &[
            stop(0.0, [255, 255, 212]),
            stop(0.25, [254, 217, 142]),
            stop(0.5, [254, 151, 41]),
            stop(0.75, [217, 91, 13]),
            stop(1.0, [153, 51, 3]),
        ],
    },
    ColorScale {
        // CloudCompare's `HSV angle [0-360]`: the six primary and secondary
        // hues, back where it started so the two ends match.
        id: "hsv",
        name_key: "hsv",
        stops: &[
            stop(0.0, [255, 0, 0]),
            stop(1.0 / 6.0, [255, 255, 0]),
            stop(2.0 / 6.0, [0, 255, 0]),
            stop(3.0 / 6.0, [0, 255, 255]),
            stop(4.0 / 6.0, [0, 0, 255]),
            stop(5.0 / 6.0, [255, 0, 255]),
            stop(1.0, [255, 0, 0]),
        ],
    },
    ColorScale {
        // Sampled from the 256-entry table CloudCompare ships, which is the
        // same viridis matplotlib is.
        id: "viridis",
        name_key: "viridis",
        stops: &[
            stop(0.0, [68, 1, 84]),
            stop(0.1, [72, 37, 118]),
            stop(0.2, [65, 68, 135]),
            stop(0.3, [52, 96, 141]),
            stop(0.4, [42, 120, 142]),
            stop(0.5, [33, 145, 140]),
            stop(0.6, [34, 168, 132]),
            stop(0.7, [68, 191, 112]),
            stop(0.8, [122, 209, 81]),
            stop(0.9, [189, 223, 38]),
            stop(1.0, [253, 231, 37]),
        ],
    },
    ColorScale {
        id: "cividis",
        name_key: "cividis",
        stops: &[
            stop(0.0, [0, 32, 77]),
            stop(0.125, [6, 54, 110]),
            stop(0.25, [65, 77, 107]),
            stop(0.375, [97, 100, 111]),
            stop(0.5, [125, 124, 120]),
            stop(0.625, [155, 148, 119]),
            stop(0.75, [188, 175, 111]),
            stop(0.875, [224, 203, 94]),
            stop(1.0, [255, 234, 70]),
        ],
    },
    ColorScale {
        // CloudCompare's `Topo landserf`, stop for stop: the hypsometric tints
        // of a printed relief map.
        id: "topo",
        name_key: "topo",
        stops: &[
            stop(0.0, [109, 158, 93]),
            stop(0.25, [255, 255, 127]),
            stop(0.5, [194, 108, 54]),
            stop(0.75, [85, 63, 50]),
            stop(1.0, [255, 255, 255]),
        ],
    },
    ColorScale {
        id: "high-contrast",
        name_key: "high_contrast",
        stops: &[
            stop(0.0, [170, 255, 255]),
            stop(0.01, [158, 158, 158]),
            stop(0.02, [0, 0, 127]),
            stop(0.04, [0, 255, 0]),
            stop(0.08, [0, 85, 0]),
            stop(0.16, [255, 255, 0]),
            stop(0.32, [255, 0, 0]),
            stop(0.5, [135, 0, 0]),
            stop(1.0, [232, 232, 232]),
        ],
    },
    ColorScale {
        // CloudCompare's `ASPRS classes`, in class order: the bin a point falls
        // in is its classification, and the colour is that class's, not a
        // position between two. Class names are in the file's own metadata, so
        // the legend shows numbers — which is what CC's `ASPRS_CLASSES` scale
        // without labels is, and the reason the `WITH_LABELS` twin exists.
        id: "asprs",
        name_key: "asprs",
        stops: &[
            stop(0.0 / 23.0, [255, 255, 255]),
            stop(1.0 / 23.0, [192, 192, 192]),
            stop(2.0 / 23.0, [166, 116, 4]),
            stop(3.0 / 23.0, [38, 114, 0]),
            stop(4.0 / 23.0, [69, 229, 0]),
            stop(5.0 / 23.0, [204, 240, 123]),
            stop(6.0 / 23.0, [255, 255, 0]),
            stop(7.0 / 23.0, [255, 0, 0]),
            stop(8.0 / 23.0, [255, 0, 255]),
            stop(9.0 / 23.0, [0, 0, 255]),
            stop(10.0 / 23.0, [85, 85, 0]),
            stop(11.0 / 23.0, [128, 128, 128]),
            stop(12.0 / 23.0, [255, 170, 255]),
            stop(13.0 / 23.0, [191, 231, 205]),
            stop(14.0 / 23.0, [193, 230, 125]),
            stop(15.0 / 23.0, [0, 0, 139]),
            stop(16.0 / 23.0, [128, 128, 0]),
            stop(17.0 / 23.0, [0, 139, 139]),
            stop(18.0 / 23.0, [139, 0, 0]),
            stop(19.0 / 23.0, [255, 170, 255]),
            stop(20.0 / 23.0, [50, 255, 198]),
            stop(21.0 / 23.0, [255, 250, 250]),
            stop(22.0 / 23.0, [0, 0, 0]),
        ],
    },
    ColorScale {
        // CloudCompare's `Dip [0-90]`, made for [`Field::Slope`].
        id: "dip",
        name_key: "dip",
        stops: &[
            stop(0.0, [129, 0, 0]),
            stop(0.33, [255, 68, 0]),
            stop(0.66, [255, 255, 0]),
            stop(1.0, [255, 255, 255]),
        ],
    },
    ColorScale {
        // CloudCompare's `Dip direction (repeat) [0-360]`, made for
        // [`Field::Aspect`]: the hue runs twice round the compass so opposite
        // bearings do not share a colour.
        id: "dip-direction",
        name_key: "dip_direction",
        stops: &[
            stop(0.0 / 360.0, [255, 0, 0]),
            stop(30.0 / 360.0, [255, 255, 0]),
            stop(60.0 / 360.0, [0, 255, 0]),
            stop(90.0 / 360.0, [0, 255, 255]),
            stop(120.0 / 360.0, [0, 0, 255]),
            stop(150.0 / 360.0, [255, 0, 255]),
            stop(180.0 / 360.0, [255, 0, 0]),
            stop(210.0 / 360.0, [255, 255, 0]),
            stop(240.0 / 360.0, [0, 255, 0]),
            stop(270.0 / 360.0, [0, 255, 255]),
            stop(300.0 / 360.0, [0, 0, 255]),
            stop(330.0 / 360.0, [255, 0, 255]),
            stop(1.0, [255, 0, 0]),
        ],
    },
];

/// The scale a given id names, falling back to the default. A config written by
/// a newer version — or a hand-edited one — names an id that is not here; that
/// should cost a colour, not a crash.
pub fn scale_by_id(id: &str) -> &'static ColorScale {
    COLOR_SCALES
        .iter()
        .find(|scale| scale.id == id)
        .unwrap_or(&COLOR_SCALES[0])
}

/// Which scale a ramp paints with: one of the built-in tables, or a scale the
/// user built. CloudCompare's `Default` and `Custom` radio buttons are the same
/// fork — hand over the named scale, or the one being edited.
#[derive(Clone, Debug, PartialEq)]
pub enum Scale {
    Preset(&'static ColorScale),
    /// The ramp is boxed because a preset scale — which is what almost every
    /// look is — should not pay for the 32 anchors a user's own carries.
    Custom {
        id: String,
        ramp: Box<Ramp>,
    },
}

impl Default for Scale {
    fn default() -> Self {
        Self::Preset(&COLOR_SCALES[0])
    }
}

impl Scale {
    /// The id the config and a per-asset look store.
    pub fn key(&self) -> String {
        match self {
            Self::Preset(scale) => scale.id.to_string(),
            Self::Custom { id, .. } => format!("{CUSTOM_PREFIX}{id}"),
        }
    }

    /// The user-made scale this names, if it names one.
    pub fn custom_id(&self) -> Option<&str> {
        match self {
            Self::Custom { id, .. } => Some(id),
            Self::Preset(_) => None,
        }
    }

    /// A user-made scale by id, or the default preset when that id is gone —
    /// deleting a scale costs a colour, not a model that will not open.
    pub fn custom(custom: &CustomScale) -> Self {
        Self::Custom {
            id: custom.id.clone(),
            ramp: Box::new(custom.ramp()),
        }
    }

    fn ramp(&self) -> Ramp {
        match self {
            Self::Preset(scale) => scale.ramp(),
            Self::Custom { ramp, .. } => **ramp,
        }
    }

    /// Whether the anchors are bins, whichever scale is in use.
    pub fn categorical(&self) -> bool {
        self.ramp().categorical()
    }

    /// The live anchors, for the editor: the built-ins' own, or the user's.
    pub fn stops(&self) -> Vec<ColorStop> {
        match self {
            Self::Preset(scale) => scale.stops.to_vec(),
            Self::Custom { ramp, .. } => ramp.stops().to_vec(),
        }
    }

    /// The scale drawn as `steps` discrete colours, for a preview strip.
    pub fn gradient(&self, steps: usize) -> Vec<[u8; 3]> {
        self.ramp().gradient(steps)
    }
}

impl Sample {
    /// A point with nothing but its position: enough for every field that reads
    /// the geometry, which is most of what the tests and the legend ask.
    pub fn at(point: [f32; 3]) -> Self {
        Self {
            point,
            ..Default::default()
        }
    }

    /// A point and the normal it faces, for the two fields read off the normal.
    pub fn surface(point: [f32; 3], normal: [f32; 3]) -> Self {
        Self {
            point,
            normal,
            ..Default::default()
        }
    }
}

/// Which per-point channel a [`Field`] reads, and so which one a model has to
/// have before that field can be painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Intensity,
    Class,
}

/// One point, as both renderers see it.
///
/// A struct rather than four arguments because the two scalar channels are new
/// and every caller of `tint_at` would otherwise have to remember which float is
/// which: the shader takes them from the instance buffer's attributes, the CPU
/// rasteriser from the model's own arrays, and the two must agree slot for slot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub point: [f32; 3],
    /// The geometry's own normal — not the one flipped to face the camera.
    pub normal: [f32; 3],
    /// Raw, as the file wrote it: unscaled, because the range is the model's.
    pub intensity: f32,
    /// The class number as a float, which is what the shader attribute is.
    pub class: f32,
}

impl Sample {
    /// One vertex of a model, with its channels.
    ///
    /// The single place a position, a normal and the two scanner attributes are
    /// read together, so the CPU rasteriser cannot pick a channel up differently
    /// from the instance buffer the GPU reads: both ask for the entry at the
    /// vertex's own index, and both fall back to zero when the model has no such
    /// channel — which the field's range already makes harmless, because a model
    /// without one cannot have that field selected.
    pub fn of(mesh: &super::formats::types::Mesh, index: usize, normal: [f32; 3]) -> Self {
        // Two lookups rather than one helper: the channels are different types
        // (`f32` and `u8`), and a closure generic over both costs more to read
        // than the four lines it would save.
        let fields = mesh.fields.as_deref();
        let intensity = fields
            .and_then(|fields| fields.intensities.get(index))
            .copied()
            .unwrap_or(0.0);
        let class = fields
            .and_then(|fields| fields.classes.get(index))
            .copied()
            .unwrap_or(0);
        Self {
            point: mesh.positions[index],
            normal,
            intensity,
            class: class as f32,
        }
    }
}

/// The ranges a look is measured against: the bounding box, plus whatever
/// scalar channels this particular model carries.
#[derive(Clone, Copy, Debug)]
pub struct FieldData<'a> {
    pub bounds: &'a Bounds,
    /// The intensity range, or `None` for a model with no such channel.
    pub intensities: Option<(f32, f32)>,
    /// The class count — highest class plus one — or `None` without the channel.
    pub classes: Option<usize>,
}
impl<'a> FieldData<'a> {
    /// Geometry only: the ranges of the three fields a model always has, and no
    /// channels for the two it may lack. Useful to a caller that has a bounding
    /// box and nothing else — the tests, above all.
    pub fn geometry(bounds: &'a Bounds) -> Self {
        Self {
            bounds,
            intensities: None,
            classes: None,
        }
    }
}

/// One model's geometry plus its scalar channels, in the shape [`FieldData`]
/// wants. Kept next to the renderers' shared entry point so a caller cannot ask
/// for a look over a half-read model.
pub fn field_data(mesh: &super::formats::types::Mesh) -> FieldData<'_> {
    FieldData {
        bounds: &mesh.bounds,
        intensities: mesh.intensity_range(),
        classes: mesh.class_count(),
    }
}

/// A stop table resolved for use: built-in anchors or a user-picked pair,
/// copied into a fixed-size array so nothing has to stay borrowed while it is
/// read — which is what lets a custom pair ride the same uniform as a preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ramp {
    stops: [ColorStop; RAMP_STOPS],
    count: usize,
    categorical: bool,
}

impl Default for Ramp {
    fn default() -> Self {
        Self::from_stops(&[], false)
    }
}

impl Ramp {
    /// The anchors cleaned up for use: sorted, the ends pinned to 0 and 1, a
    /// duplicate position dropped, and nothing past what a uniform carries.
    ///
    /// This is `ccColorScale::update`'s validation — sort, then insist the
    /// boundaries are `[0.0-1.0]`, then paint black if there is nothing to
    /// interpolate — done at the point a user's anchor list becomes a ramp, so
    /// no caller has to repeat it.
    pub fn custom(stops: &[ColorStop], categorical: bool) -> Self {
        let mut sorted = stops.to_vec();
        sorted.sort_by(|a, b| a.at.total_cmp(&b.at));
        sorted.dedup_by(|a, b| (a.at - b.at).abs() < 1e-6);
        let mut kept = Vec::with_capacity(sorted.len().min(RAMP_STOPS));
        for stop in sorted {
            if kept.len() == RAMP_STOPS {
                break;
            }
            // Clamping as it goes, so one out-of-range anchor (a hand-edited
            // config, a click a pixel off the end) cannot shift every later one.
            kept.push(ColorStop {
                at: stop.at.clamp(0.0, 1.0),
                ..stop
            });
        }
        if let Some(first) = kept.first_mut() {
            first.at = 0.0;
        }
        if let Some(last) = kept.last_mut() {
            last.at = 1.0;
        }
        Self::from_stops(&kept, categorical)
    }

    /// The pair a new user scale starts from: black to white, as CloudCompare's
    /// two colour buttons do.
    pub fn defaults() -> Self {
        Self::from_stops(
            &[
                stop(0.0, DEFAULT_COLOUR_LOW),
                stop(1.0, DEFAULT_COLOUR_HIGH),
            ],
            false,
        )
    }

    fn from_stops(stops: &[ColorStop], categorical: bool) -> Self {
        let mut filled = [stop(0.0, [0, 0, 0]); RAMP_STOPS];
        let count = stops.len().min(RAMP_STOPS);
        filled[..count].copy_from_slice(&stops[..count]);
        Self {
            stops: filled,
            count,
            categorical,
        }
    }

    /// Whether this ramp's anchors are bins. Read by the legend and by the
    /// uniform, which is where the shader learns to stop interpolating.
    pub fn categorical(&self) -> bool {
        self.categorical
    }

    /// The live anchors, ascending by position. The editor reads them back out
    /// of a [`Scale::Custom`] to redraw its markers.
    pub fn stops(&self) -> &[ColorStop] {
        &self.stops[..self.count]
    }

    /// The colour at position `t`, interpolating between the stops around it and
    /// clamped outside the ends.
    ///
    /// The mirror of the shader's `ramp_color`, and of `ccColorScale::update`'s
    /// interval walk.
    pub fn color_at(&self, t: f32) -> [f32; 3] {
        let stops = self.stops();
        if stops.len() < 2 {
            // CloudCompare's "I saw an invalid scale and I want it painted
            // black".
            return [0.0; 3];
        }
        let x = t.clamp(0.0, 1.0);
        if self.categorical {
            // A bin, not a position: the class number picks the anchor and
            // nothing is mixed. This is what CC's epsilon-spaced anchor pairs
            // achieve by other means, and it is why the classes do not bleed
            // into one another at a boundary.
            let index = ((x * stops.len() as f32) as usize).min(stops.len() - 1);
            let rgb = stops[index].rgb;
            return [
                rgb[0] as f32 / 255.0,
                rgb[1] as f32 / 255.0,
                rgb[2] as f32 / 255.0,
            ];
        }
        let mut interval = 0usize;
        while interval + 2 < stops.len() && stops[interval + 1].at < x {
            interval += 1;
        }
        let before = &stops[interval];
        let after = &stops[interval + 1];
        let span = after.at - before.at;
        let alpha = if span > 0.0 {
            (x - before.at) / span
        } else {
            0.0
        };
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * alpha) / 255.0;
        [
            mix(before.rgb[0], after.rgb[0]),
            mix(before.rgb[1], after.rgb[1]),
            mix(before.rgb[2], after.rgb[2]),
        ]
    }

    /// The ramp drawn as `steps` discrete colours, start to end. A categorical
    /// ramp is drawn as one cell per bin, because blending two class colours is
    /// a colour no class has.
    pub fn gradient(&self, steps: usize) -> Vec<[u8; 3]> {
        let steps = steps.max(1);
        (0..steps)
            .map(|index| {
                let rgb = self.color_at(index as f32 / (steps - 1).max(1) as f32);
                [
                    (rgb[0] * 255.0).round() as u8,
                    (rgb[1] * 255.0).round() as u8,
                    (rgb[2] * 255.0).round() as u8,
                ]
            })
            .collect()
    }

    /// The uniform rows: `rgb` plus `w` = position, zero-padded past the live
    /// count.
    fn pack(&self) -> [[f32; 4]; RAMP_STOPS] {
        let mut rows = [[0.0f32; 4]; RAMP_STOPS];
        for (index, stop) in self.stops().iter().enumerate() {
            rows[index] = [
                stop.rgb[0] as f32 / 255.0,
                stop.rgb[1] as f32 / 255.0,
                stop.rgb[2] as f32 / 255.0,
                stop.at,
            ];
        }
        rows
    }
}

/// Whether a model is painted by a field at all, and how.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeightMode {
    /// The file's own colours, or the flat material.
    #[default]
    Off,
    /// One continuous colour scale over the field's range.
    Ramp,
    /// Sinusoidal RGB stripes: CloudCompare's `setRGBColorByBanding`.
    Bands,
}

impl HeightMode {
    /// The number the shader compares against, which is also the order the
    /// panel's three choices are listed in.
    pub fn index(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Ramp => 1,
            Self::Bands => 2,
        }
    }

    pub fn from_index(index: u8) -> Self {
        match index {
            1 => Self::Ramp,
            2 => Self::Bands,
            _ => Self::Off,
        }
    }

    /// The name the config stores. A word rather than an index so a hand-edited
    /// config says what it means.
    pub fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ramp => "ramp",
            Self::Bands => "bands",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "ramp" => Self::Ramp,
            "bands" => Self::Bands,
            _ => Self::Off,
        }
    }
}

/// Which per-point value the colour runs along. CloudCompare colours its scalar
/// fields the way it colours height — one normalised value through one colour
/// scale — so these are the first three of its fields rather than a
/// height-only feature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Field {
    /// The coordinate along the chosen axis: what CloudCompare colours by
    /// height, given the dimension.
    #[default]
    Height,
    /// How far a point's normal leans away from that axis, in degrees: the dip
    /// of the surface there, 0 flat and 90 vertical.
    Slope,
    /// The bearing of that lean, in degrees: the dip direction, which
    /// CloudCompare colours with a hue scale that repeats twice round 360.
    Aspect,
    /// The scanner's own return strength, as the file wrote it. Normalised
    /// against the range the cloud actually spans, which is why a 12-bit and a
    /// 16-bit intensity both fill the scale.
    Intensity,
    /// The ASPRS class number, coloured by bin: an index into a palette rather
    /// than a value to interpolate. See [`CATEGORICAL_SCALES`].
    Class,
}

impl Field {
    pub fn index(self) -> u8 {
        match self {
            Self::Height => 0,
            Self::Slope => 1,
            Self::Aspect => 2,
            Self::Intensity => 3,
            Self::Class => 4,
        }
    }

    pub fn from_index(index: u8) -> Self {
        match index {
            1 => Self::Slope,
            2 => Self::Aspect,
            3 => Self::Intensity,
            4 => Self::Class,
            _ => Self::Height,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Height => "height",
            Self::Slope => "slope",
            Self::Aspect => "aspect",
            Self::Intensity => "intensity",
            Self::Class => "class",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "slope" => Self::Slope,
            "aspect" => Self::Aspect,
            "intensity" => Self::Intensity,
            "class" => Self::Class,
            _ => Self::Height,
        }
    }

    /// Which per-point channel this field reads, if not the geometry: the two
    /// scanner attributes live in [`CloudFields`] rather than in the position or
    /// the normal, so they are the reason a model can be missing a field.
    ///
    /// [`CloudFields`]: super::formats::types::CloudFields
    pub fn channel(self) -> Option<Channel> {
        match self {
            Self::Intensity => Some(Channel::Intensity),
            Self::Class => Some(Channel::Class),
            _ => None,
        }
    }

    /// The range the value is normalised against when the field defines one
    /// itself — CloudCompare's `setAbsolute` — which is what lets the legend say
    /// 0° to 90° whatever the model turns out to be. `None` leaves the range to
    /// the geometry, as [`Field::Height`] does.
    pub fn absolute_range(self) -> Option<(f32, f32)> {
        match self {
            Self::Height | Self::Intensity | Self::Class => None,
            Self::Slope => Some((0.0, 90.0)),
            Self::Aspect => Some((0.0, 360.0)),
        }
    }

    /// The values CloudCompare puts its colour bar's labels at, in this field's
    /// own units: `ccColorScale::customLabels`, which the dip scales set to
    /// 0/30/60/90 and 0/90/180/270. `None` leaves the legend to pick round
    /// numbers over the model's own range, as [`nice_ticks`].
    pub fn labels(self) -> Option<&'static [f32]> {
        match self {
            Self::Height | Self::Intensity | Self::Class => None,
            Self::Slope => Some(&[0.0, 30.0, 60.0, 90.0]),
            Self::Aspect => Some(&[0.0, 90.0, 180.0, 270.0, 360.0]),
        }
    }

    /// Suffix for the legend's numbers. Height is in whatever units the file
    /// carries, which are nobody's business but the model's.
    pub fn unit(self) -> Option<&'static str> {
        match self {
            Self::Height | Self::Intensity | Self::Class => None,
            Self::Slope | Self::Aspect => Some("°"),
        }
    }

    /// Whether a legend for this field labels whole numbers: a class number is
    /// a name, and `7.5` is not a class.
    pub fn is_integral(self) -> bool {
        matches!(self, Self::Class)
    }
}

/// Everything the user picks about the look, before it is measured against a
/// model.
#[derive(Clone, Debug, PartialEq)]
pub struct HeightLook {
    pub mode: HeightMode,
    pub field: Field,
    /// Which model-space axis the field is read against: 0 = X, 1 = Y, 2 = Z.
    /// For [`Field::Height`] it is the axis the elevations sit on; for the two
    /// normal-based fields it is the axis that counts as up. Y is the viewport's
    /// up axis, so that is the default, and a cloud scanned in a Z-up tool wants
    /// Z in both cases.
    pub axis: usize,
    pub scale: Scale,
    /// Field units per cycle, for [`HeightMode::Bands`].
    pub period: f32,
}

impl Default for HeightLook {
    fn default() -> Self {
        Self {
            mode: HeightMode::Off,
            field: Field::Height,
            axis: 1,
            scale: Scale::default(),
            period: DEFAULT_PERIOD,
        }
    }
}

impl HeightLook {
    /// Measure this look against one model.
    ///
    /// The height range is the caller's, which for the viewport is the *scene*'s
    /// bounds rather than the frame's: a streamed cloud hands back a different
    /// subset every frame, and re-normalising against those would make its
    /// colours shift while it loads.
    pub fn resolve(&self, data: &FieldData) -> HeightField {
        let axis = self.axis.min(2);
        let bounds = data.bounds;
        let ramp = self.scale.ramp();
        // A categorical scale's anchors *are* its domain: class 6 is the
        // building yellow whether or not this model contains classes 2 to 5.
        // That is CloudCompare's `ASPRS` range, which it sets absolute — 0 to
        // 22.999, one plateau per class — rather than normalising over the
        // classes present, and it is the reason a partial cloud still reads
        // right.
        let (min, max) = if ramp.categorical() && self.field == Field::Class {
            (0.0, ramp.stops().len() as f32)
        } else {
            match self.field.absolute_range().or_else(|| match self.field {
                // A model with no channel has no range either, which the inverse
                // below turns into "everyone gets the first colour" — the same
                // answer a flat model gets, and the panel keeps the field out of
                // reach in the first place.
                Field::Intensity => data.intensities,
                // A class number counts whole units, so the range is the count
                // rather than the highest class: class `i` of `n` sits at
                // `i / n`, the start of its own bin.
                Field::Class => data.classes.map(|count| (0.0, count as f32)),
                _ => None,
            }) {
                Some(range) => range,
                None => {
                    let (min, max) = (bounds.min[axis], bounds.max[axis]);
                    // CloudCompare's flat-cloud branch: collapse the range, which
                    // the inverse below turns into "everyone is at position 0".
                    if max - min <= FLAT_SPAN {
                        (min, min)
                    } else {
                        (min, max)
                    }
                }
            }
        };
        HeightField {
            mode: self.mode,
            field: self.field,
            axis,
            ramp,
            min,
            max,
            // A model with no span gets `0` rather than the inverse of nothing,
            // so every point reads as position 0 of the scale — CloudCompare's
            // flat-cloud branch.
            inv_span: if max - min > FLAT_SPAN {
                1.0 / (max - min)
            } else {
                0.0
            },
            // Radians of banding cycle per field unit. The stripes are a ruler
            // over the value rather than a share of the range, so a model with
            // no height changes nothing about them.
            band_step: std::f32::consts::TAU / self.period.clamp(MIN_PERIOD, MAX_PERIOD),
        }
    }
}

/// A [`HeightLook`] measured against one model, with nothing left to work out
/// per point.
///
/// What both renderers consume each frame: the CPU rasteriser through
/// [`HeightField::tint_at`], the shader through [`HeightField::uniforms`], and
/// the legend through [`HeightField::range`] and [`HeightField::legend_steps`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeightField {
    mode: HeightMode,
    field: Field,
    axis: usize,
    ramp: Ramp,
    min: f32,
    max: f32,
    inv_span: f32,
    band_step: f32,
}

/// Nothing tinted, which is what a caller that never asked gets.
impl Default for HeightField {
    fn default() -> Self {
        Self {
            mode: HeightMode::Off,
            field: Field::Height,
            axis: 1,
            ramp: Ramp::default(),
            min: 0.0,
            max: 0.0,
            inv_span: 0.0,
            band_step: 0.0,
        }
    }
}

impl HeightField {
    pub fn mode(&self) -> HeightMode {
        self.mode
    }

    pub fn field(&self) -> Field {
        self.field
    }

    pub fn axis(&self) -> usize {
        self.axis
    }

    /// The value range the legend runs over.
    pub fn range(&self) -> (f32, f32) {
        (self.min, self.max)
    }

    pub fn unit(&self) -> Option<&'static str> {
        self.field.unit()
    }

    /// The field's value at one point.
    ///
    /// `normal` must be the geometry's own — the raw attribute rather than the
    /// one flipped to face the camera for two-sided shading — because that is
    /// what the shader's vertex stage sees, and dip direction reads a 180°
    /// difference out of it.
    pub fn value_at(&self, sample: &Sample) -> f32 {
        let point = sample.point;
        let normal = sample.normal;
        match self.field {
            Field::Height => point[self.axis],
            Field::Intensity => sample.intensity,
            Field::Class => sample.class,
            // The angle away from the axis, folded to 0..=90: a surface facing
            // straight down the axis is as flat as one facing up, which is what
            // a dip means.
            Field::Slope => {
                let length = vector_length(normal);
                if length < DEGENERATE_NORMAL {
                    return 0.0;
                }
                let along = (component(normal, self.axis) / length)
                    .abs()
                    .clamp(0.0, 1.0);
                along.acos().to_degrees()
            }
            // The bearing of the lean. The shader's `fract` and this
            // `rem_euclid` are the same fold into one turn.
            Field::Aspect => {
                let (a, b) = horizontal(self.axis, normal);
                (a.atan2(b).to_degrees() / 360.0).rem_euclid(1.0) * 360.0
            }
        }
    }

    /// The colour this point gets, or `None` when the look is off and the
    /// caller should keep the colour it already had.
    ///
    /// This and `surface_color` in `gpu3d.wgsl` are one function written twice;
    /// they must stay in step, because the CPU rasteriser's output is a
    /// thumbnail and the shader's is the frame the user compares it against.
    pub fn tint_at(&self, sample: &Sample) -> Option<[f32; 3]> {
        let value = self.value_at(sample);
        match self.mode {
            HeightMode::Off => None,
            HeightMode::Ramp => Some(self.ramp.color_at((value - self.min) * self.inv_span)),
            // CloudCompare's `setRGBColorByBanding`, on the field's own value.
            HeightMode::Bands => Some(band_color(value, self.band_step)),
        }
    }

    /// Pack for the shader. The one place the two renderers agree on the
    /// encoding.
    pub fn uniforms(&self) -> HeightUniforms {
        HeightUniforms {
            coloring: [
                self.mode.index() as f32,
                self.axis as f32,
                self.min,
                self.inv_span,
            ],
            params: [
                self.band_step,
                self.ramp.stops().len() as f32,
                self.field.index() as f32,
                // The one flag the shader needs that no float can be inferred
                // from: bins do not interpolate.
                if self.ramp.categorical() { 1.0 } else { 0.0 },
            ],
            ramp: self.ramp.pack(),
        }
    }

    /// The colours this field paints with, sampled at evenly spaced values and
    /// returned highest first, so a caller can lay them out as a vertical bar.
    ///
    /// A legend's only job is to say what the picture means, so this reads the
    /// same [`HeightField::tint_at`] the two renderers do rather than sampling
    /// the scale directly: banded, it therefore shows however many cycles of the
    /// stripe fall inside the range.
    pub fn legend_steps(&self, steps: usize) -> Vec<[f32; 3]> {
        let steps = steps.max(1);
        (0..steps)
            .rev()
            .map(|index| {
                let t = (index as f32 + 0.5) / steps as f32;
                let value = self.min + (self.max - self.min) * t;
                self.tint_at(&self.sample_at(value)).unwrap_or([0.0; 3])
            })
            .collect()
    }

    /// A sample whose value on this field is exactly `value`, so the legend can
    /// ask the same [`HeightField::tint_at`] everything else does.
    fn sample_at(&self, value: f32) -> Sample {
        let mut point = [0.0f32; 3];
        let mut normal = [0.0f32; 3];
        // The channel fields *are* the sample for the two scanner attributes;
        // whatever the renderer would ignore for the others stays zero.
        let mut intensity = 0.0;
        let mut class = 0.0;
        match self.field {
            Field::Height => point[self.axis] = value,
            Field::Intensity => intensity = value,
            Field::Class => class = value,
            // Lean away from the axis by `value`: the normal sits in the plane
            // that contains the axis and one horizontal direction.
            Field::Slope => {
                let radians = value.to_radians();
                normal[self.axis] = radians.cos();
                normal[(self.axis + 1) % 3] = radians.sin();
            }
            // Swing round the compass by `value`, staying in the plane the axis
            // leaves: the same pair `horizontal` reads back out.
            Field::Aspect => {
                // In the order `horizontal` reads back: the second of the pair
                // is the reference direction the bearing turns away from.
                let (first, second) = (value.to_radians().sin(), value.to_radians().cos());
                normal = match self.axis {
                    0 => [0.0, first, second],
                    1 => [first, 0.0, second],
                    _ => [first, second, 0.0],
                };
            }
        }
        Sample {
            point,
            normal,
            intensity,
            class,
        }
    }
}

/// The uniform payload of a [`HeightField`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeightUniforms {
    /// `x` = mode, `y` = axis, `z` = the range floor, `w` = 1 / the range span.
    pub coloring: [f32; 4],
    /// `x` = banding radians per field unit, `y` = how many `ramp` entries are
    /// live, `z` = the field.
    pub params: [f32; 4],
    /// The scale's stops, `rgb` plus `w` = position, ascending, zero-padded.
    pub ramp: [[f32; 4]; RAMP_STOPS],
}

/// Everything zero, which the shader reads as "nothing is tinted".
impl Default for HeightUniforms {
    fn default() -> Self {
        Self {
            coloring: [0.0; 4],
            params: [0.0; 4],
            ramp: [[0.0; 4]; RAMP_STOPS],
        }
    }
}

/// The colour the banding cycle gives at a value: CloudCompare's
/// `setRGBColorByBanding` line for line, three sines a third of a cycle apart.
///
/// Because the three phases are evenly spaced around the circle their sines sum
/// to zero at every value, so the pattern keeps a constant brightness and only
/// turns hue — which is what lets it read as a scale rather than as shading.
fn band_color(value: f32, step: f32) -> [f32; 3] {
    let z = step * value;
    let wave = |phase: f32| (z + phase).sin() * 0.5 + 0.5;
    [wave(0.0), wave(BAND_PHASE_2), wave(BAND_PHASE_3)]
}

fn vector_length(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn component(v: [f32; 3], index: usize) -> f32 {
    v[index.min(2)]
}

/// The two components that are not the axis, in a fixed order, so the two
/// renderers share one convention: the bearing is `atan2(first, second)`, which
/// for the viewport's Y axis reads as an azimuth from +Z turning towards +X.
/// CloudCompare's dip direction is the same idea with whichever end of the
/// compass the file's own orientation happens to put on its up-axis' north.
fn horizontal(axis: usize, v: [f32; 3]) -> (f32, f32) {
    match axis {
        0 => (v[1], v[2]),
        1 => (v[0], v[2]),
        _ => (v[0], v[1]),
    }
}

/// Round numbers inside a range, for a colour bar's intermediate labels.
///
/// The step comes out of `1 / 2 / 2.5 / 5` times a power of ten, which is the
/// family that gives labels a person can read at a glance — the alternative,
/// dividing the range into equal parts, produces values like `1874.62` for a
/// terrain that is really 0 to 6248.74 units tall. Values outside the range, and
/// the range's own ends, are left out: the bar labels those already.
pub fn nice_ticks(min: f32, max: f32, wanted: usize) -> Vec<f32> {
    let span = max - min;
    if span <= 0.0 || wanted == 0 {
        return Vec::new();
    }
    // The decade the rough step falls in, then the family member nearest it.
    let rough = span / (wanted + 1) as f32;
    let decade = 10f32.powf(rough.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|factor| factor * decade)
        .find(|candidate| *candidate >= rough)
        .unwrap_or(10.0 * decade);
    let mut ticks = Vec::new();
    let mut value = (min / step).ceil() * step;
    while value <= max {
        // Skip an end that the range lands on: the bar already says it, and two
        // labels eleven pixels apart is one label too many.
        if value > min + step * 0.001 && value < max - step * 0.001 {
            // Against zero a step's worth of float noise is a long way; this is
            // what keeps `-0.0` and a hair off `0` from becoming a label.
            ticks.push((value * 1e6).round() / 1e6);
        }
        value += step;
    }
    ticks
}

/// The look in the shape a library stores: names and numbers only, so a scale
/// that is edited later is edited everywhere it is used, and one that is
/// deleted costs a fallback rather than a broken row.
///
/// The [`Appearance`] of a 3D viewport — same `NULL` means default, same
/// "drop a value this build cannot read rather than fail to open it".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredLook {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub field: String,
    /// 0 = X, 1 = Y, 2 = Z.
    #[serde(default)]
    pub axis: u8,
    #[serde(default)]
    pub scale: String,
    #[serde(default)]
    pub period: f32,
}

impl StoredLook {
    /// The stored text, or `None` for the default look so a cleared row goes
    /// back to `NULL` rather than holding an empty object.
    pub fn to_storage(&self) -> Option<String> {
        if self == &Self::default() {
            return None;
        }
        serde_json::to_string(self).ok()
    }

    /// Read a stored value. Unreadable, or naming a scale this build does not
    /// have, resolves to the default look rather than failing: a model's colours
    /// are the least important thing about it.
    pub fn from_storage(text: Option<&str>) -> Self {
        text.and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or_default()
    }

    /// Resolve against the user's scales: a missing id falls back to the
    /// built-in default, exactly as a config that names an unknown one does.
    pub fn resolve(&self, customs: &[CustomScale]) -> HeightLook {
        let scale = match CustomScale::id_of(&self.scale) {
            Some(id) => customs
                .iter()
                .find(|custom| custom.id == id)
                .map(Scale::custom)
                .unwrap_or_default(),
            None => Scale::Preset(scale_by_id(&self.scale)),
        };
        HeightLook {
            mode: HeightMode::from_key(&self.mode),
            field: Field::from_key(&self.field),
            axis: usize::from(self.axis.min(2)),
            scale,
            period: self.period.clamp(MIN_PERIOD, MAX_PERIOD),
        }
    }
}

impl HeightLook {
    /// The same look, in the shape a row or a config stores.
    pub fn stored(&self) -> StoredLook {
        StoredLook {
            mode: self.mode.key().to_string(),
            field: self.field.key().to_string(),
            axis: self.axis.min(2) as u8,
            scale: self.scale.key(),
            period: self.period.clamp(MIN_PERIOD, MAX_PERIOD),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(min: [f32; 3], max: [f32; 3]) -> Bounds {
        Bounds { min, max }
    }

    /// The one shape every test reaches for: a look over a given range.
    fn field(mode: HeightMode, box_bounds: Bounds) -> HeightField {
        HeightLook {
            mode,
            ..Default::default()
        }
        .resolve(&FieldData::geometry(&box_bounds))
    }

    #[test]
    fn every_scale_is_ordered_and_fits_the_uniform() {
        for scale in COLOR_SCALES {
            assert!(
                scale.stops.len() >= 2,
                "{} has nothing to interpolate",
                scale.id
            );
            assert!(
                scale.stops.len() <= RAMP_STOPS,
                "{} overflows the uniform",
                scale.id
            );
            assert_eq!(scale.stops.first().unwrap().at, 0.0, "{} floor", scale.id);
            // The list of categorical ids is a list of these ids, so it has to
            // agree with the tables it names.
            assert_eq!(
                scale.categorical(),
                CATEGORICAL_SCALES.contains(&scale.id),
                "{} categorical",
                scale.id
            );
            if scale.categorical() {
                // A categorical scale's anchors are the bottoms of their bins, so
                // the last one sits a bin short of the end. Nothing interpolates
                // through them, but the panel lays its markers out along them.
                let bins = scale.stops.len() as f32;
                assert!(
                    (scale.stops.last().unwrap().at - (bins - 1.0) / bins).abs() < 1e-6,
                    "{} last bin",
                    scale.id
                );
            } else {
                assert_eq!(scale.stops.last().unwrap().at, 1.0, "{} ceiling", scale.id);
            }
            for pair in scale.stops.windows(2) {
                assert!(
                    pair[1].at > pair[0].at,
                    "{} stops are not ascending",
                    scale.id
                );
            }
        }
    }

    #[test]
    fn a_scale_endpoints_are_its_first_and_last_colour() {
        let ramp = scale_by_id("bgyr").ramp();
        assert_eq!(ramp.color_at(0.0), [0.0, 0.0, 1.0]);
        assert_eq!(ramp.color_at(1.0), [1.0, 0.0, 0.0]);
        // Outside the ends it clamps rather than wrapping.
        assert_eq!(ramp.color_at(-2.0), [0.0, 0.0, 1.0]);
        assert_eq!(ramp.color_at(3.0), [1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_scale_interpolates_between_its_stops() {
        let ramp = scale_by_id("bwr").ramp();
        assert_eq!(ramp.color_at(0.5), [1.0, 1.0, 1.0]);
        // A quarter of the way up the blue→white leg is halfway along it.
        let quarter = ramp.color_at(0.25);
        assert!((quarter[2] - 1.0).abs() < 1e-3);
        assert!((quarter[0] - 0.5).abs() < 0.01, "{quarter:?}");
    }

    #[test]
    fn an_unknown_scale_id_falls_back_to_the_default() {
        assert_eq!(scale_by_id("nonsense").id, COLOR_SCALES[0].id);
    }

    #[test]
    fn a_custom_scale_is_named_by_reference_and_keeps_its_anchors() {
        let custom = CustomScale {
            id: "7".into(),
            stops: vec![stop(0.0, [10, 20, 30]), stop(1.0, [240, 250, 255])],
        };
        assert_eq!(custom.key(), "custom:7");
        assert_eq!(CustomScale::id_of("custom:7"), Some("7"));
        assert_eq!(CustomScale::id_of("bgyr"), None);
        let scale = Scale::custom(&custom);
        assert_eq!(scale.key(), "custom:7");
        assert_eq!(scale.custom_id(), Some("7"));
        assert_eq!(
            scale.gradient(3),
            vec![[10, 20, 30], [125, 135, 143], [240, 250, 255]]
        );
    }

    #[test]
    fn a_users_anchor_list_is_cleaned_up_before_it_paints() {
        // Out of order, with the ends misplaced: sorted and pinned, which is
        // what `ccColorScale::update` does before it fills its table.
        let ramp = Ramp::custom(
            &[
                stop(0.8, [255, 0, 0]),
                stop(0.0, [0, 0, 0]),
                stop(0.4, [0, 255, 0]),
            ],
            false,
        );
        let at: Vec<f32> = ramp.stops().iter().map(|stop| stop.at).collect();
        assert_eq!(at, vec![0.0, 0.4, 1.0]);
        // A duplicate position is one anchor, not a division by zero later.
        let ramp = Ramp::custom(
            &[
                stop(0.0, [0, 0, 0]),
                stop(0.5, [10, 10, 10]),
                stop(0.5, [20, 20, 20]),
                stop(1.0, [255, 255, 255]),
            ],
            false,
        );
        assert_eq!(ramp.stops().len(), 3);
        // Past the end of the table, and past what a uniform carries.
        let many: Vec<ColorStop> = (0..40)
            .map(|i| stop(i as f32 / 39.0, [i as u8, 0, 0]))
            .collect();
        let ramp = Ramp::custom(&many, false);
        assert_eq!(ramp.stops().len(), RAMP_STOPS);
        assert_eq!(ramp.stops().last().unwrap().at, 1.0);
        // Two anchors is the minimum that can interpolate; one is painted black,
        // as CloudCompare paints an invalid scale.
        assert_eq!(
            Ramp::custom(&[stop(0.3, [9, 9, 9])], false).color_at(0.5),
            [0.0, 0.0, 0.0]
        );
        assert_eq!(Ramp::custom(&[], false).color_at(0.5), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_new_scale_starts_black_to_white() {
        let custom = CustomScale::seed("1".into());
        assert_eq!(custom.id, "1");
        assert_eq!(custom.ramp().stops().len(), 2);
        assert_eq!(custom.ramp().color_at(0.0), [0.0, 0.0, 0.0]);
        assert_eq!(custom.ramp().color_at(1.0), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn a_stored_look_names_things_it_can_find_again() {
        let look = HeightLook {
            mode: HeightMode::Bands,
            field: Field::Aspect,
            axis: 2,
            scale: Scale::custom(&CustomScale {
                id: "3".into(),
                stops: vec![stop(0.0, [1, 2, 3]), stop(1.0, [4, 5, 6])],
            }),
            period: 12.0,
        };
        let stored = look.stored();
        assert_eq!(stored.mode, "bands");
        assert_eq!(stored.field, "aspect");
        assert_eq!(stored.axis, 2);
        assert_eq!(stored.scale, "custom:3");
        let customs = vec![CustomScale {
            id: "3".into(),
            stops: vec![stop(0.0, [1, 2, 3]), stop(1.0, [4, 5, 6])],
        }];
        assert_eq!(stored.resolve(&customs), look);
        // The round trip through the row's text is the whole storage contract.
        let text = stored.to_storage().expect("not the default look");
        assert_eq!(
            StoredLook::from_storage(Some(&text)).resolve(&customs),
            look
        );
        // A row that names a scale this build has lost paints the default
        // scale; a row that cannot be read at all is the default look.
        let orphan = StoredLook {
            scale: "custom:gone".into(),
            ..Default::default()
        }
        .resolve(&customs);
        assert_eq!(orphan.scale.key(), COLOR_SCALES[0].id);
        assert_eq!(
            StoredLook::from_storage(Some("{not json")),
            StoredLook::default()
        );
        assert_eq!(StoredLook::default().to_storage(), None);
    }

    #[test]
    fn height_is_normalised_to_the_models_own_range() {
        let ramp = scale_by_id("bgyr").ramp();
        // The same two colours whatever the units, because the range is the
        // model's own: a metre-tall and a kilometre-tall model both run the
        // scale end to end.
        for (min, max) in [(0.0, 1.0), (100.0, 101.0), (-5_000.0, 5_500.0)] {
            let tinted = field(HeightMode::Ramp, bounds([0.0, min, 0.0], [10.0, max, 10.0]));
            assert_eq!(
                tinted.tint_at(&Sample::at([0.0, min, 0.0])),
                Some(ramp.color_at(0.0))
            );
            assert_eq!(
                tinted.tint_at(&Sample::at([0.0, max, 0.0])),
                Some(ramp.color_at(1.0))
            );
        }
    }

    #[test]
    fn the_chosen_axis_is_the_height() {
        let box_bounds = bounds([0.0, 0.0, 0.0], [1.0, 5.0, 10.0]);
        let point = [1.0, 3.0, 10.0];
        let ramp = scale_by_id("bgyr").ramp();
        for axis in 0..3 {
            let look = HeightLook {
                mode: HeightMode::Ramp,
                axis,
                scale: Scale::Preset(&COLOR_SCALES[0]),
                ..Default::default()
            };
            let expected = point[axis] / box_bounds.max[axis];
            assert_eq!(
                look.resolve(&FieldData::geometry(&box_bounds))
                    .tint_at(&Sample::at(point)),
                Some(ramp.color_at(expected)),
                "axis {axis}"
            );
        }
    }

    #[test]
    fn slope_reads_degrees_off_the_normal_and_folds_both_ways() {
        let tinted = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Slope,
            ..Default::default()
        }
        .resolve(&FieldData::geometry(&Bounds::default()));
        assert_eq!(tinted.range(), (0.0, 90.0));
        assert_eq!(tinted.unit(), Some("°"));
        // Along the axis is flat; across it is vertical, either way round.
        let slope_at = |normal: [f32; 3]| tinted.value_at(&Sample::surface([0.0; 3], normal));
        assert!(slope_at([0.0, 1.0, 0.0]).abs() < 1e-5);
        assert!((slope_at([1.0, 0.0, 0.0]) - 90.0).abs() < 1e-4);
        assert!((slope_at([-1.0, 0.0, 0.0]) - 90.0).abs() < 1e-4);
        // 45° in between, and a degenerate normal is flat rather than NaN.
        let halfway = slope_at([0.707, 0.707, 0.0]);
        assert!((halfway - 45.0).abs() < 0.2, "{halfway}");
        assert!(slope_at([0.0, 0.0, 0.0]).is_finite());
    }

    #[test]
    fn aspect_is_a_bearing_that_wraps_once_round_the_compass() {
        let tinted = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Aspect,
            ..Default::default()
        }
        .resolve(&FieldData::geometry(&Bounds::default()));
        assert_eq!(tinted.range(), (0.0, 360.0));
        let at = |n: [f32; 3]| tinted.value_at(&Sample::surface([0.0; 3], n));
        // The two horizontal directions the Y axis leaves, read in the order
        // `horizontal` promises: atan2(x, z). So +Z is where the bearing starts,
        // and it turns towards +X.
        assert!(at([0.0, 0.0, 1.0]).abs() < 1e-4);
        assert!((at([1.0, 0.0, 0.0]) - 90.0).abs() < 1e-4);
        assert!((at([0.0, 0.0, -1.0]) - 180.0).abs() < 1e-4);
        assert!((at([-1.0, 0.0, 0.0]) - 270.0).abs() < 1e-4);
        // Never outside one turn, however the normal points.
        for index in 0..24 {
            let angle = index as f32 * 0.26;
            let value = at([angle.cos(), 0.3, angle.sin()]);
            assert!((0.0..360.0).contains(&value), "{value}");
        }
    }

    #[test]
    fn an_absolute_field_ignores_the_models_bounds() {
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Slope,
            ..Default::default()
        };
        // A model with no height at all still gets a full dip range, because
        // 30° of dip is 30° wherever it is.
        let flat = look.resolve(&FieldData::geometry(&bounds(
            [0.0, 7.5, 0.0],
            [10.0, 7.5, 10.0],
        )));
        assert_eq!(flat.range(), (0.0, 90.0));
        assert_eq!(
            flat.tint_at(&Sample::surface([1.0, 7.5, 2.0], [0.0, 0.0, 1.0])),
            look.resolve(&FieldData::geometry(&Bounds::default()))
                .tint_at(&Sample::surface([0.0; 3], [0.0, 0.0, 1.0]))
        );
    }

    #[test]
    fn off_keeps_whatever_colour_the_caller_had() {
        let tinted = field(HeightMode::Off, bounds([0.0, 0.0, 0.0], [1.0, 10.0, 1.0]));
        assert_eq!(tinted.tint_at(&Sample::at([0.0, 5.0, 0.0])), None);
        // Which is also what the uniform says, so the shader agrees.
        assert_eq!(
            tinted.uniforms().coloring[0],
            HeightMode::Off.index() as f32
        );
    }

    #[test]
    fn a_flat_model_takes_the_first_colour_instead_of_dividing_by_zero() {
        let flat = bounds([0.0, 7.5, 0.0], [10.0, 7.5, 10.0]);
        let ramp = field(HeightMode::Ramp, flat);
        assert_eq!(
            ramp.tint_at(&Sample::at([1.0, 7.5, 2.0])),
            Some(ramp.ramp.color_at(0.0))
        );
        assert_eq!(ramp.uniforms().coloring[3], 0.0);
        // Banding is indifferent to the range, flat or otherwise: a period over
        // the value divides by nothing.
        let banded = field(HeightMode::Bands, flat);
        assert!(
            banded
                .tint_at(&Sample::at([1.0, 7.5, 2.0]))
                .unwrap()
                .iter()
                .all(|value| value.is_finite())
        );
    }

    #[test]
    fn a_banding_cycle_repeats_every_period() {
        let look = HeightLook {
            mode: HeightMode::Bands,
            period: 10.0,
            ..Default::default()
        };
        let tinted = look.resolve(&FieldData::geometry(&bounds(
            [0.0, 0.0, 0.0],
            [1.0, 100.0, 1.0],
        )));
        let at = |height: f32| tinted.tint_at(&Sample::at([0.0, height, 0.0])).unwrap();
        // One full period lands back where the last one started...
        let (here, next_cycle) = (at(3.0), at(13.0));
        for channel in 0..3 {
            assert!(
                (here[channel] - next_cycle[channel]).abs() < 1e-4,
                "{here:?} vs {next_cycle:?}"
            );
        }
        // ...and a quarter of the way through it does not.
        assert_ne!(at(3.0), at(5.5));
    }

    #[test]
    fn banding_holds_its_brightness_and_measures_from_the_value() {
        let look = HeightLook {
            mode: HeightMode::Bands,
            period: 7.0,
            ..Default::default()
        };
        let tinted = look.resolve(&FieldData::geometry(&bounds(
            [0.0, 0.0, 0.0],
            [1.0, 100.0, 1.0],
        )));
        for height in 0..=100 {
            let rgb = tinted
                .tint_at(&Sample::at([0.0, height as f32, 0.0]))
                .unwrap();
            // Three evenly spaced phases sum to a constant, which is what keeps
            // the stripes a colour pattern rather than a shading one.
            assert!(
                (rgb[0] + rgb[1] + rgb[2] - 1.5).abs() < 1e-5,
                "height {height}: {rgb:?}"
            );
            assert!(rgb.iter().all(|value| (0.0..=1.0).contains(value)));
        }
        // The stripes are a ruler along the value, so they follow the coordinate
        // rather than the model's floor: the same height paints the same way
        // wherever the model happens to sit.
        let lifted = look.resolve(&FieldData::geometry(&bounds(
            [0.0, 100.0, 0.0],
            [1.0, 200.0, 1.0],
        )));
        assert_eq!(
            lifted.tint_at(&Sample::at([0.0, 33.0, 0.0])),
            tinted.tint_at(&Sample::at([0.0, 33.0, 0.0]))
        );
    }

    /// The field's ranges, for a cloud that carries both scanner attributes.
    fn channels<'a>(
        box_bounds: &'a Bounds,
        classes: Option<usize>,
        intensities: Option<(f32, f32)>,
    ) -> FieldData<'a> {
        FieldData {
            bounds: box_bounds,
            intensities,
            classes,
        }
    }

    /// The ASPRS palette is a key rather than a gradient: class 2 is the ground
    /// brown whether or not this cloud holds classes 3 to 5. CloudCompare gets
    /// the same answer by setting that scale's range absolute, which is why the
    /// palette — not the data — is what bounds a categorical field.
    #[test]
    fn a_class_palette_paints_by_number() {
        let box_bounds = bounds([0.0, 0.0, 0.0], [1.0, 10.0, 1.0]);
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Class,
            scale: Scale::Preset(scale_by_id("asprs")),
            ..Default::default()
        };
        // Seven classes is the count, and the highest one painted is 6.
        let tinted = look.resolve(&channels(&box_bounds, Some(7), None));
        let class = |number: u8| Sample {
            class: number as f32,
            ..Default::default()
        };
        assert_eq!(
            tinted.tint_at(&class(2)),
            Some([166.0 / 255.0, 116.0 / 255.0, 4.0 / 255.0]),
            "ground"
        );
        assert_eq!(tinted.tint_at(&class(6)), Some([1.0, 1.0, 0.0]), "building");
        // And every class of the palette keeps the colour the table gives it:
        // the bin number is the class number, so the round trip through the
        // range costs nothing. (Small whole numbers make that exact — see the
        // boundary test below.)
        for number in 0u8..23 {
            let rgb = scale_by_id("asprs").stops[number as usize].rgb;
            assert_eq!(
                tinted.tint_at(&class(number)),
                Some([
                    rgb[0] as f32 / 255.0,
                    rgb[1] as f32 / 255.0,
                    rgb[2] as f32 / 255.0,
                ]),
                "class {number}"
            );
        }
        // A class the file could not have — 200, say, past the palette's end —
        // clamps to its last bin rather than wrapping into the first.
        assert_eq!(
            tinted.tint_at(&class(200)),
            Some(scale_by_id("asprs").ramp().color_at(1.0))
        );
    }

    /// Bins do not interpolate, which is the whole difference between a
    /// categorical scale and the fifteen that are not.
    #[test]
    fn a_bin_boundary_belongs_to_the_bin_above() {
        let ramp = scale_by_id("asprs").ramp();
        assert!(ramp.categorical());
        // The bottom edge of class 1's band, and anywhere above it short of
        // class 2's, is class 1. A hair below the edge is class 0: an
        // interpolation would have bled the two together there.
        assert_eq!(ramp.color_at(1.0 / 23.0), ramp.color_at(1.5 / 23.0));
        assert_eq!(ramp.color_at(1.0 / 23.0), ramp.color_at(2.0 / 23.0 - 0.01));
        assert_eq!(ramp.color_at(1.0 / 23.0 - f32::EPSILON), ramp.color_at(0.0));
        assert_eq!(ramp.color_at(0.75), ramp.color_at(17.0 / 23.0));
        // A continuous scale has no such thing: it runs between its stops.
        let grey = scale_by_id("grey").ramp();
        assert!(!grey.categorical());
        assert_ne!(grey.color_at(0.5), grey.color_at(0.6));
    }

    /// A continuous scale on the class field does read the cloud's own range,
    /// because nothing about it says the values are names.
    #[test]
    fn a_continuous_scale_runs_the_classes_it_is_given() {
        let box_bounds = bounds([0.0, 0.0, 0.0], [1.0, 10.0, 1.0]);
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Class,
            ..Default::default()
        };
        let tinted = look.resolve(&channels(&box_bounds, Some(7), None));
        assert_eq!(tinted.range(), (0.0, 7.0));
        assert_eq!(tinted.uniforms().params[3], 0.0, "bgyr is not a palette");
        let asprs = HeightLook {
            scale: Scale::Preset(scale_by_id("asprs")),
            ..look
        }
        .resolve(&channels(&box_bounds, Some(7), None));
        assert_eq!(asprs.range(), (0.0, 23.0), "the palette bounds it");
        assert_eq!(asprs.uniforms().params[3], 1.0);
    }

    /// Intensity is normalised against the cloud's own span, so a 12-bit and a
    /// 16-bit scan both fill the scale.
    #[test]
    fn an_intensity_range_is_the_clouds_own() {
        let box_bounds = bounds([0.0, 0.0, 0.0], [1.0, 10.0, 1.0]);
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Intensity,
            scale: Scale::Preset(scale_by_id("grey")),
            ..Default::default()
        };
        for (min, max) in [(0.0, 4095.0), (0.0, 65535.0), (120.0, 180.0)] {
            let tinted = look.resolve(&channels(&box_bounds, None, Some((min, max))));
            let intensity = |value: f32| Sample {
                intensity: value,
                ..Default::default()
            };
            assert_eq!(
                tinted.tint_at(&intensity(min)),
                Some([0.0; 3]),
                "{min}..{max} floor"
            );
            assert_eq!(
                tinted.tint_at(&intensity(max)),
                Some([1.0; 3]),
                "{min}..{max} ceiling"
            );
            // Half way up the range is half way up the scale, whatever the units.
            let middle = tinted.tint_at(&intensity((min + max) / 2.0)).unwrap();
            assert!((middle[0] - 0.5).abs() < 0.01, "{min}..{max} middle");
        }
        // A cloud with no intensity at all: the caller's gate keeps this out of
        // reach, and the maths still has an answer that is not a NaN.
        let blind = look.resolve(&FieldData::geometry(&box_bounds));
        assert!(
            blind
                .tint_at(&Sample::at([0.0, 5.0, 0.0]))
                .unwrap()
                .iter()
                .all(|value| value.is_finite())
        );
    }

    #[test]
    fn the_uniform_encodes_the_field_and_pads_the_scale() {
        let look = HeightLook {
            mode: HeightMode::Ramp,
            field: Field::Aspect,
            axis: 2,
            scale: Scale::Preset(scale_by_id("topo")),
            period: 4.0,
        };
        let uniforms = look
            .resolve(&FieldData::geometry(&bounds(
                [0.0, 0.0, -10.0],
                [1.0, 1.0, 10.0],
            )))
            .uniforms();
        assert_eq!(uniforms.coloring[0], HeightMode::Ramp.index() as f32);
        assert_eq!(uniforms.coloring[1], 2.0, "the axis");
        assert_eq!(uniforms.coloring[2], 0.0, "the absolute floor");
        assert!(
            (uniforms.coloring[3] - 1.0 / 360.0).abs() < 1e-6,
            "the absolute span"
        );
        assert_eq!(uniforms.params[1], 5.0, "five live stops");
        assert_eq!(
            uniforms.params[2],
            Field::Aspect.index() as f32,
            "the field"
        );
        assert!(
            (uniforms.params[0] - std::f32::consts::TAU / 4.0).abs() < 1e-6,
            "the banding phase step"
        );
        // Live stops first, ascending, and silence after them.
        assert_eq!(
            uniforms.ramp[0],
            [109.0 / 255.0, 158.0 / 255.0, 93.0 / 255.0, 0.0]
        );
        assert_eq!(uniforms.ramp[4], [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(uniforms.ramp[5], [0.0; 4]);
        assert_eq!(uniforms.ramp.len(), RAMP_STOPS);
    }

    #[test]
    fn a_legend_reads_the_same_colours_the_model_gets() {
        let box_bounds = bounds([0.0, 0.0, 0.0], [1.0, 4.0, 1.0]);
        let banded = HeightLook {
            mode: HeightMode::Bands,
            period: 2.0,
            ..Default::default()
        }
        .resolve(&FieldData::geometry(&box_bounds));
        let steps = banded.legend_steps(8);
        assert_eq!(steps.len(), 8);
        // Highest first, and every swatch is a colour the model really got.
        assert_eq!(
            steps[0],
            banded.tint_at(&Sample::at([0.0, 3.75, 0.0])).unwrap()
        );
        assert_eq!(
            *steps.last().unwrap(),
            banded.tint_at(&Sample::at([0.0, 0.25, 0.0])).unwrap()
        );
        // A ramp is sampled the same way, so the bar runs its scale end to end.
        let ramp = field(HeightMode::Ramp, box_bounds);
        let steps = ramp.legend_steps(32);
        assert_eq!(steps.len(), 32);
        assert_eq!(
            *steps.last().unwrap(),
            ramp.tint_at(&Sample::at([0.0, 0.062_5, 0.0])).unwrap()
        );
    }

    #[test]
    fn a_legends_sample_point_really_does_carry_the_value() {
        // The legend has to invent a point or normal whose field value *is* the
        // sampled value, so check that for the two normal-based fields.
        for field in [Field::Slope, Field::Aspect] {
            let tinted = HeightLook {
                mode: HeightMode::Ramp,
                field,
                axis: 1,
                ..Default::default()
            }
            .resolve(&FieldData::geometry(&bounds(
                [0.0, 0.0, 0.0],
                [1.0, 4.0, 1.0],
            )));
            let (min, max) = tinted.range();
            for step in 1..=5 {
                let value = min + (max - min) * step as f32 / 6.0;
                let back = tinted.value_at(&tinted.sample_at(value));
                assert!(
                    (back - value).abs() < 1e-3,
                    "{}: {value} sampled as {back}",
                    field.key()
                );
            }
        }
    }

    #[test]
    fn tick_labels_land_on_round_numbers_and_not_on_the_ends() {
        // The terrain case: a range whose equal parts are anything but readable
        // — a fourth of 6248.74 is 1562.185, which nobody can take off a bar.
        assert_eq!(
            nice_ticks(0.0, 6_248.74, 3),
            vec![2_000.0, 4_000.0, 6_000.0]
        );
        // Sub-unit ranges get sub-unit steps rather than a single label.
        assert_eq!(nice_ticks(0.0, 1.4, 2), vec![0.5, 1.0]);
        // Steps are round, so a range that lands on one never labels an end
        // twice: the ends are the bar's own, added by the caller.
        assert_eq!(nice_ticks(0.0, 100.0, 3), vec![25.0, 50.0, 75.0]);
        assert!(!nice_ticks(0.0, 90.0, 3).contains(&90.0));
        assert!(nice_ticks(0.0, 10.0, 3).contains(&5.0));
        // Every label is inside the range it describes, whatever it takes.
        for (min, max) in [(0.0, 1.4), (-5.0, 5.0), (100.0, 137.5), (0.0, 90.0)] {
            assert!(
                nice_ticks(min, max, 3).iter().all(|v| *v > min && *v < max),
                "{min}..{max}"
            );
        }
        // Nothing to work with: no span, or nothing asked for.
        assert!(nice_ticks(7.5, 7.5, 3).is_empty());
        assert!(nice_ticks(0.0, 100.0, 0).is_empty());
    }

    #[test]
    fn the_absolute_fields_carry_cloud_compares_own_labels() {
        // `ccColorScale::customLabels` for the two dip scales, verbatim: they
        // are the numbers a geologist reads the bar by.
        assert_eq!(Field::Slope.labels(), Some(&[0.0, 30.0, 60.0, 90.0][..]));
        assert_eq!(
            Field::Aspect.labels(),
            Some(&[0.0, 90.0, 180.0, 270.0, 360.0][..])
        );
        assert_eq!(Field::Height.labels(), None);
        // And they stay inside the range they are labelled against.
        for field in [Field::Slope, Field::Aspect] {
            let (min, max) = field.absolute_range().unwrap();
            assert!(
                field
                    .labels()
                    .unwrap()
                    .iter()
                    .all(|v| (min..=max).contains(v))
            );
        }
    }

    #[test]
    fn a_gradient_preview_starts_and_ends_where_the_scale_does() {
        let scale = scale_by_id("viridis");
        let steps = scale.gradient(9);
        assert_eq!(steps.len(), 9);
        assert_eq!(steps.first().unwrap().to_vec(), scale.stops[0].rgb.to_vec());
        assert_eq!(
            steps.last().unwrap().to_vec(),
            scale.stops.last().unwrap().rgb.to_vec()
        );
    }

    #[test]
    fn a_mode_and_field_survive_the_config_round_trip() {
        for mode in [HeightMode::Off, HeightMode::Ramp, HeightMode::Bands] {
            assert_eq!(HeightMode::from_key(mode.key()), mode);
            assert_eq!(HeightMode::from_index(mode.index()), mode);
        }
        assert_eq!(HeightMode::from_key("nonsense"), HeightMode::Off);
        for field in [
            Field::Height,
            Field::Slope,
            Field::Aspect,
            Field::Intensity,
            Field::Class,
        ] {
            assert_eq!(Field::from_key(field.key()), field);
            assert_eq!(Field::from_index(field.index()), field);
        }
        assert_eq!(Field::from_key("nonsense"), Field::Height);
        // The two scanner fields are the only ones that need a channel the file
        // may not have carried, which is what the viewport gates them on.
        assert_eq!(Field::Height.channel(), None);
        assert_eq!(Field::Slope.channel(), None);
        assert_eq!(Field::Aspect.channel(), None);
        assert_eq!(Field::Intensity.channel(), Some(Channel::Intensity));
        assert_eq!(Field::Class.channel(), Some(Channel::Class));
    }
}
