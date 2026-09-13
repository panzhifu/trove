//! Virtual memory page table for out-of-core point cloud rendering.
//!
//! A file is divided into fixed-size **pages**. The page table tracks whether
//! each page is resident in memory, being loaded, or only on disk. An LRU
//! eviction policy keeps memory usage bounded.

use std::collections::VecDeque;

/// Default page size: 256 KiB (≈8000 points at 32 bytes/point).
pub const PAGE_SIZE: u64 = 256 << 10;

/// State of a single page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    /// On disk, not in memory.
    OnDisk,
    /// Being loaded by an IO task.
    Loading,
    /// Resident in memory. Stores the index into the page cache.
    Resident,
}

/// Metadata for one page.
#[derive(Debug, Clone)]
pub struct Page {
    pub id: PageId,
    pub file_offset: u64,
    pub byte_len: u64,
    pub state: PageState,
}

/// Identifies a page by its index.
pub type PageId = u32;

/// Bounded page table with LRU eviction.
///
/// Memory budget is `max_resident_pages * PAGE_SIZE`. When a new page is
/// loaded and the budget is exceeded, the least-recently-used resident page
/// is evicted back to "on disk".
#[derive(Debug)]
pub struct PageTable {
    pages: Vec<Page>,
    /// LRU queue: front = least recently used, back = most recently used.
    lru: VecDeque<PageId>,
    max_resident: usize,
    file_len: u64,
}

impl PageTable {
    /// Create a page table for a file of `file_len` bytes.
    ///
    /// `max_resident_memory` is the target resident set size; the actual
    /// number of resident pages is `max_resident_memory / PAGE_SIZE`.
    pub fn new(file_len: u64, max_resident_memory: usize) -> Self {
        let num_pages = file_len.div_ceil(PAGE_SIZE).max(1) as usize;
        let max_resident = (max_resident_memory / PAGE_SIZE as usize).max(1);

        let mut pages = Vec::with_capacity(num_pages);
        for i in 0..num_pages {
            let offset = i as u64 * PAGE_SIZE;
            let remaining = file_len - offset;
            let byte_len = remaining.min(PAGE_SIZE);
            pages.push(Page {
                id: i as PageId,
                file_offset: offset,
                byte_len,
                state: PageState::OnDisk,
            });
        }

        Self {
            pages,
            lru: VecDeque::new(),
            max_resident,
            file_len,
        }
    }

    /// Number of pages in the file.
    pub fn num_pages(&self) -> usize {
        self.pages.len()
    }

    /// Total file size in bytes.
    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    /// Page size in bytes.
    pub fn page_size(&self) -> u64 {
        PAGE_SIZE
    }

    /// Get a page by ID.
    pub fn page(&self, id: PageId) -> Option<&Page> {
        self.pages.get(id as usize)
    }

    /// Mark a page as resident (just loaded). Evicts LRU pages if over budget.
    pub fn mark_resident(&mut self, id: PageId) {
        if let Some(page) = self.pages.get_mut(id as usize) {
            page.state = PageState::Resident;
        }
        // Remove from LRU if already present, then push to back (most recent).
        self.lru.retain(|&p| p != id);
        self.lru.push_back(id);
        self.enforce_budget();
    }

    /// Mark a page as being loaded.
    pub fn mark_loading(&mut self, id: PageId) {
        if let Some(page) = self.pages.get_mut(id as usize) {
            page.state = PageState::Loading;
        }
    }

    /// Touch a page (mark as recently used).
    pub fn touch(&mut self, id: PageId) {
        if self
            .pages
            .get(id as usize)
            .is_some_and(|p| p.state == PageState::Resident)
        {
            self.lru.retain(|&p| p != id);
            self.lru.push_back(id);
        }
    }

    /// Evict least-recently-used pages until within budget.
    fn enforce_budget(&mut self) {
        while self.lru.len() > self.max_resident {
            if let Some(evict_id) = self.lru.pop_front()
                && let Some(page) = self.pages.get_mut(evict_id as usize)
            {
                page.state = PageState::OnDisk;
            }
        }
    }

    /// Pages that are on disk and should be loaded next, ordered by `priority`
    /// (lower = load first). Returns at most `max_pages` candidates.
    pub fn pages_to_load(&self, priority_order: &[PageId], max_pages: usize) -> Vec<PageId> {
        priority_order
            .iter()
            .filter(|&&id| {
                self.pages
                    .get(id as usize)
                    .is_some_and(|p| p.state == PageState::OnDisk)
            })
            .take(max_pages)
            .copied()
            .collect()
    }

    /// Resident memory usage in bytes (approximate).
    pub fn resident_memory(&self) -> u64 {
        self.lru.len() as u64 * PAGE_SIZE
    }

    /// Fraction of pages that are resident [0, 1].
    pub fn residency_ratio(&self) -> f32 {
        if self.pages.is_empty() {
            return 1.0;
        }
        let resident = self.lru.len();
        resident as f32 / self.pages.len() as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_table_basics() {
        // 10 MB file, 1 MB resident budget → 40 pages, 4 resident.
        let mut pt = PageTable::new(10 << 20, 1 << 20);
        assert_eq!(pt.num_pages(), 40);
        assert_eq!(pt.page_size(), PAGE_SIZE);

        // Load pages 0, 1, 2.
        pt.mark_loading(0);
        pt.mark_resident(0);
        pt.mark_resident(1);
        pt.mark_resident(2);
        assert_eq!(pt.residency_ratio(), 3.0 / 40.0);

        // Load more — should evict LRU (page 0).
        pt.mark_resident(3);
        pt.mark_resident(4);
        pt.mark_resident(5);
        assert_eq!(pt.resident_memory(), 4 * PAGE_SIZE);
        assert_eq!(pt.page(0).unwrap().state, PageState::OnDisk); // evicted
        assert_eq!(pt.page(5).unwrap().state, PageState::Resident); // most recent
    }

    #[test]
    fn pages_to_load() {
        let mut pt = PageTable::new(10 << 20, 1 << 20);
        // Pages 5, 3, 7 are priority.
        let priority = vec![5, 3, 7];
        let to_load = pt.pages_to_load(&priority, 2);
        assert_eq!(to_load, vec![5, 3]); // first 2 on-disk pages in priority order

        // After loading page 5, it should no longer appear.
        pt.mark_resident(5);
        let to_load = pt.pages_to_load(&priority, 2);
        assert_eq!(to_load, vec![3, 7]);
    }
}
