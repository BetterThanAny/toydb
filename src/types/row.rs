//! Row primitive — an ordered tuple of values.
//!
//! Rows are positional, not named. The schema (column order) is held by
//! the catalog; the executor zips the two to render a result set.

use crate::types::value::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row(pub Vec<Value>);

impl Row {
    pub fn new(values: Vec<Value>) -> Self { Self(values) }

    pub fn len(&self) -> usize { self.0.len() }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }

    pub fn get(&self, index: usize) -> Option<&Value> { self.0.get(index) }
    pub fn into_inner(self) -> Vec<Value> { self.0 }
}

impl<I: IntoIterator<Item = Value>> From<I> for Row {
    fn from(iter: I) -> Self { Self(iter.into_iter().collect()) }
}

impl std::ops::Index<usize> for Row {
    type Output = Value;
    fn index(&self, i: usize) -> &Value { &self.0[i] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_indexing() {
        let r = Row::new(vec![Value::Integer(1), Value::String("x".into())]);
        assert_eq!(r[0], Value::Integer(1));
        assert_eq!(r[1], Value::String("x".into()));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn row_from_iter() {
        let r: Row = vec![Value::Integer(1), Value::Null].into();
        assert_eq!(r.len(), 2);
        assert!(r[1].is_null());
    }
}
