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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, Index, Table};
use crate::engine::index::IndexStore;
use crate::engine::{Engine, RowId};
use crate::error::{Error, Result};
use crate::storage::encoding::{decode_row, decode_table, encode_row, encode_table};
use crate::storage::page::{HEADER_SIZE, PAGE_SIZE, Page, PageId, PageType, SLOT_SIZE};
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

    fn insert_many(&mut self, table: &str, rows: Vec<Row>) -> Result<Vec<RowId>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let table_def = self.catalog.get(table)?.clone();
        let mut validated = Vec::with_capacity(rows.len());
        for row in rows {
            let row = validate_row(&table_def, row)?;
            validate_row_size(&table_def, &row)?;
            validated.push(row);
        }

        let existing = scan_table_pages(&mut self.pager, &self.table_heads, table)?;
        check_unique_inserts_disk(&table_def, &existing, &validated)?;

        let plan = plan_insert_pages(&mut self.pager, &self.table_heads, table, &validated)?;
        self.wal.append(&LogRecord::InsertBatch {
            table: table.to_string(),
            rows: plan.records.clone(),
        })?;

        for page_id in &plan.new_page_ids {
            self.pager
                .ensure_page_id(*page_id, PageType::TableData, true)?;
        }
        for (page_id, page) in plan.pages {
            self.pager.write_page(page_id, page)?;
        }

        let old_head = *self
            .table_heads
            .get(table)
            .ok_or_else(|| Error::internal(format!("missing head for `{table}`")))?;
        if plan.head != old_head {
            self.table_heads.insert(table.to_string(), plan.head);
            self.rewrite_catalog()?;
        } else {
            self.pager.flush()?;
        }

        for index in &table_def.indexes {
            let col_idx = table_def.column_index(&index.column)?;
            for (id, row) in &plan.records {
                self.indexes.insert(&index.name, &row.0[col_idx], *id);
            }
        }
        Ok(plan.records.into_iter().map(|(id, _)| id).collect())
    }
}

