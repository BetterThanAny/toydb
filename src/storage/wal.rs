//! Write-ahead log (WAL).
//!
//! Format: append-only sequence of `[length: u32 LE][payload bytes]`
//! records. The payload is itself versioned with a 1-byte type tag and
//! follows the layouts in [`LogRecord`].
//!
//! The WAL is intentionally tiny: there's no LSN, no checkpoint, no
//! group commit. Recovery just reads the file from start to end and
//! replays each record onto the engine. Combined with an idempotent
//! engine API (insert with row id, delete with row id) this gives
//! crash-consistent durability.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::catalog::Index;
use crate::engine::RowId;
use crate::error::{Error, Result};
use crate::storage::encoding::{decode_row, decode_table, encode_row, encode_table};
use crate::storage::page::PageId;
use crate::types::row::Row;

#[derive(Debug, Clone, PartialEq)]
pub enum LogRecord {
    /// `CREATE TABLE` was committed — replay creates the table again.
    CreateTable(crate::catalog::Table),
    /// `DROP TABLE`.
    DropTable(String),
    /// `DROP TABLE` with the exact page chain captured before any page is
    /// deallocated. Newer disk engines use this to make replay idempotent even
    /// if a crash left some table pages already on the free list.
    DropTablePages { table: String, pages: Vec<PageId> },
    /// `CREATE INDEX`.
    CreateIndex(Index),
    /// `DROP INDEX`.
    DropIndex(String),
    /// `INSERT` returned this row id.
    Insert { table: String, id: RowId, row: Row },
    /// Batch INSERT committed as one statement.
    InsertBatch {
        table: String,
        rows: Vec<(RowId, Row)>,
    },
    /// `UPDATE` of an existing row.
    Update { table: String, id: RowId, row: Row },
    /// Batch UPDATE committed as one statement.
    UpdateBatch {
        table: String,
        rows: Vec<(RowId, Row)>,
    },
    /// `DELETE` by row id.
    Delete { table: String, id: RowId },
    /// Batch DELETE committed as one statement.
    DeleteBatch { table: String, ids: Vec<RowId> },
}

pub struct Wal {
    file: File,
    path: PathBuf,
}

