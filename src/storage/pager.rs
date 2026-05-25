//! File-backed paged storage with a small write-back cache.
//!
//! The pager is the only thing that touches the disk; everything above
//! it (catalog, row pages, WAL) sees pages as `Page` objects.
//!
//! Layout: page 0 is the *super page* with magic + version + free-list
//! head + catalog root. Pages 1..N are user data.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::page::{HEADER_SIZE, PAGE_SIZE, Page, PageId, PageType};

pub const MAGIC: &[u8; 8] = b"TOYDB001";
pub const SUPER_PAGE: PageId = 0;

/// Position of fields inside the super page.
const SUPER_MAGIC_OFF: usize = HEADER_SIZE; // start of free space area
const SUPER_VERSION_OFF: usize = HEADER_SIZE + 8;
const SUPER_PAGE_COUNT_OFF: usize = HEADER_SIZE + 12;
const SUPER_FREE_HEAD_OFF: usize = HEADER_SIZE + 20;
const SUPER_CATALOG_ROOT_OFF: usize = HEADER_SIZE + 28;

pub struct Pager {
    file: File,
    path: PathBuf,
    cache: HashMap<PageId, CacheEntry>,
    page_count: u64,
    free_head: PageId,
    catalog_root: PageId,
}

struct CacheEntry {
    page: Page,
    dirty: bool,
}

