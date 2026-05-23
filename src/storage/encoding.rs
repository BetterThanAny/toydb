//! Hand-rolled binary encoding for the on-disk types.
//!
//! Layouts (little-endian throughout):
//!
//! ```text
//! Value:
//!   tag: u8    payload (depends on tag):
//!     0  Null     ()
//!     1  Boolean  u8 (0/1)
//!     2  Integer  i64
//!     3  Float    f64 (raw bits)
//!     4  String   u32 length + UTF-8 bytes
//!
//! Row:
//!   u32 column_count + Value*
//!
//! Table schema (catalog):
//!   u32 name_len + name_bytes
//!   u32 column_count
//!   per column:
//!     u32 name_len + name_bytes
//!     u8  data_type tag (0=Bool, 1=Int, 2=Float, 3=String)
//!     u8  flags (bit 0 = primary_key, bit 1 = nullable, bit 2 = unique)
//!     u8  has_default (0/1)
//!     [if has_default] Value (encoded literal)
//!   u32 index_count
//!   per index:
//!     u32 name_len + name_bytes
//!     u32 column_len + column_bytes
//! ```

use std::io::{Read, Write};

use crate::catalog::{Column, Index, Table};
use crate::error::{Error, Result};
use crate::sql::ast::{DataType, Expression, Literal};
use crate::types::row::Row;
use crate::types::value::Value;

// ---------------------------------------------------------------------
// Value
// ---------------------------------------------------------------------

