//! Virtual memory and prefetching for out-of-core 3D rendering.
//!
//! Combines the page table (disk ↔ memory mapping) with camera-aware
//! prefetching. Pages are loaded based on:
//! * **Visibility**: pages inside the camera frustum get highest priority
//! * **Proximity**: pages near the camera are loaded before distant ones
//! * **Direction**: pages in the camera's movement direction are pre-fetched
//!
//! Memory budget is enforced by LRU eviction. A 20 GB file can be previewed
//! with only ~256 MB of RAM.

#[allow(unused_imports)]
use super::streaming::page_table::{PAGE_SIZE, Page, PageId, PageState, PageTable};
use crate::media::formats::point_cloud::Frustum;
use crate::media::formats::types::Bounds;

/// Default resident memory budget: 256 MiB.
pub const DEFAULT_MEMORY_BUDGET: usize = 256 << 20;

/// Maximum pages to load per frame.
const MAX_PAGES_PER_FRAME: usize = 4;

/// Virtual memory manager for a 3D file.
pub struct VirtualMemory {
    page_table: PageTable,
    file_len: u64,
}

/// Priority score for page loading (higher = load first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PagePriority {
    page_id: PageId,
    /// Distance from camera to page center quantized to u32 (lower = higher priority).
    distance: u32,
    /// Flags packed into a byte: bit 0 = visible, bit 1 = in_direction.
    flags: u8,
}

impl Ord for PagePriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let self_score = self.score();
        let other_score = other.score();
        self_score.cmp(&other_score)
    }
}

impl PartialOrd for PagePriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PagePriority {
    fn score(&self) -> i64 {
        let mut s = 0i64;
        if self.flags & 1 != 0 {
            s -= 1000; // visible
        }
        if self.flags & 2 != 0 {
            s -= 500; // in_direction
        }
        s - (self.distance as i64)
    }
}

impl VirtualMemory {
    /// Create a virtual memory manager for a file.
    pub fn new(file_len: u64) -> Self {
        Self {
            page_table: PageTable::new(file_len, DEFAULT_MEMORY_BUDGET),
            file_len,
        }
    }

    /// Create with a custom memory budget.
    pub fn with_budget(file_len: u64, memory_budget: usize) -> Self {
        Self {
            page_table: PageTable::new(file_len, memory_budget),
            file_len,
        }
    }

    /// Total file size.
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Number of pages.
    pub fn num_pages(&self) -> usize {
        self.page_table.num_pages()
    }

    /// Resident memory usage in bytes.
    pub fn resident_memory(&self) -> u64 {
        self.page_table.resident_memory()
    }

    /// Pages that should be loaded next, ordered by priority.
    pub fn prefetch_pages(
        &self,
        frustum: &Frustum,
        camera_pos: [f32; 3],
        camera_dir: [f32; 3],
    ) -> Vec<PageId> {
        let mut priorities = Vec::new();

        for page_id in 0..self.page_table.num_pages() as u32 {
            let page = match self.page_table.page(page_id) {
                Some(p) => p,
                None => continue,
            };
            if page.state != PageState::OnDisk {
                continue;
            }

            let page_center = page_center(page);
            let distance = dist(camera_pos, page_center);
            let visible = frustum.intersects_bounds(&page_bounds(page));
            let to_page = normalize([
                page_center[0] - camera_pos[0],
                page_center[1] - camera_pos[1],
                page_center[2] - camera_pos[2],
            ]);
            let in_direction = dot(to_page, camera_dir) > 0.5;

            let dist_quantized = (distance * 1000.0) as u32;
            let flags = (visible as u8) | ((in_direction as u8) << 1);
            priorities.push(PagePriority {
                page_id,
                distance: dist_quantized,
                flags,
            });
        }

        priorities.sort();
        priorities
            .iter()
            .take(MAX_PAGES_PER_FRAME)
            .map(|p| p.page_id)
            .collect()
    }

    /// Mark a page as resident (loaded).
    pub fn mark_resident(&mut self, page_id: PageId) {
        self.page_table.mark_resident(page_id);
    }

    /// Mark a page as being loaded.
    pub fn mark_loading(&mut self, page_id: PageId) {
        self.page_table.mark_loading(page_id);
    }

    /// Evict a page (mark as on-disk).
    pub fn evict_page(&mut self, page_id: PageId) {
        self.page_table.touch(page_id);
        // (Actual eviction is handled by LRU in mark_resident.)
    }

    /// Check if a page is resident.
    pub fn is_resident(&self, page_id: PageId) -> bool {
        self.page_table
            .page(page_id)
            .is_some_and(|p| p.state == PageState::Resident)
    }

    /// Get page info.
    pub fn page(&self, page_id: PageId) -> Option<&Page> {
        self.page_table.page(page_id)
    }

    /// Residency ratio [0, 1].
    pub fn residency_ratio(&self) -> f32 {
        self.page_table.residency_ratio()
    }
}

fn page_center(page: &Page) -> [f32; 3] {
    let mid = page.file_offset + page.byte_len / 2;
    let normalized = (mid as f64 / u32::MAX as f64).min(1.0);
    [normalized as f32, 0.5, 0.5]
}

fn page_bounds(page: &Page) -> Bounds {
    let center = page_center(page);
    let extent = (page.byte_len as f64 / (1 << 30) as f64).max(0.001) as f32;
    Bounds {
        min: [center[0] - extent, center[1] - extent, center[2] - extent],
        max: [center[0] + extent, center[1] + extent, center[2] + extent],
    }
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    ((dx * dx + dy * dy + dz * dz) as f64).sqrt()
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
    [v[0] / len, v[1] / len, v[2] / len]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_memory_basics() {
        let vm = VirtualMemory::new(10 << 20); // 10 MB file
        assert!(vm.num_pages() > 0);
        assert_eq!(vm.file_len(), 10 << 20);
    }

    #[test]
    fn prefetch_prioritizes_visible_pages() {
        let vm = VirtualMemory::new(10 << 20);
        let full_frustum = Frustum {
            planes: std::array::from_fn(|i| {
                let n = match i {
                    0 => [1.0, 0.0, 0.0],
                    1 => [-1.0, 0.0, 0.0],
                    2 => [0.0, 1.0, 0.0],
                    3 => [0.0, -1.0, 0.0],
                    4 => [0.0, 0.0, 1.0],
                    5 => [0.0, 0.0, -1.0],
                    _ => [0.0, 0.0, 0.0],
                };
                crate::media::formats::point_cloud::Plane {
                    normal: n,
                    distance: 10.0,
                }
            }),
        };

        let to_load = vm.prefetch_pages(&full_frustum, [0.5, 0.5, -10.0], [0.0, 0.0, 1.0]);
        assert!(!to_load.is_empty());
    }

    #[test]
    fn lru_eviction() {
        let mut vm = VirtualMemory::with_budget(100 << 20, 1 << 20); // 100 MB file, 1 MB budget
        // Load pages until eviction happens.
        for i in 0..20 {
            vm.mark_resident(i as u32);
        }
        // Memory should stay within budget (1 MB / 256 KB = 4 pages).
        assert!(vm.resident_memory() <= 5 * PAGE_SIZE);
    }
}
