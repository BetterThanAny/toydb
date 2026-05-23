//! Disk-backed engine.
//!
//! The catalog is stored as a single linked list of [`PageType::Catalog`]
//! pages whose head is `pager.catalog_root()`. Each entry there is a
//! pair `(table_descriptor, head_data_page)`.
//!
//! Each user table's rows live in a linked list of [`PageType::TableData`]
//! pages. The head page id is stored alongside the table descriptor in
//! the catalog. RowIds encode (page_id, slot) packed into a `u64`.
//!
//! Every mutation goes through the WAL first, so a crash mid-statement
//! is recoverable: replay restores rows + catalog state on the next open.

use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, Index, Table};
use crate::engine::index::IndexStore;
use crate::engine::{Engine, RowId};
use crate::error::{Error, Result};
use crate::storage::encoding::{decode_row, decode_table, encode_row, encode_table};
use crate::storage::page::{PageId, PageType};
use crate::storage::pager::Pager;
use crate::storage::wal::{LogRecord, Wal};
use crate::types::row::Row;
use crate::types::value::Value;

const ROW_ID_PAGE_SHIFT: u32 = 16;
const ROW_ID_SLOT_MASK: u64 = 0xFFFF;

fn make_row_id(page: PageId, slot: u16) -> RowId {
    (page << ROW_ID_PAGE_SHIFT) | (slot as u64)
}

fn split_row_id(id: RowId) -> (PageId, u16) {
    (id >> ROW_ID_PAGE_SHIFT, (id & ROW_ID_SLOT_MASK) as u16)
}

pub struct DiskEngine {
    pager: Pager,
    wal: Wal,
    catalog: Catalog,
    /// Head data page id for each table, mirroring what lives on disk.
    table_heads: std::collections::HashMap<String, PageId>,
    indexes: IndexStore,
}

impl DiskEngine {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let mut pager = Pager::open(&db_path)?;
        let wal_path = wal_path(&db_path);
        let mut wal = Wal::open(&wal_path)?;

        // Replay any prior WAL records onto the data file.
        replay_wal(&mut pager, &mut wal)?;

        // Read catalog.
        let (catalog, table_heads) = read_catalog(&mut pager)?;
        let indexes = rebuild_indexes(&mut pager, &catalog, &table_heads)?;
        Ok(Self {
            pager,
            wal,
            catalog,
            table_heads,
            indexes,
        })
    }

    /// Path to the WAL companion file.
    pub fn wal_path(&self) -> &Path {
        self.wal.path()
    }
    pub fn db_path(&self) -> &Path {
        self.pager.path()
    }

    /// Persist all dirty data pages and truncate the WAL. Call this at
    /// well-defined checkpoints (end of transaction, REPL exit, ...).
    pub fn checkpoint(&mut self) -> Result<()> {
        self.pager.flush()?;
        self.wal.truncate()?;
        Ok(())
    }

    fn rewrite_catalog(&mut self) -> Result<()> {
        write_catalog(&mut self.pager, &self.catalog, &self.table_heads)?;
        self.pager.flush()?;
        Ok(())
    }
}

impl Drop for DiskEngine {
    fn drop(&mut self) {
        // Best effort — Drop swallows errors. The next open() will redo
        // any pending WAL anyway, so we don't lose anything.
        let _ = self.pager.flush();
    }
}

