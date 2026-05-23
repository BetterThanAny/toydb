use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::engine::RowId;
use crate::types::value::Value;

#[derive(Debug, Default, Clone)]
pub(crate) struct IndexStore {
    maps: HashMap<String, BTreeMap<IndexKey, BTreeSet<RowId>>>,
}

impl IndexStore {
    pub fn rebuild<I>(&mut self, name: &str, entries: I)
    where
        I: IntoIterator<Item = (Value, RowId)>,
    {
        let mut map: BTreeMap<IndexKey, BTreeSet<RowId>> = BTreeMap::new();
        for (value, row_id) in entries {
            map.entry(IndexKey(value)).or_default().insert(row_id);
        }
        self.maps.insert(name.to_string(), map);
    }

    pub fn drop(&mut self, name: &str) {
        self.maps.remove(name);
    }

    pub fn insert(&mut self, name: &str, value: &Value, row_id: RowId) {
        self.maps
            .entry(name.to_string())
            .or_default()
            .entry(IndexKey(value.clone()))
            .or_default()
            .insert(row_id);
    }

    pub fn remove(&mut self, name: &str, value: &Value, row_id: RowId) {
        let Some(map) = self.maps.get_mut(name) else {
            return;
        };
        let key = IndexKey(value.clone());
        let Some(row_ids) = map.get_mut(&key) else {
            return;
        };
        row_ids.remove(&row_id);
        if row_ids.is_empty() {
            map.remove(&key);
        }
    }

    pub fn lookup(&self, name: &str, value: &Value) -> Vec<RowId> {
        self.maps
            .get(name)
            .and_then(|map| map.get(&IndexKey(value.clone())))
            .map(|row_ids| row_ids.iter().copied().collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
struct IndexKey(Value);

impl PartialEq for IndexKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for IndexKey {}

impl PartialOrd for IndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}
