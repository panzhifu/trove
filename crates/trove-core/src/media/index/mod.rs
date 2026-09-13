//! The on-disk index that makes a very large model openable.
//!
//! A cloud is indexed for the same reason an image gets a thumbnail: reading
//! all of it to draw part of it does not scale, and at twenty gigabytes it
//! does not fit in memory at all. The index is a spatial ordering of the
//! points (see [`order`]) plus, in time, the per-run bounds and offsets a
//! renderer streams by.
//!
//! Nothing here renders: it is the file format and the sort that feeds it, so
//! it can be tested without a graphics device.

pub mod file;
pub mod order;
pub mod ply;
pub mod sort;
pub mod view;

pub use file::{BatchSource, CloudIndex, IndexChunk, IndexConfig, IndexSummary, build_index};
pub use order::{Chunk, Grid, SpatialOrder, chunked, morton_code, sort_spatially, spatial_key};
pub use ply::{build_ply_index, build_ply_index_with_grid};
pub use sort::{SortedPoints, SpatialSorter};
pub use view::{INDEX_EXTENSION, IndexStep, IndexedCloud, index_path_for};
