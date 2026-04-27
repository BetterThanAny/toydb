//! Slotted page — fixed 8 KiB block holding variable-length records.
//!
//! Layout (offsets in bytes within an [`PAGE_SIZE`] buffer):
//!
//! ```text
//! 0    page_type    : u8
//! 1    _padding     : 3 bytes
//! 4    slot_count   : u32   (number of valid slots, may include tombstones)
//! 8    free_offset  : u32   (start of free space, growing downward from end)
//! 12   next_page    : u64   (page id of overflow page; 0 = none)
//! 20   slot_dir[]   : u32 record_offset, u32 record_len  (8 bytes per slot)
//! ...                free space ...
//! end  records[]    grow upward from PAGE_SIZE
//! ```
//!
//! Records are stored at the *bottom* of the page, slots at the *top*.
//! Free space is everything between `slot_dir_end` and `free_offset`.
//!
//! Tombstoned slots have `record_len == 0` so iteration knows to skip
//! them; reuse on next write that fits.

use crate::error::{Error, Result};

pub const PAGE_SIZE: usize = 8192;
pub const HEADER_SIZE: usize = 20;
pub const SLOT_SIZE: usize = 8;

pub type PageId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// First page of the data file: holds metadata + free list head.
    Super = 1,
    /// Catalog page: holds serialized table descriptors.
    Catalog = 2,
    /// Row data for a user table.
    TableData = 3,
    /// Free, available for reuse.
    Free = 4,
}

impl PageType {
    pub fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            1 => PageType::Super,
            2 => PageType::Catalog,
            3 => PageType::TableData,
            4 => PageType::Free,
            other => return Err(Error::other(format!("unknown page type {other}"))),
        })
    }
}

/// In-memory page buffer. Always exactly [`PAGE_SIZE`] bytes.
#[derive(Clone)]
pub struct Page {
    pub buf: Box<[u8; PAGE_SIZE]>,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("type", &self.page_type().ok())
            .field("slot_count", &self.slot_count())
            .field("free_offset", &self.free_offset())
            .field("free_space", &self.free_space())
            .finish()
    }
}

impl Page {
    /// Create a fresh page with the given type. Slot count = 0,
    /// free-space pointer at the very end.
    pub fn new(page_type: PageType) -> Self {
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf[0] = page_type as u8;
        write_u32(&mut buf[..], 4, 0); // slot_count
        write_u32(&mut buf[..], 8, PAGE_SIZE as u32); // free_offset
        write_u64(&mut buf[..], 12, 0); // next_page
        Self { buf }
    }

    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self {
        Self { buf: Box::new(bytes) }
    }

    pub fn page_type(&self) -> Result<PageType> {
        PageType::from_byte(self.buf[0])
    }

    pub fn set_page_type(&mut self, t: PageType) {
        self.buf[0] = t as u8;
    }

    pub fn slot_count(&self) -> u32 {
        read_u32(&self.buf[..], 4)
    }

    fn set_slot_count(&mut self, n: u32) {
        write_u32(&mut self.buf[..], 4, n);
    }

    pub fn free_offset(&self) -> u32 {
        read_u32(&self.buf[..], 8)
    }

    fn set_free_offset(&mut self, o: u32) {
        write_u32(&mut self.buf[..], 8, o);
    }

    pub fn next_page(&self) -> PageId {
        read_u64(&self.buf[..], 12)
    }

    pub fn set_next_page(&mut self, p: PageId) {
        write_u64(&mut self.buf[..], 12, p);
    }

    /// Bytes available for a *new* record (already counts the slot
    /// directory entry that the new record will need).
    pub fn free_space(&self) -> usize {
        let slot_end = HEADER_SIZE + (self.slot_count() as usize) * SLOT_SIZE;
        let free_off = self.free_offset() as usize;
        free_off.saturating_sub(slot_end).saturating_sub(SLOT_SIZE)
    }

