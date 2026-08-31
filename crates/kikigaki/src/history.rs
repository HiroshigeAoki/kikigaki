//! In-memory ring buffer of recent successful transcriptions.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::Serialize;

const CAPACITY: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub at: DateTime<Utc>,
    pub raw: String,
    pub text: String,
}

#[derive(Default)]
pub struct History {
    entries: VecDeque<HistoryEntry>,
    next_id: u64,
}

impl History {
    pub fn push(&mut self, raw: String, text: String, at: DateTime<Utc>) -> HistoryEntry {
        let entry = HistoryEntry {
            id: self.next_id,
            at,
            raw,
            text,
        };
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push_front(entry.clone());
        if self.entries.len() > CAPACITY {
            self.entries.pop_back();
        }
        entry
    }

    pub fn list(&self) -> Vec<HistoryEntry> {
        self.entries.iter().cloned().collect()
    }

    pub fn get(&self, id: u64) -> Option<&HistoryEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn push_assigns_increasing_ids_and_keeps_newest_first() {
        let mut history = History::default();
        let first = history.push("raw1".into(), "text1".into(), now());
        let second = history.push("raw2".into(), "text2".into(), now());
        assert_eq!((first.id, second.id), (0, 1));
        assert_eq!(history.list()[0].id, 1);
    }

    #[test]
    fn push_beyond_capacity_drops_the_oldest() {
        let mut history = History::default();
        for i in 0..205 {
            history.push(format!("raw{i}"), format!("text{i}"), now());
        }
        assert_eq!(history.list().len(), 200);
        assert_eq!(history.list().last().unwrap().id, 5);
    }

    #[test]
    fn clear_empties_immediately() {
        let mut history = History::default();
        history.push("r".into(), "t".into(), now());
        history.clear();
        assert!(history.list().is_empty());
    }

    #[test]
    fn get_finds_by_id_or_none() {
        let mut history = History::default();
        let entry = history.push("r".into(), "t".into(), now());
        assert_eq!(history.get(entry.id), Some(&entry));
        assert_eq!(history.get(9999), None);
    }
}