impl Drop for DiskEngine {
    fn drop(&mut self) {
        // Best effort — Drop swallows errors. The next open() will redo
        // any pending WAL anyway, so we don't lose anything. If flushing
        // succeeds, truncate the WAL so clean exits do not accumulate replay
        // work forever.
        if self.pager.flush().is_ok() {
            let _ = self.wal.truncate();
        }
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
        validate_catalog_entry_fits(&table)?;
        // Allocate first data page for the table.
        let head = self.pager.allocate(PageType::TableData)?;

        let mut next_catalog = self.catalog.clone();
        let mut next_heads = self.table_heads.clone();
        next_catalog.create_table(table.clone())?;
        next_heads.insert(table.name.clone(), head);
        validate_catalog_entries_fit(&next_catalog, &next_heads)?;

        self.wal.append(&LogRecord::CreateTable(table.clone()))?;
        write_catalog(&mut self.pager, &next_catalog, &next_heads)?;
        self.catalog = next_catalog;
        self.table_heads = next_heads;
        for index in &table.indexes {
            self.indexes.rebuild(&index.name, std::iter::empty());
        }
        Ok(())
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<bool> {
        if !self.catalog.contains(name) {
            if if_exists {
                return Ok(false);
            }
            return Err(Error::schema(format!("table `{name}` does not exist")));
        }
        let table = self.catalog.get(name)?.clone();
        let head = *self
            .table_heads
            .get(name)
            .ok_or_else(|| Error::internal(format!("missing data head for `{name}`")))?;
        let pages = collect_chain_pages(&mut self.pager, head, name)?;
        self.wal.append(&LogRecord::DropTablePages {
            table: name.to_string(),
            pages: pages.clone(),
        })?;
        // Free the page chain.
        for cur in pages {
            self.pager.deallocate(cur)?;
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

        let mut next_catalog = self.catalog.clone();
        next_catalog.create_index(index.clone())?;
        validate_catalog_entries_fit(&next_catalog, &self.table_heads)?;
        let rows = scan_table_pages(&mut self.pager, &self.table_heads, &index.table)?;

        self.wal.append(&LogRecord::CreateIndex(index.clone()))?;
        write_catalog(&mut self.pager, &next_catalog, &self.table_heads)?;
        self.catalog = next_catalog;
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
        let mut next_catalog = self.catalog.clone();
        next_catalog.drop_index(name)?;
        validate_catalog_entries_fit(&next_catalog, &self.table_heads)?;
        self.wal.append(&LogRecord::DropIndex(name.to_string()))?;
        write_catalog(&mut self.pager, &next_catalog, &self.table_heads)?;
        self.catalog = next_catalog;
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
        let mut ids = self.insert_many(table, vec![row])?;
        ids.pop()
            .ok_or_else(|| Error::internal("single-row insert produced no row id"))
    }

    fn insert_batch(&mut self, table: &str, rows: Vec<Row>) -> Result<Vec<RowId>> {
        self.insert_many(table, rows)
    }

    fn scan(&mut self, table: &str) -> Result<Vec<(RowId, Row)>> {
        let _ = self.catalog.get(table)?;
        scan_table_pages(&mut self.pager, &self.table_heads, table)
    }

    fn preflight_update_batch(&mut self, table: &str, updates: &[(RowId, Row)]) -> Result<()> {
        let table_def = self.catalog.get(table)?.clone();
        let validated = validate_update_rows(&table_def, updates)?;
        check_unique_updates_disk(&mut self.pager, &self.table_heads, &table_def, &validated)?;
        preflight_update_pages(&mut self.pager, &validated)?;
        Ok(())
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
        self.update_unchecked(table, &table_def, id, row)
    }

    fn update_batch(&mut self, table: &str, updates: &[(RowId, Row)]) -> Result<()> {
        let table_def = self.catalog.get(table)?.clone();
        let validated = validate_update_rows(&table_def, updates)?;
        check_unique_updates_disk(&mut self.pager, &self.table_heads, &table_def, &validated)?;
        let plan = plan_update_pages(&mut self.pager, &validated)?;
        self.wal.append(&LogRecord::UpdateBatch {
            table: table.to_string(),
            rows: validated.clone(),
        })?;
        for (page_id, page) in plan.pages {
            self.pager.write_page(page_id, page)?;
        }
        self.pager.flush()?;
        for (id, old_row, new_row) in plan.changed {
            for index in &table_def.indexes {
                let col_idx = table_def.column_index(&index.column)?;
                self.indexes.remove(&index.name, &old_row.0[col_idx], id);
                self.indexes.insert(&index.name, &new_row.0[col_idx], id);
            }
        }
        Ok(())
    }

    fn delete_batch(&mut self, table: &str, ids: &[RowId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let table_def = self.catalog.get(table)?.clone();
        let plan = plan_delete_pages(&mut self.pager, ids)?;
        self.wal.append(&LogRecord::DeleteBatch {
            table: table.to_string(),
            ids: ids.to_vec(),
        })?;
        for (page_id, page) in plan.pages {
            self.pager.write_page(page_id, page)?;
        }
        self.pager.flush()?;
        for (id, old_row) in plan.deleted {
            for index in &table_def.indexes {
                let col_idx = table_def.column_index(&index.column)?;
                self.indexes.remove(&index.name, &old_row.0[col_idx], id);
            }
        }
        Ok(())
    }

    fn delete(&mut self, table: &str, id: RowId) -> Result<()> {
        self.delete_batch(table, &[id])
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

impl DiskEngine {
    fn update_unchecked(
        &mut self,
        table: &str,
        table_def: &Table,
        id: RowId,
        row: Row,
    ) -> Result<()> {
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
        self.wal.append(&LogRecord::Update {
            table: table.to_string(),
            id,
            row: row.clone(),
        })?;
        self.pager.write_page(page_id, page)?;
        self.pager.flush()?;
        for index in &table_def.indexes {
            let col_idx = table_def.column_index(&index.column)?;
            self.indexes.remove(&index.name, &old_row.0[col_idx], id);
            self.indexes.insert(&index.name, &row.0[col_idx], id);
        }
        Ok(())
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

fn validate_update_rows(table: &Table, updates: &[(RowId, Row)]) -> Result<Vec<(RowId, Row)>> {
    let mut seen_targets = HashSet::new();
    let mut validated = Vec::with_capacity(updates.len());
    for (id, row) in updates {
        if !seen_targets.insert(*id) {
            return Err(Error::internal(format!("duplicate update target row {id}")));
        }
        let row = validate_row(table, row.clone())?;
        validate_row_size(table, &row)?;
        validated.push((*id, row));
    }
    Ok(validated)
}

fn validate_row_size(table: &Table, row: &Row) -> Result<()> {
    let mut encoded = Vec::new();
    encode_row(row, &mut encoded);
    if encoded.len() > crate::storage::page::PAGE_SIZE - crate::storage::page::HEADER_SIZE {
        return Err(Error::other(format!(
            "row for table `{}` is too large for a page: {} bytes",
            table.name,
            encoded.len()
        )));
    }
    Ok(())
}

struct InsertPlan {
    records: Vec<(RowId, Row)>,
    pages: HashMap<PageId, Page>,
    new_page_ids: Vec<PageId>,
    head: PageId,
}

struct UpdatePlan {
    changed: Vec<(RowId, Row, Row)>,
    pages: HashMap<PageId, Page>,
}

struct DeletePlan {
    deleted: Vec<(RowId, Row)>,
    pages: HashMap<PageId, Page>,
}

fn plan_insert_pages(
    pager: &mut Pager,
    heads: &HashMap<String, PageId>,
    table: &str,
    rows: &[Row],
) -> Result<InsertPlan> {
    let original_head = *heads
        .get(table)
        .ok_or_else(|| Error::internal(format!("missing head for `{table}`")))?;
    let mut head = original_head;
    let mut pages: HashMap<PageId, Page> = HashMap::new();
    let mut new_page_ids = Vec::new();
    let mut next_new_page = pager.page_count();
    let mut records = Vec::with_capacity(rows.len());

    for row in rows {
        let mut buf = Vec::new();
        encode_row(row, &mut buf);
        let mut cur = head;
        let mut seen = HashSet::new();
        let id = loop {
            if !seen.insert(cur) {
                return Err(Error::other(format!(
                    "page chain for `{table}` contains a cycle at page {cur}"
                )));
            }
            let page = match pages.entry(cur) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(pager.read_page(cur)?)
                }
            };
            if let Ok(slot) = page.insert(&buf) {
                break make_row_id(cur, slot);
            }
            let next = page.next_page();
            if next == 0 {
                let new = next_new_page;
                next_new_page += 1;
                let mut new_page = Page::new(PageType::TableData);
                let slot = new_page.insert(&buf)?;
                new_page.set_next_page(head);
                pages.insert(new, new_page);
                new_page_ids.push(new);
                head = new;
                break make_row_id(new, slot);
            }
            cur = next;
        };
        records.push((id, row.clone()));
    }

    Ok(InsertPlan {
        records,
        pages,
        new_page_ids,
        head,
    })
}

fn preflight_update_pages(pager: &mut Pager, updates: &[(RowId, Row)]) -> Result<()> {
    plan_update_pages(pager, updates).map(|_| ())
}

fn plan_update_pages(pager: &mut Pager, updates: &[(RowId, Row)]) -> Result<UpdatePlan> {
    let mut pages = HashMap::new();
    let mut changed = Vec::with_capacity(updates.len());
    for (id, row) in updates {
        let (page_id, slot) = split_row_id(*id);
        let page = match pages.entry(page_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(pager.read_page(page_id)?)
            }
        };
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
        encode_row(row, &mut buf);
        page.update(slot, &buf)?;
        changed.push((*id, old_row, row.clone()));
    }
    Ok(UpdatePlan { changed, pages })
}

fn plan_delete_pages(pager: &mut Pager, ids: &[RowId]) -> Result<DeletePlan> {
    let mut pages = HashMap::new();
    let mut deleted = Vec::new();
    for id in ids {
        let (page_id, slot) = split_row_id(*id);
        let page = match pages.entry(page_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(pager.read_page(page_id)?)
            }
        };
        if let Some(bytes) = page.get(slot) {
            let mut s = bytes;
            deleted.push((*id, decode_row(&mut s)?));
        }
        page.delete(slot)?;
    }
    Ok(DeletePlan { deleted, pages })
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

fn check_unique_inserts_disk(table: &Table, existing: &[(RowId, Row)], rows: &[Row]) -> Result<()> {
    for (col_idx, col) in table.columns.iter().enumerate() {
        if !col.unique && !col.primary_key {
            continue;
        }
        for (idx, row) in rows.iter().enumerate() {
            let candidate = &row.0[col_idx];
            if candidate.is_null() {
                continue;
            }
            for (_, existing_row) in existing {
                if let Some(true) = candidate.equal_sql(&existing_row.0[col_idx])? {
                    return Err(Error::constraint(format!(
                        "duplicate value for unique column `{}`: {}",
                        col.name, candidate
                    )));
                }
            }
            for prior in &rows[..idx] {
                if let Some(true) = candidate.equal_sql(&prior.0[col_idx])? {
                    return Err(Error::constraint(format!(
                        "duplicate value for unique column `{}`: {}",
                        col.name, candidate
                    )));
                }
            }
        }
    }
    Ok(())
}

fn check_unique_updates_disk(
    pager: &mut Pager,
    heads: &HashMap<String, PageId>,
    table: &Table,
    updates: &[(RowId, Row)],
) -> Result<()> {
    let head = *heads
        .get(&table.name)
        .ok_or_else(|| Error::internal(format!("missing head for `{}`", table.name)))?;
    let updating_ids: HashSet<RowId> = updates.iter().map(|(id, _)| *id).collect();
    for (col_idx, col) in table.columns.iter().enumerate() {
        if !col.unique && !col.primary_key {
            continue;
        }
        for (idx, (_id, row)) in updates.iter().enumerate() {
            let candidate = &row.0[col_idx];
            if candidate.is_null() {
                continue;
            }
            let mut cur = head;
            while cur != 0 {
                let page = pager.read_page(cur)?;
                for (slot, bytes) in page.iter() {
                    let existing_id = make_row_id(cur, slot);
                    if updating_ids.contains(&existing_id) {
                        continue;
                    }
                    let mut s = bytes;
                    let existing_row = decode_row(&mut s)?;
                    if let Some(true) = candidate.equal_sql(&existing_row.0[col_idx])? {
                        return Err(Error::constraint(format!(
                            "duplicate value for unique column `{}`: {}",
                            col.name, candidate
                        )));
                    }
                }
                cur = page.next_page();
            }
            for (_, prior) in &updates[..idx] {
                if let Some(true) = candidate.equal_sql(&prior.0[col_idx])? {
                    return Err(Error::constraint(format!(
                        "duplicate value for unique column `{}`: {}",
                        col.name, candidate
                    )));
                }
            }
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

fn collect_chain_pages(pager: &mut Pager, head: PageId, label: &str) -> Result<Vec<PageId>> {
    let mut out = Vec::new();
    let mut cur = head;
    let mut seen = HashSet::new();
    while cur != 0 {
        if !seen.insert(cur) {
            return Err(Error::other(format!(
                "page chain `{label}` contains a cycle at page {cur}"
            )));
        }
        let page = pager.read_page(cur)?;
        out.push(cur);
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
    let mut seen = HashSet::new();
    while cur != 0 {
        if !seen.insert(cur) {
            return Err(Error::other(format!(
                "catalog page chain contains a cycle at page {cur}"
            )));
        }
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
    // Start from a stable page cache before writing the replacement catalog
    // chain. This keeps unrelated dirty data pages out of the root-switch
    // flushes below.
    pager.flush()?;
    let old_root = pager.catalog_root();
    let new_root = build_catalog_chain(pager, catalog, heads)?;

    // Make the new catalog pages durable before publishing the new root.
    pager.flush()?;
    pager.set_catalog_root(new_root)?;
    pager.flush()?;

    // The old chain is no longer reachable. Free it after the root switch;
    // a crash before this point can at worst leak pages, not lose catalog.
    free_catalog_chain(pager, old_root)?;
    pager.flush()?;
    Ok(())
}

fn validate_catalog_entries_fit(
    catalog: &Catalog,
    heads: &std::collections::HashMap<String, PageId>,
) -> Result<()> {
    for (name, table) in catalog.iter() {
        let head = *heads
            .get(name)
            .ok_or_else(|| Error::internal(format!("no head for `{name}`")))?;
        validate_catalog_entry_bytes(name, head, table)?;
    }
    Ok(())
}

fn validate_catalog_entry_fits(table: &Table) -> Result<()> {
    validate_catalog_entry_bytes(&table.name, 0, table)
}

fn validate_catalog_entry_bytes(name: &str, head: PageId, table: &Table) -> Result<()> {
    let mut entry = Vec::new();
    entry.extend_from_slice(&head.to_le_bytes());
    encode_table(table, &mut entry)?;
    let max_entry = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;
    if entry.len() > max_entry {
        return Err(Error::value(format!(
            "catalog entry for table `{name}` is too large: {} bytes encoded, max {}",
            entry.len(),
            max_entry
        )));
    }
    Ok(())
}

fn build_catalog_chain(
    pager: &mut Pager,
    catalog: &Catalog,
    heads: &std::collections::HashMap<String, PageId>,
) -> Result<PageId> {
    if catalog.is_empty() {
        return Ok(0);
    }

    let mut head_id: PageId = 0;
    let mut current: Option<(PageId, crate::storage::page::Page)> = None;
    for (name, table) in catalog.iter() {
        let head_data = *heads
            .get(name)
            .ok_or_else(|| Error::internal(format!("no head for `{name}`")))?;
        let mut entry = Vec::new();
        entry.extend_from_slice(&head_data.to_le_bytes());
        encode_table(table, &mut entry)?;

        match current.as_mut() {
            Some((_id, page)) if page.free_space() >= entry.len() => {
                page.insert(&entry)?;
            }
            _ => {
                if let Some((id, page)) = current.take() {
                    pager.write_page(id, page)?;
                }
                // Prepend a fresh catalog page. We deliberately do not free
                // the old catalog chain until after the new root is durable.
                let new = pager.allocate(PageType::Catalog)?;
                let mut np = pager.read_page(new)?;
                np.set_next_page(head_id);
                np.insert(&entry)?;
                head_id = new;
                current = Some((new, np));
            }
        }
    }
    if let Some((id, page)) = current.take() {
        pager.write_page(id, page)?;
    }
    Ok(head_id)
}

fn free_catalog_chain(pager: &mut Pager, root: PageId) -> Result<()> {
    let mut cur = root;
    let mut seen = HashSet::new();
    while cur != 0 {
        if !seen.insert(cur) {
            return Err(Error::other(format!(
                "catalog page chain contains a cycle at page {cur}"
            )));
        }
        let p = pager.read_page(cur)?;
        let next = p.next_page();
        pager.deallocate(cur)?;
        cur = next;
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
                if current_table_matches_later_create(&catalog, &recs, idx, name) {
                    continue;
                }
                let head = *heads
                    .get(name)
                    .ok_or_else(|| Error::internal(format!("replay: missing head for `{name}`")))?;
                let mut cur = head;
                let mut seen = HashSet::new();
                while cur != 0 {
                    if !seen.insert(cur) {
                        return Err(Error::other(format!(
                            "replay: page chain for `{name}` contains a cycle at page {cur}"
                        )));
                    }
                    let page = pager.read_page(cur)?;
                    let next = page.next_page();
                    if page.page_type()? != PageType::Free {
                        pager.deallocate(cur)?;
                    }
                    cur = next;
                }
                catalog.drop_table(name)?;
                heads.remove(name);
            }
            LogRecord::DropTablePages { table, pages } => {
                if !catalog.contains(table) {
                    continue;
                }
                if current_table_matches_later_create(&catalog, &recs, idx, table) {
                    continue;
                }
                for page_id in pages {
                    if *page_id == 0 || *page_id >= pager.page_count() {
                        continue;
                    }
                    let page = pager.read_page(*page_id)?;
                    if page.page_type()? != PageType::Free {
                        pager.deallocate(*page_id)?;
                    }
                }
                catalog.drop_table(table)?;
                heads.remove(table);
            }
            LogRecord::CreateIndex(index) => {
                if has_later_drop_for_table(&recs, idx, &index.table) {
                    continue;
                }
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
                if has_later_drop_for_table(&recs, idx, table) {
                    continue;
                }
                if !catalog.contains(table) {
                    // Table was dropped later in the WAL; the
                    // subsequent DropTable record will free its pages.
                    continue;
                }
                let (page_id, expected_slot) = split_row_id(*id);
                let mut buf = Vec::new();
                encode_row(row, &mut buf);
                ensure_replay_page_in_chain(pager, &mut heads, table, page_id)?;
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
            LogRecord::InsertBatch { table, rows } => {
                if has_later_drop_for_table(&recs, idx, table) {
                    continue;
                }
                if !catalog.contains(table) {
                    continue;
                }
                for (id, row) in rows {
                    let (page_id, expected_slot) = split_row_id(*id);
                    let mut buf = Vec::new();
                    encode_row(row, &mut buf);
                    ensure_replay_page_in_chain(pager, &mut heads, table, page_id)?;
                    let mut page = pager.read_page(page_id)?;
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
            }
            LogRecord::Update { table, id, row } => {
                if has_later_drop_for_table(&recs, idx, table) {
                    continue;
                }
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
            LogRecord::UpdateBatch { table, rows } => {
                if has_later_drop_for_table(&recs, idx, table) {
                    continue;
                }
                if !catalog.contains(table) {
                    continue;
                }
                for (id, row) in rows {
                    let (page_id, slot) = split_row_id(*id);
                    let mut buf = Vec::new();
                    encode_row(row, &mut buf);
                    let mut page = pager.read_page(page_id)?;
                    if page.get(slot) == Some(buf.as_slice()) {
                        continue;
                    }
                    if has_later_record_for_row(&recs, idx, table, *id) {
                        continue;
                    }
                    page.update(slot, &buf)?;
                    pager.write_page(page_id, page)?;
                }
            }
            LogRecord::Delete { table, id } => {
                if has_later_drop_for_table(&recs, idx, table) {
                    continue;
                }
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
            LogRecord::DeleteBatch { table, ids } => {
                if has_later_drop_for_table(&recs, idx, table) {
                    continue;
                }
                if !catalog.contains(table) {
                    continue;
                }
                for id in ids {
                    let (page_id, slot) = split_row_id(*id);
                    let mut page = pager.read_page(page_id)?;
                    if page.get(slot).is_none() {
                        continue;
                    }
                    page.delete(slot)?;
                    pager.write_page(page_id, page)?;
                }
            }
        }
    }
    write_catalog(pager, &catalog, &heads)?;
    pager.flush()?;
    wal.truncate()?;
    Ok(())
}

fn has_later_drop_for_table(recs: &[LogRecord], idx: usize, table: &str) -> bool {
    recs[idx + 1..].iter().any(|rec| match rec {
        LogRecord::DropTable(name) => name == table,
        LogRecord::DropTablePages { table: name, .. } => name == table,
        _ => false,
    })
}

fn current_table_matches_later_create(
    catalog: &Catalog,
    recs: &[LogRecord],
    idx: usize,
    table: &str,
) -> bool {
    let Ok(current) = catalog.get(table) else {
        return false;
    };
    recs[idx + 1..].iter().any(|rec| match rec {
        LogRecord::CreateTable(later) if later.name == table => {
            same_table_generation(current, later)
        }
        _ => false,
    })
}

fn same_table_generation(current: &Table, create_record: &Table) -> bool {
    current.name == create_record.name
        && current.columns == create_record.columns
        && current.primary_key == create_record.primary_key
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
        LogRecord::InsertBatch {
            table: later_table,
            rows,
        } => later_table == table && rows.iter().any(|(later_id, _)| *later_id == id),
        LogRecord::UpdateBatch {
            table: later_table,
            rows,
        } => later_table == table && rows.iter().any(|(later_id, _)| *later_id == id),
        LogRecord::DeleteBatch {
            table: later_table,
            ids,
        } => later_table == table && ids.contains(&id),
        LogRecord::DropTable(name) => name == table,
        LogRecord::DropTablePages { table: name, .. } => name == table,
        LogRecord::CreateTable(_) | LogRecord::CreateIndex(_) | LogRecord::DropIndex(_) => false,
    })
}

fn ensure_replay_page_in_chain(
    pager: &mut Pager,
    heads: &mut std::collections::HashMap<String, PageId>,
    table: &str,
    page_id: PageId,
) -> Result<()> {
    if page_in_chain(pager, heads, table, page_id)? {
        pager.ensure_page_id(page_id, PageType::TableData, false)?;
        return Ok(());
    }
    if page_belongs_to_known_chain(pager, heads, page_id)? {
        return Err(Error::other(format!(
            "replay insert for `{table}` targets page {page_id}, already reachable from another chain"
        )));
    }
    pager.ensure_page_id(page_id, PageType::TableData, true)?;
    let old_head = *heads
        .get(table)
        .ok_or_else(|| Error::internal(format!("replay: missing head for `{table}`")))?;
    let mut page = pager.read_page(page_id)?;
    page.set_next_page(old_head);
    pager.write_page(page_id, page)?;
    heads.insert(table.to_string(), page_id);
    Ok(())
}

fn page_in_chain(
    pager: &mut Pager,
    heads: &std::collections::HashMap<String, PageId>,
    table: &str,
    page_id: PageId,
) -> Result<bool> {
    let mut cur = *heads
        .get(table)
        .ok_or_else(|| Error::internal(format!("replay: missing head for `{table}`")))?;
    let mut seen = HashSet::new();
    while cur != 0 {
        if cur == page_id {
            return Ok(true);
        }
        if !seen.insert(cur) {
            return Err(Error::other(format!(
                "page chain for `{table}` contains a cycle at page {cur}"
            )));
        }
        cur = pager.read_page(cur)?.next_page();
    }
    Ok(false)
}

fn page_belongs_to_known_chain(
    pager: &mut Pager,
    heads: &HashMap<String, PageId>,
    page_id: PageId,
) -> Result<bool> {
    if page_id == 0 {
        return Ok(true);
    }
    if chain_contains_page(pager, pager.catalog_root(), page_id, "catalog")? {
        return Ok(true);
    }
    for (table, head) in heads {
        if chain_contains_page(pager, *head, page_id, table)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn chain_contains_page(
    pager: &mut Pager,
    mut cur: PageId,
    page_id: PageId,
    label: &str,
) -> Result<bool> {
    let mut seen = HashSet::new();
    while cur != 0 {
        if cur == page_id {
            return Ok(true);
        }
        if !seen.insert(cur) {
            return Err(Error::other(format!(
                "page chain `{label}` contains a cycle at page {cur}"
            )));
        }
        cur = pager.read_page(cur)?.next_page();
    }
    Ok(false)
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
            // Simulate a process crash: keep the WAL instead of letting Drop
            // perform its clean-exit checkpoint.
            std::mem::forget(e);
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