impl Wal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        Ok(Self { file, path })
    }

    pub fn append(&mut self, rec: &LogRecord) -> Result<()> {
        let mut payload = Vec::new();
        encode_record(rec, &mut payload)?;
        let mut framed = Vec::with_capacity(payload.len() + 4);
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        self.file.write_all(&framed)?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Read every record in the file in order.
    pub fn replay(&mut self) -> Result<Vec<LogRecord>> {
        let mut buf = Vec::new();
        let mut f = OpenOptions::new().read(true).open(&self.path)?;
        f.seek(SeekFrom::Start(0))?;
        f.read_to_end(&mut buf)?;
        let mut slice = buf.as_slice();
        let mut out = Vec::new();
        let mut offset = 0usize;
        let mut truncate_to = None;
        while !slice.is_empty() {
            if slice.len() < 4 {
                // Torn write at the end of the file — tolerate.
                truncate_to = Some(offset);
                break;
            }
            let frame_start = offset;
            let len = u32::from_le_bytes(slice[..4].try_into().unwrap()) as usize;
            slice = &slice[4..];
            offset += 4;
            if slice.len() < len {
                // Tail-truncated WAL — ignore the partial record. This
                // matches "torn write" semantics: the last commit either
                // fully made it to disk or didn't.
                truncate_to = Some(frame_start);
                break;
            }
            let payload = &slice[..len];
            slice = &slice[len..];
            offset += len;
            let mut p = payload;
            let rec = decode_record(&mut p)?;
            if !p.is_empty() {
                return Err(Error::other("WAL: trailing bytes in record payload"));
            }
            out.push(rec);
        }
        if let Some(n) = truncate_to {
            self.file.set_len(n as u64)?;
            self.file.sync_data()?;
        }
        Ok(out)
    }

    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.sync_data()?;
        self.file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------
// Record encoding
// ---------------------------------------------------------------------

fn encode_record(rec: &LogRecord, out: &mut Vec<u8>) -> Result<()> {
    match rec {
        LogRecord::CreateTable(t) => {
            out.push(1);
            encode_table(t, out)?;
        }
        LogRecord::DropTable(name) => {
            out.push(2);
            encode_string(name, out);
        }
        LogRecord::DropTablePages { table, pages } => {
            out.push(9);
            encode_string(table, out);
            out.extend_from_slice(
                &checked_len_u32(pages.len(), "drop-table page count")?.to_le_bytes(),
            );
            for page in pages {
                out.extend_from_slice(&page.to_le_bytes());
            }
        }
        LogRecord::CreateIndex(index) => {
            out.push(6);
            encode_string(&index.name, out);
            encode_string(&index.table, out);
            encode_string(&index.column, out);
        }
        LogRecord::DropIndex(name) => {
            out.push(7);
            encode_string(name, out);
        }
        LogRecord::Insert { table, id, row } => {
            out.push(3);
            encode_string(table, out);
            out.extend_from_slice(&id.to_le_bytes());
            encode_row(row, out);
        }
        LogRecord::InsertBatch { table, rows } => {
            out.push(8);
            encode_string(table, out);
            out.extend_from_slice(
                &checked_len_u32(rows.len(), "insert batch row count")?.to_le_bytes(),
            );
            for (id, row) in rows {
                out.extend_from_slice(&id.to_le_bytes());
                encode_row(row, out);
            }
        }
        LogRecord::Update { table, id, row } => {
            out.push(4);
            encode_string(table, out);
            out.extend_from_slice(&id.to_le_bytes());
            encode_row(row, out);
        }
        LogRecord::UpdateBatch { table, rows } => {
            out.push(10);
            encode_string(table, out);
            out.extend_from_slice(
                &checked_len_u32(rows.len(), "update batch row count")?.to_le_bytes(),
            );
            for (id, row) in rows {
                out.extend_from_slice(&id.to_le_bytes());
                encode_row(row, out);
            }
        }
        LogRecord::Delete { table, id } => {
            out.push(5);
            encode_string(table, out);
            out.extend_from_slice(&id.to_le_bytes());
        }
        LogRecord::DeleteBatch { table, ids } => {
            out.push(11);
            encode_string(table, out);
            out.extend_from_slice(
                &checked_len_u32(ids.len(), "delete batch row count")?.to_le_bytes(),
            );
            for id in ids {
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
    }
    Ok(())
}

fn decode_record(r: &mut &[u8]) -> Result<LogRecord> {
    let tag = take_u8(r)?;
    Ok(match tag {
        1 => LogRecord::CreateTable(decode_table(r)?),
        2 => LogRecord::DropTable(decode_string(r)?),
        9 => {
            let table = decode_string(r)?;
            let count = take_u32(r)? as usize;
            let mut pages = Vec::with_capacity(count);
            for _ in 0..count {
                pages.push(take_u64(r)?);
            }
            LogRecord::DropTablePages { table, pages }
        }
        6 => LogRecord::CreateIndex(Index::new(
            decode_string(r)?,
            decode_string(r)?,
            decode_string(r)?,
        )?),
        7 => LogRecord::DropIndex(decode_string(r)?),
        3 => LogRecord::Insert {
            table: decode_string(r)?,
            id: take_u64(r)?,
            row: decode_row(r)?,
        },
        8 => {
            let table = decode_string(r)?;
            let count = take_u32(r)? as usize;
            let mut rows = Vec::with_capacity(count);
            for _ in 0..count {
                rows.push((take_u64(r)?, decode_row(r)?));
            }
            LogRecord::InsertBatch { table, rows }
        }
        4 => LogRecord::Update {
            table: decode_string(r)?,
            id: take_u64(r)?,
            row: decode_row(r)?,
        },
        10 => {
            let table = decode_string(r)?;
            let count = take_u32(r)? as usize;
            let mut rows = Vec::with_capacity(count);
            for _ in 0..count {
                rows.push((take_u64(r)?, decode_row(r)?));
            }
            LogRecord::UpdateBatch { table, rows }
        }
        5 => LogRecord::Delete {
            table: decode_string(r)?,
            id: take_u64(r)?,
        },
        11 => {
            let table = decode_string(r)?;
            let count = take_u32(r)? as usize;
            let mut ids = Vec::with_capacity(count);
            for _ in 0..count {
                ids.push(take_u64(r)?);
            }
            LogRecord::DeleteBatch { table, ids }
        }
        other => return Err(Error::other(format!("WAL: unknown record tag {other}"))),
    })
}

fn encode_string(s: &str, w: &mut Vec<u8>) {
    let b = s.as_bytes();
    w.extend_from_slice(&(b.len() as u32).to_le_bytes());
    w.extend_from_slice(b);
}

fn decode_string(r: &mut &[u8]) -> Result<String> {
    let n = take_u32(r)? as usize;
    if r.len() < n {
        return Err(Error::other("WAL: truncated string"));
    }
    let s =
        String::from_utf8(r[..n].to_vec()).map_err(|e| Error::other(format!("WAL utf-8: {e}")))?;
    *r = &r[n..];
    Ok(s)
}

fn checked_len_u32(len: usize, label: &str) -> Result<u32> {
    u32::try_from(len).map_err(|_| Error::other(format!("{label} {len} exceeds u32::MAX")))
}

fn take_u8(r: &mut &[u8]) -> Result<u8> {
    if r.is_empty() {
        return Err(Error::other("WAL: truncated u8"));
    }
    let b = r[0];
    *r = &r[1..];
    Ok(b)
}

fn take_u32(r: &mut &[u8]) -> Result<u32> {
    if r.len() < 4 {
        return Err(Error::other("WAL: truncated u32"));
    }
    let arr: [u8; 4] = r[..4].try_into().unwrap();
    *r = &r[4..];
    Ok(u32::from_le_bytes(arr))
}

fn take_u64(r: &mut &[u8]) -> Result<u64> {
    if r.len() < 8 {
        return Err(Error::other("WAL: truncated u64"));
    }
    let arr: [u8; 8] = r[..8].try_into().unwrap();
    *r = &r[8..];
    Ok(u64::from_le_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Column, Table};
    use crate::sql::ast::DataType;
    use crate::types::value::Value;

    fn tmppath() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("toydb-wal-{n}-{c}-{}.log", std::process::id()))
    }

    fn make_table() -> Table {
        Table::new("t", vec![Column::new("a", DataType::Integer).primary_key()]).unwrap()
    }

    #[test]
    fn roundtrip_create_drop() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::CreateTable(make_table())).unwrap();
            w.append(&LogRecord::DropTable("t".into())).unwrap();
        }
        let mut w = Wal::open(&path).unwrap();
        let recs = w.replay().unwrap();
        assert_eq!(recs.len(), 2);
        match &recs[0] {
            LogRecord::CreateTable(t) => assert_eq!(t.name, "t"),
            _ => panic!(),
        }
        match &recs[1] {
            LogRecord::DropTable(name) => assert_eq!(name, "t"),
            _ => panic!(),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn roundtrip_drop_table_pages() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::DropTablePages {
                table: "t".into(),
                pages: vec![2, 3, 5],
            })
            .unwrap();
        }
        let mut w = Wal::open(&path).unwrap();
        let recs = w.replay().unwrap();
        assert_eq!(
            recs,
            vec![LogRecord::DropTablePages {
                table: "t".into(),
                pages: vec![2, 3, 5],
            }]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn roundtrip_dml() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::Insert {
                table: "t".into(),
                id: 7,
                row: Row(vec![Value::Integer(42)]),
            })
            .unwrap();
            w.append(&LogRecord::InsertBatch {
                table: "t".into(),
                rows: vec![
                    (8, Row(vec![Value::Integer(43)])),
                    (9, Row(vec![Value::Integer(44)])),
                ],
            })
            .unwrap();
            w.append(&LogRecord::Update {
                table: "t".into(),
                id: 7,
                row: Row(vec![Value::Integer(100)]),
            })
            .unwrap();
            w.append(&LogRecord::UpdateBatch {
                table: "t".into(),
                rows: vec![
                    (8, Row(vec![Value::Integer(101)])),
                    (9, Row(vec![Value::Integer(102)])),
                ],
            })
            .unwrap();
            w.append(&LogRecord::Delete {
                table: "t".into(),
                id: 7,
            })
            .unwrap();
            w.append(&LogRecord::DeleteBatch {
                table: "t".into(),
                ids: vec![8, 9],
            })
            .unwrap();
        }
        let mut w = Wal::open(&path).unwrap();
        let recs = w.replay().unwrap();
        assert_eq!(recs.len(), 6);
        match &recs[1] {
            LogRecord::InsertBatch { table, rows } => {
                assert_eq!(table, "t");
                assert_eq!(rows.len(), 2);
            }
            _ => panic!(),
        }
        match &recs[3] {
            LogRecord::UpdateBatch { table, rows } => {
                assert_eq!(table, "t");
                assert_eq!(rows.len(), 2);
            }
            _ => panic!(),
        }
        match &recs[5] {
            LogRecord::DeleteBatch { table, ids } => {
                assert_eq!(table, "t");
                assert_eq!(ids, &[8, 9]);
            }
            _ => panic!(),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn roundtrip_index_ddl() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::CreateIndex(
                Index::new("idx_t_a", "t", "a").unwrap(),
            ))
            .unwrap();
            w.append(&LogRecord::DropIndex("idx_t_a".into())).unwrap();
        }
        let mut w = Wal::open(&path).unwrap();
        let recs = w.replay().unwrap();
        assert_eq!(recs.len(), 2);
        match &recs[0] {
            LogRecord::CreateIndex(index) => assert_eq!(index.column, "a"),
            _ => panic!(),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::Delete {
                table: "t".into(),
                id: 7,
            })
            .unwrap();
        }
        // Append a partial header (only 2 bytes — too short).
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[5u8, 0])
            .unwrap();
        let mut w = Wal::open(&path).unwrap();
        let recs = w.replay().unwrap();
        // First record is read, partial trailer is silently skipped.
        assert_eq!(recs.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn truncated_record_after_valid_prefix_is_ignored_and_truncated() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::Delete {
                table: "t".into(),
                id: 7,
            })
            .unwrap();
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&100u32.to_le_bytes())
            .unwrap();
        let mut w = Wal::open(&path).unwrap();
        let recs = w.replay().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 18);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn truncate_clears_log() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::Delete {
                table: "t".into(),
                id: 1,
            })
            .unwrap();
            w.truncate().unwrap();
            assert_eq!(w.replay().unwrap(), vec![]);
        }
        std::fs::remove_file(&path).ok();
    }
}