    /// Insert a record into the first available tombstone (if it fits)
    /// or, failing that, into fresh space. Returns the slot index.
    pub fn insert(&mut self, data: &[u8]) -> Result<u16> {
        // Try to reuse a tombstone slot first.
        for i in 0..self.slot_count() {
            let (off, len) = self.slot(i as usize);
            if len == 0 && self.free_space_for_existing_slot() >= data.len() {
                let _ = off;
                return self.write_into_slot(i as usize, data);
            }
        }
        if self.free_space() < data.len() {
            return Err(Error::other(format!(
                "page full: need {} bytes, have {}",
                data.len(),
                self.free_space()
            )));
        }
        let slot_idx = self.slot_count() as usize;
        self.set_slot_count(slot_idx as u32 + 1);
        self.write_into_slot(slot_idx, data)
    }

    /// Mark a slot as tombstoned. The record bytes stay in place but
    /// will be reclaimed by future inserts.
    pub fn delete(&mut self, slot: u16) -> Result<()> {
        if slot as u32 >= self.slot_count() {
            return Err(Error::other(format!("slot {slot} out of range")));
        }
        let dir_off = HEADER_SIZE + (slot as usize) * SLOT_SIZE;
        write_u32(&mut self.buf[..], dir_off + 4, 0); // record_len = 0
        Ok(())
    }

    /// Update an existing slot in place if the new record fits, else
    /// returns an error so the caller can allocate a fresh slot or page.
    pub fn update(&mut self, slot: u16, data: &[u8]) -> Result<()> {
        if slot as u32 >= self.slot_count() {
            return Err(Error::other(format!("slot {slot} out of range")));
        }
        let (off, len) = self.slot(slot as usize);
        if data.len() <= len as usize {
            // Same size or shrinking: in-place.
            let start = off as usize;
            let end = start + data.len();
            self.buf[start..end].copy_from_slice(data);
            // Update slot len.
            let dir_off = HEADER_SIZE + (slot as usize) * SLOT_SIZE;
            write_u32(&mut self.buf[..], dir_off + 4, data.len() as u32);
            Ok(())
        } else {
            // Need to grow: tombstone old, reinsert.
            self.delete(slot)?;
            // Caller will pick a slot via insert; we return Err so
            // they re-route. Returning ok would change the slot id.
            Err(Error::other("page update needs reallocation"))
        }
    }

    /// Get bytes at a slot. Returns `None` for tombstoned slots.
    pub fn get(&self, slot: u16) -> Option<&[u8]> {
        if slot as u32 >= self.slot_count() {
            return None;
        }
        let (off, len) = self.slot(slot as usize);
        if len == 0 {
            return None;
        }
        Some(&self.buf[off as usize..(off + len) as usize])
    }

    /// Iterate over `(slot_index, bytes)` pairs, skipping tombstones.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &[u8])> {
        (0..self.slot_count()).filter_map(move |i| {
            let (off, len) = self.slot(i as usize);
            if len == 0 {
                return None;
            }
            Some((i as u16, &self.buf[off as usize..(off + len) as usize]))
        })
    }

    pub fn raw(&self) -> &[u8; PAGE_SIZE] { &self.buf }
    pub fn raw_mut(&mut self) -> &mut [u8; PAGE_SIZE] { &mut self.buf }

    /// Slice form of [`raw_mut`] for callers that need `&mut [u8]`.
    pub fn raw_slice_mut(&mut self) -> &mut [u8] { &mut self.buf[..] }

    // -- internals -----------------------------------------------------

    fn slot(&self, idx: usize) -> (u32, u32) {
        let base = HEADER_SIZE + idx * SLOT_SIZE;
        (read_u32(&self.buf[..], base), read_u32(&self.buf[..], base + 4))
    }

    fn write_into_slot(&mut self, slot_idx: usize, data: &[u8]) -> Result<u16> {
        let new_free = self.free_offset() as usize - data.len();
        // Place record at [new_free .. new_free + len].
        self.buf[new_free..new_free + data.len()].copy_from_slice(data);
        // Slot directory entry.
        let dir_off = HEADER_SIZE + slot_idx * SLOT_SIZE;
        write_u32(&mut self.buf[..], dir_off, new_free as u32);
        write_u32(&mut self.buf[..], dir_off + 4, data.len() as u32);
        self.set_free_offset(new_free as u32);
        Ok(slot_idx as u16)
    }

    fn free_space_for_existing_slot(&self) -> usize {
        let slot_end = HEADER_SIZE + (self.slot_count() as usize) * SLOT_SIZE;
        let free_off = self.free_offset() as usize;
        free_off.saturating_sub(slot_end)
    }
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn write_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    let arr: [u8; 4] = buf[off..off + 4].try_into().unwrap();
    u32::from_le_bytes(arr)
}

