//! Two-level page table over the 32-bit id space, pages allocated on
//! demand. Shared by the pool's sparse index and the id allocator's
//! alive-tracking.

const PAGE_ENTRIES: usize = 4096;

pub(crate) struct PagedArray<T> {
    pages: Vec<Option<Box<[T; PAGE_ENTRIES]>>>,
    fill: T,
}

impl<T: Copy + PartialEq> PagedArray<T> {
    pub fn new(fill: T) -> Self {
        Self {
            pages: Vec::new(),
            fill,
        }
    }

    pub fn get(&self, idx: u32) -> T {
        match self.pages.get(idx as usize / PAGE_ENTRIES) {
            Some(Some(page)) => page[idx as usize % PAGE_ENTRIES],
            _ => self.fill,
        }
    }

    pub fn set(&mut self, idx: u32, value: T) {
        let page = idx as usize / PAGE_ENTRIES;
        let absent = self.pages.get(page).is_none_or(|p| p.is_none());
        if absent && value == self.fill {
            return;
        }
        if page >= self.pages.len() {
            self.pages.resize_with(page + 1, || None);
        }
        let fill = self.fill;
        let entries = self.pages[page].get_or_insert_with(|| Box::new([fill; PAGE_ENTRIES]));
        entries[idx as usize % PAGE_ENTRIES] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_reads_return_fill() {
        let arr: PagedArray<u32> = PagedArray::new(u32::MAX);
        assert_eq!(arr.get(0), u32::MAX);
        assert_eq!(arr.get(1_000_000), u32::MAX);
    }

    #[test]
    fn set_get_across_pages() {
        let mut arr = PagedArray::new(false);
        arr.set(3, true);
        arr.set(100_000, true);
        assert!(arr.get(3));
        assert!(arr.get(100_000));
        assert!(!arr.get(4));
        arr.set(3, false);
        assert!(!arr.get(3));
    }

    #[test]
    fn writing_fill_to_absent_page_allocates_nothing() {
        let mut arr = PagedArray::new(0u32);
        arr.set(50_000, 0);
        assert_eq!(arr.pages.len(), 0);
    }
}
