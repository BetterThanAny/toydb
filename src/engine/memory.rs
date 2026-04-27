//! In-memory storage engine.
//!
//! Holds rows in a `BTreeMap<RowId, Row>` per table so that scans are
//! ordered by insertion (`RowId` is monotonically allocated). Primary
//! key and unique constraints are enforced on insert/update.

use std::collections::{BTreeMap, HashMap};

use crate::catalog::{Catalog, Table};
use crate::engine::{Engine, RowId};
use crate::error::{Error, Result};
use crate::types::row::Row;

#[derive(Debug, Default, Clone)]
pub struct MemoryEngine {
    catalog: Catalog,
    data: HashMap<String, BTreeMap<RowId, Row>>,
    next_id: RowId,
}

impl MemoryEngine {
    pub fn new() -> Self { Self::default() }

    pub fn catalog(&self) -> &Catalog { &self.catalog }

    fn check_unique(
        &self,
        table: &Table,
        new: &Row,
        skip: Option<RowId>,
    ) -> Result<()> {
        let store = self.data.get(&table.name);
        // For each unique column (including PK), check no other row holds
        // the same value. NULL never collides with anything (SQL semantics).
        for (idx, col) in table.columns.iter().enumerate() {
            if !col.unique && !col.primary_key {
                continue;
            }
            let candidate = &new.0[idx];
            if candidate.is_null() {
                continue;
            }
            if let Some(s) = store {
                for (rid, row) in s {
                    if Some(*rid) == skip {
                        continue;
                    }
                    if let Some(true) = candidate.equal_sql(&row.0[idx])? {
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

    fn validate_row(&self, table: &Table, raw: Row) -> Result<Row> {
        if raw.len() != table.columns.len() {
            return Err(Error::ty(format!(
                "table `{}` expects {} values, got {}",
                table.name,
                table.columns.len(),
                raw.len()
            )));
        }
        let mut out = Vec::with_capacity(raw.len());
        for (col, v) in table.columns.iter().zip(raw.into_inner()) {
            out.push(col.validate(v)?);
        }
        Ok(Row(out))
    }
}

impl Engine for MemoryEngine {
    fn create_table(&mut self, table: Table) -> Result<()> {
        self.catalog.create_table(table.clone())?;
        self.data.insert(table.name, BTreeMap::new());
        Ok(())
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<bool> {
        if !self.catalog.contains(name) {
            if if_exists {
                return Ok(false);
            }
            return Err(Error::schema(format!("table `{name}` does not exist")));
        }
        self.catalog.drop_table(name)?;
        self.data.remove(name);
        Ok(true)
    }

    fn get_table(&self, name: &str) -> Result<&Table> { self.catalog.get(name) }

    fn list_tables(&self) -> Vec<String> {
        self.catalog.names().cloned().collect()
    }

    fn insert(&mut self, table: &str, row: Row) -> Result<RowId> {
        let table_def = self.catalog.get(table)?.clone();
        let row = self.validate_row(&table_def, row)?;
        self.check_unique(&table_def, &row, None)?;
        self.next_id += 1;
        let id = self.next_id;
        self.data
            .get_mut(table)
            .ok_or_else(|| Error::internal("missing data storage"))?
            .insert(id, row);
        Ok(id)
    }

    fn scan(&self, table: &str) -> Result<Vec<(RowId, Row)>> {
        let _ = self.catalog.get(table)?;
        Ok(self
            .data
            .get(table)
            .map(|m| m.iter().map(|(id, row)| (*id, row.clone())).collect())
            .unwrap_or_default())
    }

    fn update(&mut self, table: &str, id: RowId, row: Row) -> Result<()> {
        let table_def = self.catalog.get(table)?.clone();
        let row = self.validate_row(&table_def, row)?;
        self.check_unique(&table_def, &row, Some(id))?;
        let store = self
            .data
            .get_mut(table)
            .ok_or_else(|| Error::internal("missing data storage"))?;
        if !store.contains_key(&id) {
            return Err(Error::internal(format!(
                "update target row {id} no longer exists"
            )));
        }
        store.insert(id, row);
        Ok(())
    }

    fn delete(&mut self, table: &str, id: RowId) -> Result<()> {
        let _ = self.catalog.get(table)?;
        let store = self
            .data
            .get_mut(table)
            .ok_or_else(|| Error::internal("missing data storage"))?;
        store.remove(&id);
        Ok(())
    }

    fn get(&self, table: &str, id: RowId) -> Result<Option<Row>> {
        let _ = self.catalog.get(table)?;
        Ok(self.data.get(table).and_then(|m| m.get(&id).cloned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Column;
    use crate::sql::ast::DataType;
    use crate::types::value::Value;

    fn users_table() -> Table {
        Table::new("users", vec![
            Column::new("id", DataType::Integer).primary_key(),
            Column::new("name", DataType::String).not_null(),
            Column::new("email", DataType::String).unique().nullable(true),
        ]).unwrap()
    }

    #[test]
    fn create_and_list() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        assert_eq!(e.list_tables(), vec!["users".to_string()]);
    }

    #[test]
    fn insert_and_scan() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        let id1 = e
            .insert("users", Row(vec![1.into(), "alice".into(), "a@x".into()]))
            .unwrap();
        let id2 = e
            .insert("users", Row(vec![2.into(), "bob".into(), "b@x".into()]))
            .unwrap();
        assert!(id1 < id2);
        let rows = e.scan("users").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1[1], Value::String("alice".into()));
    }

    #[test]
    fn pk_uniqueness_enforced() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        e.insert("users", Row(vec![1.into(), "alice".into(), Value::Null])).unwrap();
        let err = e
            .insert("users", Row(vec![1.into(), "bob".into(), Value::Null]))
            .unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn unique_column_enforced() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        e.insert("users", Row(vec![1.into(), "a".into(), "x@x".into()])).unwrap();
        let err = e
            .insert("users", Row(vec![2.into(), "b".into(), "x@x".into()]))
            .unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn null_unique_does_not_collide() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        e.insert("users", Row(vec![1.into(), "a".into(), Value::Null])).unwrap();
        e.insert("users", Row(vec![2.into(), "b".into(), Value::Null])).unwrap();
        assert_eq!(e.scan("users").unwrap().len(), 2);
    }

    #[test]
    fn not_null_enforced() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        let err = e
            .insert("users", Row(vec![1.into(), Value::Null, Value::Null]))
            .unwrap_err();
        assert!(err.to_string().contains("NOT NULL"));
    }

    #[test]
    fn coercion_int_to_string_column() {
        let t = Table::new("t", vec![Column::new("x", DataType::String)]).unwrap();
        let mut e = MemoryEngine::new();
        e.create_table(t).unwrap();
        e.insert("t", Row(vec![Value::Integer(42)])).unwrap();
        let v = e.scan("t").unwrap();
        assert_eq!(v[0].1[0], Value::String("42".into()));
    }

    #[test]
    fn update_changes_row() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        let id = e
            .insert("users", Row(vec![1.into(), "alice".into(), "a@x".into()]))
            .unwrap();
        e.update("users", id, Row(vec![1.into(), "alice".into(), "z@z".into()])).unwrap();
        let v = e.scan("users").unwrap();
        assert_eq!(v[0].1[2], Value::String("z@z".into()));
    }

    #[test]
    fn update_violating_pk_rejected() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        let id1 = e
            .insert("users", Row(vec![1.into(), "alice".into(), Value::Null]))
            .unwrap();
        let _id2 = e
            .insert("users", Row(vec![2.into(), "bob".into(), Value::Null]))
            .unwrap();
        let err = e
            .update("users", id1, Row(vec![2.into(), "alice".into(), Value::Null]))
            .unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn delete_removes_row() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        let id = e
            .insert("users", Row(vec![1.into(), "alice".into(), Value::Null]))
            .unwrap();
        e.delete("users", id).unwrap();
        assert!(e.scan("users").unwrap().is_empty());
        assert!(e.get("users", id).unwrap().is_none());
    }

    #[test]
    fn drop_table_removes_data() {
        let mut e = MemoryEngine::new();
        e.create_table(users_table()).unwrap();
        e.insert("users", Row(vec![1.into(), "alice".into(), Value::Null])).unwrap();
        assert!(e.drop_table("users", false).unwrap());
        assert!(e.list_tables().is_empty());
        assert!(e.get_table("users").is_err());
    }

    #[test]
    fn drop_missing_with_if_exists_is_ok() {
        let mut e = MemoryEngine::new();
        assert!(!e.drop_table("missing", true).unwrap());
    }

    #[test]
    fn drop_missing_without_if_exists_errors() {
        let mut e = MemoryEngine::new();
        assert!(e.drop_table("missing", false).is_err());
    }
}
