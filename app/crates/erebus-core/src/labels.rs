//! One label namespace shared by every session adapter, so two sessions in the same folder are
//! `erebus` and `erebus-2` whichever harness runs them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Labels(Arc<Mutex<HashMap<&'static str, Vec<String>>>>);

impl Labels {
    /// Labels held by every adapter but `me`.
    pub fn others(&self, me: &str) -> Vec<String> {
        let held = self.0.lock().unwrap_or_else(|e| e.into_inner());
        held.iter().filter(|(k, _)| **k != me).flat_map(|(_, v)| v.iter().cloned()).collect()
    }

    /// Replaces the labels `me` holds.
    pub fn set(&self, me: &'static str, labels: Vec<String>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(me, labels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_adapter_sees_the_others_labels_only() {
        let l = Labels::default();
        l.set("claude", vec!["erebus".into()]);
        l.set("codex", vec!["work".into()]);
        assert_eq!(l.others("codex"), vec!["erebus".to_owned()]);
        l.set("claude", vec![]);
        assert!(l.others("codex").is_empty());
    }
}
