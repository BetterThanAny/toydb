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

use crate::engine::RowId;
use crate::error::{Error, Result};
use crate::storage::encoding::{decode_row, decode_table, encode_row, encode_table};
use crate::types::row::Row;

#[derive(Debug, Clone, PartialEq)]
pub enum LogRecord {
    /// `CREATE TABLE` was committed — replay creates the table again.
    CreateTable(crate::catalog::Table),
    /// `DROP TABLE`.
    DropTable(String),
    /// `INSERT` returned this row id.
    Insert { table: String, id: RowId, row: Row },
    /// `UPDATE` of an existing row.
    Update { table: String, id: RowId, row: Row },
    /// `DELETE` by row id.
    Delete { table: String, id: RowId },
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
        encode_record(rec, &mut payload);
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
        while !slice.is_empty() {
            if slice.len() < 4 {
                // Torn write at the end of the file — tolerate.
                break;
            }
            let len = u32::from_le_bytes(slice[..4].try_into().unwrap()) as usize;
            slice = &slice[4..];
            if slice.len() < len {
                // Tail-truncated WAL — ignore the partial record. This
                // matches "torn write" semantics: the last commit either
                // fully made it to disk or didn't.
                break;
            }
            let payload = &slice[..len];
            slice = &slice[len..];
            let mut p = payload;
            out.push(decode_record(&mut p)?);
        }
        Ok(out)
    }

    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file = OpenOptions::new().read(true).append(true).create(true).open(&self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path { &self.path }
}

// ---------------------------------------------------------------------
// Record encoding
// ---------------------------------------------------------------------

fn encode_record(rec: &LogRecord, out: &mut Vec<u8>) {
    match rec {
        LogRecord::CreateTable(t) => {
            out.push(1);
            encode_table(t, out);
        }
        LogRecord::DropTable(name) => {
            out.push(2);
            encode_string(name, out);
        }
        LogRecord::Insert { table, id, row } => {
            out.push(3);
            encode_string(table, out);
            out.extend_from_slice(&id.to_le_bytes());
            encode_row(row, out);
        }
        LogRecord::Update { table, id, row } => {
            out.push(4);
            encode_string(table, out);
            out.extend_from_slice(&id.to_le_bytes());
            encode_row(row, out);
        }
        LogRecord::Delete { table, id } => {
            out.push(5);
            encode_string(table, out);
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
}

fn decode_record(r: &mut &[u8]) -> Result<LogRecord> {
    let tag = take_u8(r)?;
    Ok(match tag {
        1 => LogRecord::CreateTable(decode_table(r)?),
        2 => LogRecord::DropTable(decode_string(r)?),
        3 => LogRecord::Insert {
            table: decode_string(r)?,
            id: take_u64(r)?,
            row: decode_row(r)?,
        },
        4 => LogRecord::Update {
            table: decode_string(r)?,
            id: take_u64(r)?,
            row: decode_row(r)?,
        },
        5 => LogRecord::Delete {
            table: decode_string(r)?,
            id: take_u64(r)?,
        },
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
    if r.len() < n { return Err(Error::other("WAL: truncated string")); }
    let s = String::from_utf8(r[..n].to_vec())
        .map_err(|e| Error::other(format!("WAL utf-8: {e}")))?;
    *r = &r[n..];
    Ok(s)
}

fn take_u8(r: &mut &[u8]) -> Result<u8> {
    if r.is_empty() { return Err(Error::other("WAL: truncated u8")); }
    let b = r[0];
    *r = &r[1..];
    Ok(b)
}

fn take_u32(r: &mut &[u8]) -> Result<u32> {
    if r.len() < 4 { return Err(Error::other("WAL: truncated u32")); }
    let arr: [u8; 4] = r[..4].try_into().unwrap();
    *r = &r[4..];
    Ok(u32::from_le_bytes(arr))
}

fn take_u64(r: &mut &[u8]) -> Result<u64> {
    if r.len() < 8 { return Err(Error::other("WAL: truncated u64")); }
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
            }).unwrap();
            w.append(&LogRecord::Update {
                table: "t".into(),
                id: 7,
                row: Row(vec![Value::Integer(100)]),
            }).unwrap();
            w.append(&LogRecord::Delete { table: "t".into(), id: 7 }).unwrap();
        }
        let mut w = Wal::open(&path).unwrap();
        let recs = w.replay().unwrap();
        assert_eq!(recs.len(), 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::Delete { table: "t".into(), id: 7 }).unwrap();
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
    fn truncate_clears_log() {
        let path = tmppath();
        {
            let mut w = Wal::open(&path).unwrap();
            w.append(&LogRecord::Delete { table: "t".into(), id: 1 }).unwrap();
            w.truncate().unwrap();
            assert_eq!(w.replay().unwrap(), vec![]);
        }
        std::fs::remove_file(&path).ok();
    }
}