impl Engine for DiskEngine {
    fn create_table(&mut self, table: Table) -> Result<()> {
        if self.catalog.contains(&table.name) {
            return Err(Error::schema(format!(
                "table `{}` already exists",
                table.name
            )));
        }
        // Allocate first data page for the table.
        let head = self.pager.allocate(PageType::TableData)?;
        self.wal.append(&LogRecord::CreateTable(table.clone()))?;
        self.catalog.create_table(table.clone())?;
        for index in &table.indexes {
            self.indexes.rebuild(&index.name, std::iter::empty());
        }
        self.table_heads.insert(table.name.clone(), head);
        self.rewrite_catalog()?;
        Ok(())
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<bool> {
        if !self.catalog.contains(name) {
            if if_exists {
                return Ok(false);
            }
            return Err(Error::schema(format!("table `{name}` does not exist")));
        }
        self.wal.append(&LogRecord::DropTable(name.to_string()))?;
        let table = self.catalog.get(name)?.clone();
        let head = *self
            .table_heads
            .get(name)
            .ok_or_else(|| Error::internal(format!("missing data head for `{name}`")))?;
        // Free the page chain.
        let mut cur = head;
        while cur != 0 {
            let page = self.pager.read_page(cur)?;
            let next = page.next_page();
            self.pager.deallocate(cur)?;
            cur = next;
        }
        self.catalog.drop_table(name)?;
        for index in table.indexes {
            self.indexes.drop(&index.name);
        }
        self.table_heads.remove(name);
        self.rewrite_catalog()?;
        Ok(true)
    }

    fn create_index(&mut self, index: Index) -> Result<()> {
        if self.catalog.index_exists(&index.name) {
            return Err(Error::schema(format!(
                "index `{}` already exists",
                index.name
            )));
        }
        let table = self.catalog.get(&index.table)?.clone();
        let col_idx = table.column_index(&index.column)?;
        self.wal.append(&LogRecord::CreateIndex(index.clone()))?;
        self.catalog.create_index(index.clone())?;
        self.rewrite_catalog()?;
        let rows = scan_table_pages(&mut self.pager, &self.table_heads, &index.table)?;
        self.indexes.rebuild(
            &index.name,
            rows.into_iter()
                .map(|(id, row)| (row.0[col_idx].clone(), id)),
        );
        Ok(())
    }

    fn drop_index(&mut self, name: &str) -> Result<()> {
        let index = self
            .catalog
            .find_index(name)
            .cloned()
            .ok_or_else(|| Error::schema(format!("index `{name}` does not exist")))?;
        self.wal.append(&LogRecord::DropIndex(name.to_string()))?;
        self.catalog.drop_index(name)?;
        self.rewrite_catalog()?;
        self.indexes.drop(&index.name);
        Ok(())
    }

    fn get_table(&self, name: &str) -> Result<&Table> {
        self.catalog.get(name)
    }

    fn list_tables(&self) -> Vec<String> {
        self.catalog.names().cloned().collect()
    }

    fn insert(&mut self, table: &str, row: Row) -> Result<RowId> {
        let table_def = self.catalog.get(table)?.clone();
        let row = validate_row(&table_def, row)?;
        // Unique-check: scan everything. O(n) but obvious.
        check_unique_disk(&mut self.pager, &self.table_heads, &table_def, &row, None)?;
        let mut buf = Vec::new();
        encode_row(&row, &mut buf);
        let head = *self
            .table_heads
            .get(table)
            .ok_or_else(|| Error::internal(format!("missing head for `{table}`")))?;
        // Walk to find a page with room. If none fits, allocate a new
        // page and link it as the new head (we prepend to keep insert
        // O(1) when the head has space).
        let mut cur = head;
        let id = loop {
            let mut page = self.pager.read_page(cur)?;
            if page.free_space() >= buf.len() {
                let slot = page.insert(&buf)?;
                self.pager.write_page(cur, page)?;
                break make_row_id(cur, slot);
            }
            let next = page.next_page();
            if next == 0 {
                // Allocate a new page and chain it.
                let new = self.pager.allocate(PageType::TableData)?;
                let mut new_page = self.pager.read_page(new)?;
                new_page.set_next_page(0);
                let slot = new_page.insert(&buf)?;
                self.pager.write_page(new, new_page)?;
                // Link it: new.next = head, table_heads[table] = new.
                let mut new_page = self.pager.read_page(new)?;
                new_page.set_next_page(head);
                self.pager.write_page(new, new_page)?;
                self.table_heads.insert(table.to_string(), new);
                self.rewrite_catalog()?;
                break make_row_id(new, slot);
            }
            cur = next;
        };
        self.wal.append(&LogRecord::Insert {
            table: table.to_string(),
            id,
            row: row.clone(),
        })?;
        self.pager.flush()?;
        for index in &table_def.indexes {
            let col_idx = table_def.column_index(&index.column)?;
            self.indexes.insert(&index.name, &row.0[col_idx], id);
        }
        Ok(id)
    }

    fn scan(&mut self, table: &str) -> Result<Vec<(RowId, Row)>> {
        let _ = self.catalog.get(table)?;
        scan_table_pages(&mut self.pager, &self.table_heads, table)
    }

    fn update(&mut self, table: &str, id: RowId, row: Row) -> Result<()> {
        let table_def = self.catalog.get(table)?.clone();
        let row = validate_row(&table_def, row)?;
        check_unique_disk(
            &mut self.pager,
            &self.table_heads,
            &table_def,
            &row,
            Some(id),
        )?;
        let (page_id, slot) = split_row_id(id);
        let mut page = self.pager.read_page(page_id)?;
        let old_row = match page.get(slot) {
            Some(bytes) => {
                let mut s = bytes;
                decode_row(&mut s)?
            }
            None => {
                return Err(Error::internal(format!(
                    "update target row {id} no longer exists"
                )));
            }
        };
        let mut buf = Vec::new();
        encode_row(&row, &mut buf);
        page.update(slot, &buf)?;
        self.pager.write_page(page_id, page)?;
        self.wal.append(&LogRecord::Update {
            table: table.to_string(),
            id,
            row: row.clone(),
        })?;
        self.pager.flush()?;
        for index in &table_def.indexes {
            let col_idx = table_def.column_index(&index.column)?;
            self.indexes.remove(&index.name, &old_row.0[col_idx], id);
            self.indexes.insert(&index.name, &row.0[col_idx], id);
        }
        Ok(())
    }

    fn delete(&mut self, table: &str, id: RowId) -> Result<()> {
        let table_def = self.catalog.get(table)?.clone();
        let (page_id, slot) = split_row_id(id);
        let mut page = self.pager.read_page(page_id)?;
        let old_row = match page.get(slot) {
            Some(bytes) => {
                let mut s = bytes;
                Some(decode_row(&mut s)?)
            }
            None => None,
        };
        page.delete(slot)?;
        self.pager.write_page(page_id, page)?;
        self.wal.append(&LogRecord::Delete {
            table: table.to_string(),
            id,
        })?;
        self.pager.flush()?;
        if let Some(old_row) = old_row {
            for index in &table_def.indexes {
                let col_idx = table_def.column_index(&index.column)?;
                self.indexes.remove(&index.name, &old_row.0[col_idx], id);
            }
        }
        Ok(())
    }

    fn get(&mut self, table: &str, id: RowId) -> Result<Option<Row>> {
        let _ = self.catalog.get(table)?;
        let (page_id, slot) = split_row_id(id);
        let page = self.pager.read_page(page_id)?;
        match page.get(slot) {
            None => Ok(None),
            Some(bytes) => {
                let mut s = bytes;
                Ok(Some(decode_row(&mut s)?))
            }
        }
    }

    fn lookup_index(
        &mut self,
        table: &str,
        index: &str,
        value: &Value,
    ) -> Result<Vec<(RowId, Row)>> {
        let meta = self
            .catalog
            .find_index(index)
            .ok_or_else(|| Error::schema(format!("index `{index}` does not exist")))?;
        if meta.table != table {
            return Err(Error::schema(format!(
                "index `{index}` belongs to `{}`, not `{table}`",
                meta.table
            )));
        }
        let mut out = Vec::new();
        for id in self.indexes.lookup(index, value) {
            if let Some(row) = self.get(table, id)? {
                out.push((id, row));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn wal_path(db: &Path) -> PathBuf {
    let mut p = db.to_path_buf();
    let stem = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "toydb".into());
    p.set_file_name(format!("{stem}-wal"));
    p
}

fn validate_row(t: &Table, raw: Row) -> Result<Row> {
    if raw.len() != t.columns.len() {
        return Err(Error::ty(format!(
            "table `{}` expects {} values, got {}",
            t.name,
            t.columns.len(),
            raw.len()
        )));
    }
    let mut out = Vec::with_capacity(raw.len());
    for (col, v) in t.columns.iter().zip(raw.into_inner()) {
        out.push(col.validate(v)?);
    }
    Ok(Row(out))
}

fn check_unique_disk(
    pager: &mut Pager,
    heads: &std::collections::HashMap<String, PageId>,
    table: &Table,
    new: &Row,
    skip: Option<RowId>,
) -> Result<()> {
    let head = *heads
        .get(&table.name)
        .ok_or_else(|| Error::internal(format!("missing head for `{}`", table.name)))?;
    for (col_idx, col) in table.columns.iter().enumerate() {
        if !col.unique && !col.primary_key {
            continue;
        }
        let candidate = &new.0[col_idx];
        if candidate.is_null() {
            continue;
        }
        let mut cur = head;
        while cur != 0 {
            let page = pager.read_page(cur)?;
            for (slot, bytes) in page.iter() {
                let id = make_row_id(cur, slot);
                if Some(id) == skip {
                    continue;
                }
                let mut s = bytes;
                let row = decode_row(&mut s)?;
                if let Some(true) = candidate.equal_sql(&row.0[col_idx])? {
                    return Err(Error::constraint(format!(
                        "duplicate value for unique column `{}`: {}",
                        col.name, candidate
                    )));
                }
            }
            cur = page.next_page();
        }
    }
    Ok(())
}

fn scan_table_pages(
    pager: &mut Pager,
    heads: &std::collections::HashMap<String, PageId>,
    table: &str,
) -> Result<Vec<(RowId, Row)>> {
    let head = *heads
        .get(table)
        .ok_or_else(|| Error::internal(format!("missing head for `{table}`")))?;
    let mut out = Vec::new();
    let mut cur = head;
    while cur != 0 {
        let page = pager.read_page(cur)?;
        for (slot, bytes) in page.iter() {
            let id = make_row_id(cur, slot);
            let mut s = bytes;
            let row = decode_row(&mut s)?;
            out.push((id, row));
        }
        cur = page.next_page();
    }
    Ok(out)
}

fn rebuild_indexes(
    pager: &mut Pager,
    catalog: &Catalog,
    heads: &std::collections::HashMap<String, PageId>,
) -> Result<IndexStore> {
    let mut store = IndexStore::default();
    for (_name, table) in catalog.iter() {
        if table.indexes.is_empty() {
            continue;
        }
        let rows = scan_table_pages(pager, heads, &table.name)?;
        for index in &table.indexes {
            let col_idx = table.column_index(&index.column)?;
            store.rebuild(
                &index.name,
                rows.iter().map(|(id, row)| (row.0[col_idx].clone(), *id)),
            );
        }
    }
    Ok(store)
}

// ---------------------------------------------------------------------
// Catalog encoding (linked list of catalog pages)
// ---------------------------------------------------------------------
//
// Format inside each catalog page slot:
//   u64 head_page_id
//   <encoded Table>
// The catalog itself is the slot iteration of the page chain.

fn read_catalog(pager: &mut Pager) -> Result<(Catalog, std::collections::HashMap<String, PageId>)> {
    let mut catalog = Catalog::new();
    let mut heads = std::collections::HashMap::new();
    let mut cur = pager.catalog_root();
    while cur != 0 {
        let page = pager.read_page(cur)?;
        for (_slot, bytes) in page.iter() {
            let mut s = bytes;
            if s.len() < 8 {
                return Err(Error::other("catalog: truncated entry"));
            }
            let head = u64::from_le_bytes(s[..8].try_into().unwrap());
            s = &s[8..];
            let table = decode_table(&mut s)?;
            heads.insert(table.name.clone(), head);
            catalog.create_table(table)?;
        }
        cur = page.next_page();
    }
    Ok((catalog, heads))
}

fn write_catalog(
    pager: &mut Pager,
    catalog: &Catalog,
    heads: &std::collections::HashMap<String, PageId>,
) -> Result<()> {
    // Free old catalog chain.
    let mut cur = pager.catalog_root();
    while cur != 0 {
        let p = pager.read_page(cur)?;
        let next = p.next_page();
        pager.deallocate(cur)?;
        cur = next;
    }
    pager.set_catalog_root(0)?;
    if catalog.is_empty() {
        return Ok(());
    }
    // Build new chain.
    let mut head_id: PageId = 0;
    for (name, table) in catalog.iter() {
        let head_data = *heads
            .get(name)
            .ok_or_else(|| Error::internal(format!("no head for `{name}`")))?;
        let mut entry = Vec::new();
        entry.extend_from_slice(&head_data.to_le_bytes());
        encode_table(table, &mut entry);
        // Try to fit in the head page; else allocate a new page and prepend.
        if head_id == 0 {
            let new = pager.allocate(PageType::Catalog)?;
            let mut np = pager.read_page(new)?;
            np.set_next_page(0);
            np.insert(&entry)?;
            pager.write_page(new, np)?;
            head_id = new;
            pager.set_catalog_root(head_id)?;
        } else {
            let mut placed = false;
            // Try existing head first.
            let mut hp = pager.read_page(head_id)?;
            if hp.free_space() >= entry.len() {
                hp.insert(&entry)?;
                pager.write_page(head_id, hp)?;
                placed = true;
            }
            if !placed {
                let new = pager.allocate(PageType::Catalog)?;
                let mut np = pager.read_page(new)?;
                np.set_next_page(head_id);
                np.insert(&entry)?;
                pager.write_page(new, np)?;
                head_id = new;
                pager.set_catalog_root(head_id)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// WAL replay
// ---------------------------------------------------------------------

fn replay_wal(pager: &mut Pager, wal: &mut Wal) -> Result<()> {
    let recs = wal.replay()?;
    if recs.is_empty() {
        return Ok(());
    }
    // Replay step-by-step. We carefully avoid going through `Engine`
    // here because that would write more WAL records.
    let (mut catalog, mut heads) = read_catalog(pager)?;
    for (idx, rec) in recs.iter().enumerate() {
        match rec {
            LogRecord::CreateTable(t) => {
                if catalog.contains(&t.name) {
                    continue; // idempotent
                }
                let head = pager.allocate(PageType::TableData)?;
                catalog.create_table(t.clone())?;
                heads.insert(t.name.clone(), head);
            }
            LogRecord::DropTable(name) => {
                if !catalog.contains(name) {
                    continue;
                }
                let head = *heads
                    .get(name)
                    .ok_or_else(|| Error::internal(format!("replay: missing head for `{name}`")))?;
                let mut cur = head;
                while cur != 0 {
                    let page = pager.read_page(cur)?;
                    let next = page.next_page();
                    pager.deallocate(cur)?;
                    cur = next;
                }
                catalog.drop_table(name)?;
                heads.remove(name);
            }
            LogRecord::CreateIndex(index) => {
                if !catalog.contains(&index.table) || catalog.index_exists(&index.name) {
                    continue;
                }
                catalog.create_index(index.clone())?;
            }
            LogRecord::DropIndex(name) => {
                if catalog.index_exists(name) {
                    catalog.drop_index(name)?;
                }
            }
            LogRecord::Insert { table, id, row } => {
                if !catalog.contains(table) {
                    // Table was dropped later in the WAL; the
                    // subsequent DropTable record will free its pages.
                    continue;
                }
                let (page_id, expected_slot) = split_row_id(*id);
                let mut buf = Vec::new();
                encode_row(row, &mut buf);
                let mut page = pager.read_page(page_id)?;
                // Idempotent replay: this record has already been applied
                // if the slot already holds the same bytes (the common
                // case is that the prior `Drop` impl flushed pages but
                // didn't truncate the WAL). Skip rather than reinsert.
                if page.get(expected_slot) == Some(buf.as_slice()) {
                    continue;
                }
                if page.get(expected_slot).is_some()
                    && has_later_record_for_row(&recs, idx, table, *id)
                {
                    continue;
                }
                page.insert_at(expected_slot, &buf)?;
                pager.write_page(page_id, page)?;
            }
            LogRecord::Update { table, id, row } => {
                if !catalog.contains(table) {
                    continue;
                }
                let (page_id, slot) = split_row_id(*id);
                let mut buf = Vec::new();
                encode_row(row, &mut buf);
                let mut page = pager.read_page(page_id)?;
                if page.get(slot) == Some(buf.as_slice()) {
                    continue; // already applied
                }
                if has_later_record_for_row(&recs, idx, table, *id) {
                    continue;
                }
                page.update(slot, &buf)?;
                pager.write_page(page_id, page)?;
            }
            LogRecord::Delete { table, id } => {
                if !catalog.contains(table) {
                    continue;
                }
                let (page_id, slot) = split_row_id(*id);
                let mut page = pager.read_page(page_id)?;
                if page.get(slot).is_none() {
                    continue; // already tombstoned
                }
                page.delete(slot)?;
                pager.write_page(page_id, page)?;
            }
        }
    }
    write_catalog(pager, &catalog, &heads)?;
    pager.flush()?;
    wal.truncate()?;
    Ok(())
}

fn has_later_record_for_row(recs: &[LogRecord], idx: usize, table: &str, id: RowId) -> bool {
    recs[idx + 1..].iter().any(|rec| match rec {
        LogRecord::Insert {
            table: later_table,
            id: later_id,
            ..
        }
        | LogRecord::Update {
            table: later_table,
            id: later_id,
            ..
        }
        | LogRecord::Delete {
            table: later_table,
            id: later_id,
        } => later_table == table && *later_id == id,
        LogRecord::DropTable(name) => name == table,
        LogRecord::CreateTable(_) | LogRecord::CreateIndex(_) | LogRecord::DropIndex(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Column;
    use crate::sql::ast::DataType;
    use crate::types::value::Value;

    fn tmpdb() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("toydb-disk-{}-{n}-{c}.db", std::process::id()))
    }

    fn cleanup(p: &Path) {
        std::fs::remove_file(p).ok();
        std::fs::remove_file(wal_path(p)).ok();
    }

    fn users_table() -> Table {
        Table::new(
            "users",
            vec![
                Column::new("id", DataType::Integer).primary_key(),
                Column::new("name", DataType::String).not_null(),
            ],
        )
        .unwrap()
    }

    #[test]
    fn open_create_close_reopen() {
        let path = tmpdb();
        {
            let mut e = DiskEngine::open(&path).unwrap();
            e.create_table(users_table()).unwrap();
            e.checkpoint().unwrap();
        }
        {
            let e = DiskEngine::open(&path).unwrap();
            assert!(e.get_table("users").is_ok());
        }
        cleanup(&path);
    }

    #[test]
    fn insert_and_scan_persists() {
        let path = tmpdb();
        {
            let mut e = DiskEngine::open(&path).unwrap();
            e.create_table(users_table()).unwrap();
            e.insert(
                "users",
                Row(vec![Value::Integer(1), Value::String("alice".into())]),
            )
            .unwrap();
            e.insert(
                "users",
                Row(vec![Value::Integer(2), Value::String("bob".into())]),
            )
            .unwrap();
            e.checkpoint().unwrap();
        }
        {
            let mut e = DiskEngine::open(&path).unwrap();
            let rows = e.scan("users").unwrap();
            assert_eq!(rows.len(), 2);
        }
        cleanup(&path);
    }

    #[test]
    fn update_and_delete_persist() {
        let path = tmpdb();
        let id = {
            let mut e = DiskEngine::open(&path).unwrap();
            e.create_table(users_table()).unwrap();
            let id = e
                .insert(
                    "users",
                    Row(vec![Value::Integer(1), Value::String("alice".into())]),
                )
                .unwrap();
            e.update(
                "users",
                id,
                Row(vec![Value::Integer(1), Value::String("ALICE".into())]),
            )
            .unwrap();
            e.checkpoint().unwrap();
            id
        };
        {
            let mut e = DiskEngine::open(&path).unwrap();
            let v = e.get("users", id).unwrap().unwrap();
            assert_eq!(v[1], Value::String("ALICE".into()));
        }
        {
            let mut e = DiskEngine::open(&path).unwrap();
            e.delete("users", id).unwrap();
            e.checkpoint().unwrap();
        }
        {
            let mut e = DiskEngine::open(&path).unwrap();
            assert!(e.scan("users").unwrap().is_empty());
        }
        cleanup(&path);
    }

    #[test]
    fn drop_table_removes_pages() {
        let path = tmpdb();
        {
            let mut e = DiskEngine::open(&path).unwrap();
            e.create_table(users_table()).unwrap();
            e.insert(
                "users",
                Row(vec![Value::Integer(1), Value::String("a".into())]),
            )
            .unwrap();
            assert!(e.drop_table("users", false).unwrap());
            e.checkpoint().unwrap();
        }
        {
            let e = DiskEngine::open(&path).unwrap();
            assert!(e.get_table("users").is_err());
        }
        cleanup(&path);
    }

    #[test]
    fn unique_constraint_enforced_on_disk() {
        let path = tmpdb();
        {
            let mut e = DiskEngine::open(&path).unwrap();
            e.create_table(users_table()).unwrap();
            e.insert(
                "users",
                Row(vec![Value::Integer(1), Value::String("a".into())]),
            )
            .unwrap();
            let err = e
                .insert(
                    "users",
                    Row(vec![Value::Integer(1), Value::String("b".into())]),
                )
                .unwrap_err();
            assert!(err.to_string().contains("duplicate"));
        }
        cleanup(&path);
    }

    #[test]
    fn wal_replays_when_no_checkpoint() {
        let path = tmpdb();
        {
            let mut e = DiskEngine::open(&path).unwrap();
            e.create_table(users_table()).unwrap();
            e.insert(
                "users",
                Row(vec![Value::Integer(1), Value::String("a".into())]),
            )
            .unwrap();
            e.insert(
                "users",
                Row(vec![Value::Integer(2), Value::String("b".into())]),
            )
            .unwrap();
            // Note: no explicit checkpoint. Drop runs flush via Drop impl,
            // but the WAL still contains the records — replay should be a
            // no-op here because we already flushed pages through `insert`.
        }
        {
            let mut e = DiskEngine::open(&path).unwrap();
            let rows = e.scan("users").unwrap();
            assert_eq!(rows.len(), 2);
        }
        cleanup(&path);
    }

    #[test]
    fn many_rows_spill_to_new_page() {
        let path = tmpdb();
        let big_str = "x".repeat(200);
        {
            let mut e = DiskEngine::open(&path).unwrap();
            e.create_table(
                Table::new(
                    "t",
                    vec![
                        Column::new("id", DataType::Integer).primary_key(),
                        Column::new("payload", DataType::String),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
            for i in 0..50 {
                e.insert(
                    "t",
                    Row(vec![Value::Integer(i), Value::String(big_str.clone())]),
                )
                .unwrap();
            }
            e.checkpoint().unwrap();
        }
        {
            let mut e = DiskEngine::open(&path).unwrap();
            assert_eq!(e.scan("t").unwrap().len(), 50);
        }
        cleanup(&path);
    }
}