impl Pager {
    /// Open or create a database file. New files are bootstrapped with
    /// a super page and an empty catalog page.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let exists = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let mut p = Self {
            file,
            path,
            cache: HashMap::new(),
            page_count: 1,
            free_head: 0,
            catalog_root: 0,
        };
        if exists && p.file.metadata()?.len() >= PAGE_SIZE as u64 {
            p.read_super()?;
        } else {
            p.bootstrap()?;
        }
        Ok(p)
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }
    pub fn catalog_root(&self) -> PageId {
        self.catalog_root
    }
    pub fn set_catalog_root(&mut self, id: PageId) -> Result<()> {
        self.catalog_root = id;
        self.write_super()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return a borrowed copy of a page. The pager keeps an authoritative
    /// in-memory copy; callers mutate via [`Pager::write_page`].
    pub fn read_page(&mut self, id: PageId) -> Result<Page> {
        if let Some(entry) = self.cache.get(&id) {
            return Ok(entry.page.clone());
        }
        if id >= self.page_count {
            return Err(Error::other(format!("page {id} out of range")));
        }
        let mut buf = [0u8; PAGE_SIZE];
        self.file.seek(SeekFrom::Start(id * PAGE_SIZE as u64))?;
        self.file.read_exact(&mut buf)?;
        let page = Page::from_bytes(buf);
        page.validate()?;
        self.cache.insert(
            id,
            CacheEntry {
                page: page.clone(),
                dirty: false,
            },
        );
        Ok(page)
    }

    /// Write a page back to the cache (marking it dirty). Use [`flush`]
    /// to push dirty pages to disk in one go.
    pub fn write_page(&mut self, id: PageId, page: Page) -> Result<()> {
        if id >= self.page_count {
            return Err(Error::other(format!("page {id} out of range for write")));
        }
        self.cache.insert(id, CacheEntry { page, dirty: true });
        Ok(())
    }

    /// Allocate a new page (first reuse from the free list, then grow
    /// the file). The returned page is initialised to `kind` and is
    /// already in the cache as dirty.
    pub fn allocate(&mut self, kind: PageType) -> Result<PageId> {
        let id = if self.free_head != 0 {
            let head = self.free_head;
            // The free page's `next_page` points to the next free.
            let p = self.read_page(head)?;
            self.free_head = p.next_page();
            self.write_super()?;
            head
        } else {
            let id = self.page_count;
            self.page_count += 1;
            // Grow the file with zeros.
            self.file.set_len(self.page_count * PAGE_SIZE as u64)?;
            self.write_super()?;
            id
        };
        let page = Page::new(kind);
        self.cache.insert(id, CacheEntry { page, dirty: true });
        Ok(id)
    }

    /// Ensure that `id` exists for WAL replay. If `reset` is true, the page
    /// is reinitialised to `kind` after being removed from the free list.
    pub fn ensure_page_id(&mut self, id: PageId, kind: PageType, reset: bool) -> Result<()> {
        if id == SUPER_PAGE {
            return Err(Error::other("cannot prepare super page for replay"));
        }
        if id >= self.page_count {
            let old_count = self.page_count;
            self.page_count = id + 1;
            self.file.set_len(self.page_count * PAGE_SIZE as u64)?;
            self.write_super()?;
            for page_id in old_count..=id {
                let page_type = if page_id == id { kind } else { PageType::Free };
                self.cache.insert(
                    page_id,
                    CacheEntry {
                        page: Page::new(page_type),
                        dirty: true,
                    },
                );
            }
            return Ok(());
        }

        let page = self.read_page(id)?;
        let page_type = page.page_type()?;
        if page_type == PageType::Free {
            self.remove_from_free_list(id)?;
        }
        if reset || page_type == PageType::Free {
            self.cache.insert(
                id,
                CacheEntry {
                    page: Page::new(kind),
                    dirty: true,
                },
            );
        } else if page_type != kind {
            return Err(Error::other(format!(
                "page {id} has type {page_type:?}, expected {kind:?}"
            )));
        }
        Ok(())
    }

    /// Add a page to the free list. Future `allocate` calls reuse it.
    pub fn deallocate(&mut self, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(Error::other("cannot deallocate super page"));
        }
        if id >= self.page_count {
            return Err(Error::other(format!(
                "page {id} out of range for deallocate"
            )));
        }
        if id == self.catalog_root {
            return Err(Error::other("cannot deallocate current catalog root"));
        }
        let page = self.read_page(id)?;
        if page.page_type()? == PageType::Free {
            return Err(Error::other(format!("page {id} is already free")));
        }
        if self.free_list_contains(id)? {
            return Err(Error::other(format!("page {id} is already in free list")));
        }
        let mut p = Page::new(PageType::Free);
        p.set_next_page(self.free_head);
        self.cache.insert(
            id,
            CacheEntry {
                page: p,
                dirty: true,
            },
        );
        self.free_head = id;
        self.write_super()?;
        Ok(())
    }

    fn remove_from_free_list(&mut self, id: PageId) -> Result<()> {
        let mut cur = self.free_head;
        let mut prev = 0;
        let mut seen = HashSet::new();
        while cur != 0 {
            if !seen.insert(cur) {
                return Err(Error::other(format!(
                    "free list contains a cycle at page {cur}"
                )));
            }
            let page = self.read_page(cur)?;
            let next = page.next_page();
            if cur == id {
                if prev == 0 {
                    self.free_head = next;
                } else {
                    let mut prev_page = self.read_page(prev)?;
                    prev_page.set_next_page(next);
                    self.write_page(prev, prev_page)?;
                }
                self.write_super()?;
                return Ok(());
            }
            prev = cur;
            cur = next;
        }
        Ok(())
    }

    fn free_list_contains(&mut self, id: PageId) -> Result<bool> {
        let mut cur = self.free_head;
        let mut seen = HashSet::new();
        while cur != 0 {
            if cur == id {
                return Ok(true);
            }
            if !seen.insert(cur) {
                return Err(Error::other(format!(
                    "free list contains a cycle at page {cur}"
                )));
            }
            cur = self.read_page(cur)?.next_page();
        }
        Ok(false)
    }

    /// Persist all dirty pages and fsync the file. Data/catalog pages are
    /// forced before the super page, then the super page is forced separately,
    /// so metadata does not advertise page-list changes before the pages they
    /// reference have reached the storage device.
    pub fn flush(&mut self) -> Result<()> {
        let mut non_super: Vec<PageId> = self
            .cache
            .iter()
            .filter_map(|(k, v)| {
                if v.dirty && *k != SUPER_PAGE {
                    Some(*k)
                } else {
                    None
                }
            })
            .collect();
        non_super.sort_unstable();
        self.flush_pages(&non_super)?;

        let super_dirty = self.cache.get(&SUPER_PAGE).is_some_and(|entry| entry.dirty);
        if super_dirty {
            self.flush_pages(&[SUPER_PAGE])?;
        }
        Ok(())
    }

    fn flush_pages(&mut self, ids: &[PageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        for id in ids {
            let page = self.cache.get(id).expect("dirty entry").page.clone();
            self.file.seek(SeekFrom::Start(*id * PAGE_SIZE as u64))?;
            self.file.write_all(page.raw())?;
        }
        self.file.sync_data()?;
        for id in ids {
            if let Some(e) = self.cache.get_mut(id) {
                e.dirty = false;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Super page
    // ------------------------------------------------------------------

    fn bootstrap(&mut self) -> Result<()> {
        // Create super page.
        let mut super_page = Page::new(PageType::Super);
        // Stamp magic + version.
        super_page.raw_slice_mut()[SUPER_MAGIC_OFF..SUPER_MAGIC_OFF + 8].copy_from_slice(MAGIC);
        write_u32(super_page.raw_slice_mut(), SUPER_VERSION_OFF, 1);
        write_u64(super_page.raw_slice_mut(), SUPER_PAGE_COUNT_OFF, 1);
        write_u64(super_page.raw_slice_mut(), SUPER_FREE_HEAD_OFF, 0);
        write_u64(super_page.raw_slice_mut(), SUPER_CATALOG_ROOT_OFF, 0);

        self.file.set_len(PAGE_SIZE as u64)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(super_page.raw())?;
        self.file.sync_data()?;
        self.cache.insert(
            SUPER_PAGE,
            CacheEntry {
                page: super_page,
                dirty: false,
            },
        );
        self.page_count = 1;
        self.free_head = 0;
        self.catalog_root = 0;
        Ok(())
    }

    fn read_super(&mut self) -> Result<()> {
        let mut buf = [0u8; PAGE_SIZE];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut buf)?;
        if &buf[SUPER_MAGIC_OFF..SUPER_MAGIC_OFF + 8] != MAGIC {
            return Err(Error::other("bad magic — file is not a toydb database"));
        }
        let version = read_u32(&buf, SUPER_VERSION_OFF);
        if version != 1 {
            return Err(Error::other(format!("unsupported toydb version {version}")));
        }
        self.page_count = read_u64(&buf, SUPER_PAGE_COUNT_OFF);
        self.free_head = read_u64(&buf, SUPER_FREE_HEAD_OFF);
        self.catalog_root = read_u64(&buf, SUPER_CATALOG_ROOT_OFF);
        if self.page_count == 0 {
            return Err(Error::other("invalid super page: page_count is zero"));
        }
        let expected_len = self
            .page_count
            .checked_mul(PAGE_SIZE as u64)
            .ok_or_else(|| Error::other("invalid super page: page_count overflows file size"))?;
        let actual_len = self.file.metadata()?.len();
        if actual_len < expected_len {
            return Err(Error::other(format!(
                "database file is truncated: super page advertises {expected_len} bytes, file has {actual_len}"
            )));
        }
        validate_page_ref(self.free_head, self.page_count, "free list head")?;
        validate_page_ref(self.catalog_root, self.page_count, "catalog root")?;
        let page = Page::from_bytes(buf);
        self.cache
            .insert(SUPER_PAGE, CacheEntry { page, dirty: false });
        Ok(())
    }

    fn write_super(&mut self) -> Result<()> {
        let mut page = self.read_page(SUPER_PAGE)?;
        page.raw_slice_mut()[SUPER_MAGIC_OFF..SUPER_MAGIC_OFF + 8].copy_from_slice(MAGIC);
        write_u32(page.raw_slice_mut(), SUPER_VERSION_OFF, 1);
        write_u64(page.raw_slice_mut(), SUPER_PAGE_COUNT_OFF, self.page_count);
        write_u64(page.raw_slice_mut(), SUPER_FREE_HEAD_OFF, self.free_head);
        write_u64(
            page.raw_slice_mut(),
            SUPER_CATALOG_ROOT_OFF,
            self.catalog_root,
        );
        self.cache
            .insert(SUPER_PAGE, CacheEntry { page, dirty: true });
        Ok(())
    }
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn write_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn read_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn validate_page_ref(id: PageId, page_count: u64, label: &str) -> Result<()> {
    if id != 0 && id >= page_count {
        return Err(Error::other(format!(
            "invalid super page: {label} page {id} is outside page_count {page_count}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        let n = std::process::id();
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = dir.join(format!("toydb-test-{n}-{}-{c}.db", rand_suffix()));
        let mut i = 0;
        while path.exists() {
            i += 1;
            path = dir.join(format!("toydb-test-{n}-{}-{c}-{i}.db", rand_suffix()));
        }
        path
    }

    fn rand_suffix() -> u64 {
        // No `rand` crate — use clock + process counter.
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0)
    }

    #[test]
    fn open_creates_super_page() {
        let path = tmpfile();
        {
            let p = Pager::open(&path).unwrap();
            assert_eq!(p.page_count(), 1);
            assert_eq!(p.catalog_root(), 0);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn allocate_grows_file() {
        let path = tmpfile();
        {
            let mut p = Pager::open(&path).unwrap();
            let id1 = p.allocate(PageType::TableData).unwrap();
            let id2 = p.allocate(PageType::TableData).unwrap();
            assert_eq!(id1, 1);
            assert_eq!(id2, 2);
            p.flush().unwrap();
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn write_then_reread_persists() {
        let path = tmpfile();
        {
            let mut p = Pager::open(&path).unwrap();
            let id = p.allocate(PageType::TableData).unwrap();
            let mut page = p.read_page(id).unwrap();
            page.insert(b"persist me").unwrap();
            p.write_page(id, page).unwrap();
            p.flush().unwrap();
        }
        {
            let mut p = Pager::open(&path).unwrap();
            assert_eq!(p.page_count(), 2);
            let page = p.read_page(1).unwrap();
            assert_eq!(page.get(0), Some(&b"persist me"[..]));
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn deallocate_then_reallocate_reuses() {
        let path = tmpfile();
        {
            let mut p = Pager::open(&path).unwrap();
            let id = p.allocate(PageType::TableData).unwrap();
            p.deallocate(id).unwrap();
            let id2 = p.allocate(PageType::TableData).unwrap();
            assert_eq!(id, id2);
            p.flush().unwrap();
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn deallocate_rejects_double_free() {
        let path = tmpfile();
        {
            let mut p = Pager::open(&path).unwrap();
            let id = p.allocate(PageType::TableData).unwrap();
            p.deallocate(id).unwrap();
            let err = p.deallocate(id).unwrap_err().to_string();
            assert!(err.contains("already free"), "{err}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn deallocate_rejects_current_catalog_root() {
        let path = tmpfile();
        {
            let mut p = Pager::open(&path).unwrap();
            let id = p.allocate(PageType::Catalog).unwrap();
            p.set_catalog_root(id).unwrap();
            let err = p.deallocate(id).unwrap_err().to_string();
            assert!(err.contains("catalog root"), "{err}");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn catalog_root_persists() {
        let path = tmpfile();
        {
            let mut p = Pager::open(&path).unwrap();
            let id = p.allocate(PageType::Catalog).unwrap();
            p.set_catalog_root(id).unwrap();
            p.flush().unwrap();
        }
        {
            let p = Pager::open(&path).unwrap();
            assert_eq!(p.catalog_root(), 1);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_bad_magic() {
        let path = tmpfile();
        std::fs::write(&path, vec![0u8; PAGE_SIZE]).unwrap();
        let r = Pager::open(&path);
        assert!(r.is_err());
        std::fs::remove_file(&path).ok();
    }
}