fn read_u64(buf: &[u8], off: usize) -> u64 {
    let arr: [u8; 8] = buf[off..off + 8].try_into().unwrap();
    u64::from_le_bytes(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_page_has_full_free_space() {
        let p = Page::new(PageType::TableData);
        assert_eq!(p.slot_count(), 0);
        assert_eq!(p.free_offset(), PAGE_SIZE as u32);
        assert!(p.free_space() > PAGE_SIZE - HEADER_SIZE - 100);
    }

    #[test]
    fn insert_and_read_back() {
        let mut p = Page::new(PageType::TableData);
        let s0 = p.insert(b"hello").unwrap();
        let s1 = p.insert(b"world").unwrap();
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(p.get(s0), Some(&b"hello"[..]));
        assert_eq!(p.get(s1), Some(&b"world"[..]));
    }

    #[test]
    fn delete_then_iter_skips_tombstone() {
        let mut p = Page::new(PageType::TableData);
        p.insert(b"a").unwrap();
        let s = p.insert(b"b").unwrap();
        p.insert(b"c").unwrap();
        p.delete(s).unwrap();
        let live: Vec<_> = p.iter().map(|(_, b)| b.to_vec()).collect();
        assert_eq!(live, vec![b"a".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn update_in_place_shrink() {
        let mut p = Page::new(PageType::TableData);
        let s = p.insert(b"hello").unwrap();
        p.update(s, b"hi").unwrap();
        assert_eq!(p.get(s), Some(&b"hi"[..]));
    }

    #[test]
    fn update_grow_returns_error_for_caller() {
        let mut p = Page::new(PageType::TableData);
        let s = p.insert(b"hi").unwrap();
        // grow → caller must move it
        assert!(p.update(s, b"hello").is_err());
        // After the failed update, slot is tombstoned.
        assert!(p.get(s).is_none());
    }

    #[test]
    fn full_page_rejects_insert() {
        let mut p = Page::new(PageType::TableData);
        let payload = vec![0u8; 4096];
        p.insert(&payload).unwrap();
        let err = p.insert(&payload);
        assert!(err.is_err());
    }

    #[test]
    fn next_page_pointer_round_trips() {
        let mut p = Page::new(PageType::TableData);
        p.set_next_page(42);
        let bytes = *p.raw();
        let p2 = Page::from_bytes(bytes);
        assert_eq!(p2.next_page(), 42);
    }

    #[test]
    fn page_type_round_trip() {
        let p = Page::new(PageType::Catalog);
        assert_eq!(p.page_type().unwrap(), PageType::Catalog);
    }

    #[test]
    fn tombstone_reuse() {
        let mut p = Page::new(PageType::TableData);
        let s0 = p.insert(b"abcde").unwrap();
        let _ = p.insert(b"xyz").unwrap();
        p.delete(s0).unwrap();
        let s2 = p.insert(b"vw").unwrap();
        // Should reuse slot 0 (tombstoned) since smaller record fits.
        assert_eq!(s2, 0);
        assert_eq!(p.get(s2), Some(&b"vw"[..]));
    }
}
