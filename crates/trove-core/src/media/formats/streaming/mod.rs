//! Streaming data pipeline for large 3D files.
//!
//! Handles the path from "file on disk" to "visible points on screen":
//!
//! 1. **PageTable** — tracks which regions of a file are resident in memory,
//!    which are on disk, and which are being loaded. LRU eviction.
//!
//! 2. **PointStreamer** — reads a point-cloud file in chunks, inserts points
//!    into an octree incrementally, and reports progress. The renderer can
//!    draw intermediate results while loading continues.
//!
//! 3. **MeshStreamer** — same idea for triangle meshes (BVH + meshlet).

pub mod page_table;
pub mod point_streamer;

pub use page_table::{Page, PageId, PageState, PageTable};
pub use point_streamer::{LoadProgress, PointStreamer, StreamChunk};
