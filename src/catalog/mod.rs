//! Catalog: schema metadata for tables and columns.
//!
//! The catalog is the single source of truth for "what tables exist and
//! what columns they have". The storage engines defer to the catalog for
//! validation and the executor uses it to resolve column names → indices.

mod table;

pub use table::{Column, Table};

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// In-memory mapping from table name → [`Table`]. Disk-backed engines
/// load this from page 0 of their data file at startup.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
}

impl Catalog {
    pub fn new() -> Self { Self::default() }

    pub fn create_table(&mut self, table: Table) -> Result<()> {
        if self.tables.contains_key(&table.name) {
            return Err(Error::schema(format!("table `{}` already exists", table.name)));
        }
        self.tables.insert(table.name.clone(), table);
        Ok(())
    }

    pub fn drop_table(&mut self, name: &str) -> Result<Table> {
        self.tables
            .remove(name)
            .ok_or_else(|| Error::schema(format!("table `{name}` does not exist")))
    }

    pub fn get(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(name)
            .ok_or_else(|| Error::schema(format!("table `{name}` does not exist")))
    }

    pub fn get_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(name)
            .ok_or_else(|| Error::schema(format!("table `{name}` does not exist")))
    }

    pub fn contains(&self, name: &str) -> bool { self.tables.contains_key(name) }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Table)> { self.tables.iter() }

    pub fn names(&self) -> impl Iterator<Item = &String> { self.tables.keys() }

    pub fn len(&self) -> usize { self.tables.len() }
    pub fn is_empty(&self) -> bool { self.tables.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::DataType;

    fn t(name: &str) -> Table {
        Table::new(
            name,
            vec![Column::new("id", DataType::Integer).primary_key()],
        ).unwrap()
    }

    #[test]
    fn create_and_get() {
        let mut c = Catalog::new();
        c.create_table(t("users")).unwrap();
        assert!(c.contains("users"));
        assert_eq!(c.get("users").unwrap().name, "users");
    }

    #[test]
    fn duplicate_create_errors() {
        let mut c = Catalog::new();
        c.create_table(t("users")).unwrap();
        let e = c.create_table(t("users")).unwrap_err();
        assert!(e.to_string().contains("already exists"));
    }

    #[test]
    fn drop_and_get_missing() {
        let mut c = Catalog::new();
        c.create_table(t("users")).unwrap();
        c.drop_table("users").unwrap();
        assert!(!c.contains("users"));
        assert!(c.drop_table("users").is_err());
        assert!(c.get("users").is_err());
    }
}