pub fn encode_value(v: &Value, w: &mut Vec<u8>) {
    match v {
        Value::Null => w.push(0),
        Value::Boolean(b) => {
            w.push(1);
            w.push(if *b { 1 } else { 0 });
        }
        Value::Integer(n) => {
            w.push(2);
            w.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(f) => {
            w.push(3);
            w.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::String(s) => {
            w.push(4);
            let bytes = s.as_bytes();
            w.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            w.extend_from_slice(bytes);
        }
    }
}

pub fn decode_value(r: &mut &[u8]) -> Result<Value> {
    let tag = read_u8(r)?;
    Ok(match tag {
        0 => Value::Null,
        1 => Value::Boolean(read_u8(r)? != 0),
        2 => Value::Integer(read_i64(r)?),
        3 => Value::Float(f64::from_bits(read_u64(r)?)),
        4 => {
            let len = read_u32(r)? as usize;
            let bytes = read_bytes(r, len)?;
            Value::String(
                String::from_utf8(bytes).map_err(|e| Error::other(format!("decode utf-8: {e}")))?,
            )
        }
        other => return Err(Error::other(format!("unknown Value tag {other}"))),
    })
}

// ---------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------

pub fn encode_row(row: &Row, w: &mut Vec<u8>) {
    w.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for v in &row.0 {
        encode_value(v, w);
    }
}

pub fn decode_row(r: &mut &[u8]) -> Result<Row> {
    let n = read_u32(r)? as usize;
    let mut vs = Vec::with_capacity(n);
    for _ in 0..n {
        vs.push(decode_value(r)?);
    }
    Ok(Row(vs))
}

// ---------------------------------------------------------------------
// Table schema
// ---------------------------------------------------------------------

pub fn encode_table(t: &Table, w: &mut Vec<u8>) {
    encode_string(&t.name, w);
    w.extend_from_slice(&(t.columns.len() as u32).to_le_bytes());
    for c in &t.columns {
        encode_column(c, w);
    }
    w.extend_from_slice(&(t.indexes.len() as u32).to_le_bytes());
    for index in &t.indexes {
        encode_string(&index.name, w);
        encode_string(&index.column, w);
    }
}

pub fn decode_table(r: &mut &[u8]) -> Result<Table> {
    let name = decode_string(r)?;
    let n = read_u32(r)? as usize;
    let mut cols = Vec::with_capacity(n);
    for _ in 0..n {
        cols.push(decode_column(r)?);
    }
    let mut table = Table::new(name, cols)?;
    // Older catalog entries ended after the column list. Treat that as
    // an empty index list so databases from before indexes still open.
    if r.is_empty() {
        return Ok(table);
    }
    let index_count = read_u32(r)? as usize;
    for _ in 0..index_count {
        let index_name = decode_string(r)?;
        let column = decode_string(r)?;
        table.add_index(Index::new(index_name, table.name.clone(), column)?)?;
    }
    Ok(table)
}

fn encode_column(c: &Column, w: &mut Vec<u8>) {
    encode_string(&c.name, w);
    w.push(datatype_tag(c.ty));
    let mut flags = 0u8;
    if c.primary_key {
        flags |= 0b001;
    }
    if c.nullable {
        flags |= 0b010;
    }
    if c.unique {
        flags |= 0b100;
    }
    w.push(flags);
    match &c.default {
        None => w.push(0),
        Some(expr) => {
            // We only persist literal defaults (constant-folded at create time).
            let lit = literal_from_expr(expr).unwrap_or(Literal::Null);
            w.push(1);
            encode_value(&literal_to_value(lit), w);
        }
    }
}

fn decode_column(r: &mut &[u8]) -> Result<Column> {
    let name = decode_string(r)?;
    let ty = datatype_from_tag(read_u8(r)?)?;
    let flags = read_u8(r)?;
    let has_default = read_u8(r)? != 0;
    let default = if has_default {
        let v = decode_value(r)?;
        Some(value_to_literal_expr(v))
    } else {
        None
    };
    Ok(Column {
        name,
        ty,
        primary_key: flags & 0b001 != 0,
        nullable: flags & 0b010 != 0,
        unique: flags & 0b100 != 0,
        default,
    })
}

fn datatype_tag(t: DataType) -> u8 {
    match t {
        DataType::Boolean => 0,
        DataType::Integer => 1,
        DataType::Float => 2,
        DataType::String => 3,
    }
}

fn datatype_from_tag(t: u8) -> Result<DataType> {
    Ok(match t {
        0 => DataType::Boolean,
        1 => DataType::Integer,
        2 => DataType::Float,
        3 => DataType::String,
        n => return Err(Error::other(format!("unknown DataType tag {n}"))),
    })
}

fn literal_from_expr(e: &Expression) -> Option<Literal> {
    match e {
        Expression::Literal(l) => Some(l.clone()),
        _ => None,
    }
}

fn literal_to_value(l: Literal) -> Value {
    match l {
        Literal::Null => Value::Null,
        Literal::Boolean(b) => Value::Boolean(b),
        Literal::Integer(n) => Value::Integer(n),
        Literal::Float(f) => Value::Float(f),
        Literal::String(s) => Value::String(s),
    }
}

fn value_to_literal_expr(v: Value) -> Expression {
    let lit = match v {
        Value::Null => Literal::Null,
        Value::Boolean(b) => Literal::Boolean(b),
        Value::Integer(n) => Literal::Integer(n),
        Value::Float(f) => Literal::Float(f),
        Value::String(s) => Literal::String(s),
    };
    Expression::Literal(lit)
}

// ---------------------------------------------------------------------
// Whole-file convenience: encode a `Vec<Row>` for tests / WAL.
// ---------------------------------------------------------------------

pub fn write_rows<W: Write>(w: &mut W, rows: &[Row]) -> Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for row in rows {
        encode_row(row, &mut buf);
    }
    w.write_all(&buf)?;
    Ok(())
}

pub fn read_rows<R: Read>(r: &mut R) -> Result<Vec<Row>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    let mut slice = &buf[..];
    let n = read_u32(&mut slice)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(decode_row(&mut slice)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------

fn read_u8(r: &mut &[u8]) -> Result<u8> {
    if r.is_empty() {
        return Err(Error::other("decode: unexpected EOF (u8)"));
    }
    let b = r[0];
    *r = &r[1..];
    Ok(b)
}

fn read_u32(r: &mut &[u8]) -> Result<u32> {
    if r.len() < 4 {
        return Err(Error::other("decode: unexpected EOF (u32)"));
    }
    let arr: [u8; 4] = r[..4].try_into().unwrap();
    *r = &r[4..];
    Ok(u32::from_le_bytes(arr))
}

fn read_u64(r: &mut &[u8]) -> Result<u64> {
    if r.len() < 8 {
        return Err(Error::other("decode: unexpected EOF (u64)"));
    }
    let arr: [u8; 8] = r[..8].try_into().unwrap();
    *r = &r[8..];
    Ok(u64::from_le_bytes(arr))
}

fn read_i64(r: &mut &[u8]) -> Result<i64> {
    Ok(read_u64(r)? as i64)
}

fn read_bytes(r: &mut &[u8], n: usize) -> Result<Vec<u8>> {
    if r.len() < n {
        return Err(Error::other(format!(
            "decode: unexpected EOF (wanted {n} bytes, have {})",
            r.len()
        )));
    }
    let bytes = r[..n].to_vec();
    *r = &r[n..];
    Ok(bytes)
}

fn encode_string(s: &str, w: &mut Vec<u8>) {
    let b = s.as_bytes();
    w.extend_from_slice(&(b.len() as u32).to_le_bytes());
    w.extend_from_slice(b);
}

fn decode_string(r: &mut &[u8]) -> Result<String> {
    let n = read_u32(r)? as usize;
    let bytes = read_bytes(r, n)?;
    String::from_utf8(bytes).map_err(|e| Error::other(format!("decode utf-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_value(v: Value) -> Value {
        let mut buf = Vec::new();
        encode_value(&v, &mut buf);
        let mut slice = buf.as_slice();
        decode_value(&mut slice).unwrap()
    }

    #[test]
    fn roundtrip_all_value_kinds() {
        assert_eq!(roundtrip_value(Value::Null), Value::Null);
        assert_eq!(roundtrip_value(Value::Boolean(true)), Value::Boolean(true));
        assert_eq!(
            roundtrip_value(Value::Integer(-1234)),
            Value::Integer(-1234)
        );
        assert_eq!(roundtrip_value(Value::Float(2.5)), Value::Float(2.5));
        assert_eq!(
            roundtrip_value(Value::String("héllo".into())),
            Value::String("héllo".into())
        );
    }

    #[test]
    fn roundtrip_row() {
        let r = Row(vec![
            Value::Integer(1),
            Value::String("a".into()),
            Value::Null,
        ]);
        let mut buf = Vec::new();
        encode_row(&r, &mut buf);
        let mut slice = buf.as_slice();
        let r2 = decode_row(&mut slice).unwrap();
        assert_eq!(r, r2);
    }

    #[test]
    fn roundtrip_table_basic() {
        let mut t = Table::new(
            "users",
            vec![
                Column::new("id", DataType::Integer).primary_key(),
                Column::new("name", DataType::String).not_null(),
                Column::new("active", DataType::Boolean)
                    .default_value(Expression::Literal(Literal::Boolean(true))),
            ],
        )
        .unwrap();
        t.add_index(Index::new("idx_users_name", "users", "name").unwrap())
            .unwrap();
        let mut buf = Vec::new();
        encode_table(&t, &mut buf);
        let mut slice = buf.as_slice();
        let t2 = decode_table(&mut slice).unwrap();
        assert_eq!(t.name, t2.name);
        assert_eq!(t.columns.len(), t2.columns.len());
        assert_eq!(t.columns[0].primary_key, t2.columns[0].primary_key);
        assert_eq!(
            t2.columns[2].default,
            Some(Expression::Literal(Literal::Boolean(true)))
        );
        assert_eq!(t2.indexes[0].name, "idx_users_name");
        assert_eq!(t2.indexes[0].column, "name");
    }

    #[test]
    fn empty_string_decodes() {
        assert_eq!(
            roundtrip_value(Value::String("".into())),
            Value::String("".into())
        );
    }

    #[test]
    fn unknown_tag_errors() {
        let mut slice = &[99u8, 0, 0][..];
        assert!(decode_value(&mut slice).is_err());
    }

    #[test]
    fn truncated_buffer_errors() {
        let mut slice = &[2u8, 1, 2, 3][..]; // Integer tag but only 3 bytes
        assert!(decode_value(&mut slice).is_err());
    }

    #[test]
    fn write_read_rows_via_streams() {
        let rows = vec![
            Row(vec![Value::Integer(1)]),
            Row(vec![Value::Integer(2), Value::String("x".into())]),
        ];
        let mut buf = Vec::new();
        write_rows(&mut buf, &rows).unwrap();
        let mut slice = buf.as_slice();
        let read = read_rows(&mut slice).unwrap();
        assert_eq!(read, rows);
    }
}
