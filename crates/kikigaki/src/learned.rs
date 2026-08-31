//! Persisted learned replacement rules, kept separate from hand-authored rules.

use std::path::PathBuf;
use std::sync::Arc;

use kikigaki_core::replace::{Rule, Rules};
use serde::{Deserialize, Serialize};

const MAX_RULES: usize = 500;
const MAX_FIELD_CHARS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LearnedRule {
    #[serde(default)]
    pub id: u64,
    pub from: Vec<String>,
    pub to: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LearnedFile {
    #[serde(default, rename = "rule")]
    rules: Vec<LearnedRule>,
}

pub struct Learned {
    path: PathBuf,
    rules: Vec<LearnedRule>,
    next_id: u64,
}

fn to_core_rules(rules: &[LearnedRule]) -> Vec<Rule> {
    rules
        .iter()
        .map(|rule| Rule {
            from: rule.from.clone(),
            to: rule.to.clone(),
        })
        .collect()
}

impl Learned {
    pub(crate) fn empty(path: PathBuf) -> Self {
        Self {
            path,
            rules: Vec::new(),
            next_id: 0,
        }
    }

    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let rules = match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str::<LearnedFile>(&contents)?.rules,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let next_id = rules
            .iter()
            .map(|rule| rule.id)
            .max()
            .map_or(0, |id| id.wrapping_add(1));
        Ok(Self {
            path,
            rules,
            next_id,
        })
    }

    pub fn list(&self) -> &[LearnedRule] {
        &self.rules
    }

    pub fn as_core_rules(&self) -> Arc<Rules> {
        let candidate = to_core_rules(&self.rules);
        match Rules::from_learned_checked(candidate) {
            Ok(rules) => Arc::new(rules),
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "learned.toml has invalid rules; ignoring until fixed");
                Arc::new(Rules::default())
            }
        }
    }

    pub fn remember(&mut self, from: Vec<String>, to: String) -> anyhow::Result<Arc<Rules>> {
        anyhow::ensure!(
            !from.is_empty(),
            "learned rule needs at least one source string"
        );
        for text in from.iter().chain(std::iter::once(&to)) {
            anyhow::ensure!(
                text.chars().count() <= MAX_FIELD_CHARS,
                "learned rule text exceeds {MAX_FIELD_CHARS} characters"
            );
        }
        anyhow::ensure!(
            self.rules.len() < MAX_RULES || self.rules.iter().any(|rule| rule.from == from),
            "learned rule cap ({MAX_RULES}) reached"
        );

        let mut candidate = self.rules.clone();
        candidate.retain(|rule| rule.from != from);
        candidate.push(LearnedRule {
            id: self.next_id,
            from,
            to,
        });
        let checked = Rules::from_learned_checked(to_core_rules(&candidate))?;
        self.persist(&candidate)?;
        self.next_id = self.next_id.wrapping_add(1);
        self.rules = candidate;
        Ok(Arc::new(checked))
    }

    pub fn delete(&mut self, id: u64) -> anyhow::Result<Arc<Rules>> {
        let mut candidate = self.rules.clone();
        candidate.retain(|rule| rule.id != id);
        self.persist(&candidate)?;
        self.rules = candidate;
        Ok(self.as_core_rules())
    }

    fn persist(&self, rules: &[LearnedRule]) -> anyhow::Result<()> {
        let body = toml::to_string(&LearnedFile {
            rules: rules.to_vec(),
        })?;
        crate::settings::atomic_write(&self.path, body.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learned(temp: &tempfile::TempDir) -> Learned {
        Learned::load(temp.path().join("learned.toml")).unwrap()
    }

    #[test]
    fn remember_then_reload_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let mut a = learned(&temp);
        a.remember(vec!["ハザー".into()], "発話".into()).unwrap();
        assert_eq!(learned(&temp).list(), a.list());
    }

    #[test]
    fn learned_rules_win_over_replace_toml_on_the_same_from() {
        let temp = tempfile::tempdir().unwrap();
        let mut learned = learned(&temp);
        let merged = learned
            .remember(vec!["ハザー".into()], "発話".into())
            .unwrap();
        let base =
            kikigaki_core::replace::Rules::parse("[[rule]]\nfrom = [\"ハザー\"]\nto = \"波座\"\n")
                .unwrap();
        assert_eq!(
            kikigaki_core::replace::Rules::merge(&base, &merged).apply("ハザー"),
            "発話"
        );
    }

    #[test]
    fn remember_rejects_fields_over_64_chars() {
        let temp = tempfile::tempdir().unwrap();
        assert!(learned(&temp)
            .remember(vec!["あ".repeat(65)], "x".into())
            .is_err());
    }

    #[test]
    fn delete_removes_by_id_and_persists() {
        let temp = tempfile::tempdir().unwrap();
        let mut a = learned(&temp);
        a.remember(vec!["x".into()], "y".into()).unwrap();
        let id = a.list()[0].id;
        a.delete(id).unwrap();
        assert!(learned(&temp).list().is_empty());
    }

    #[test]
    fn as_core_rules_ignores_a_hand_corrupted_file_instead_of_looping() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("learned.toml"),
            "[[rule]]\nfrom = [\"\"]\nto = \"x\"\n",
        )
        .unwrap();
        let learned = Learned::load(temp.path().join("learned.toml")).unwrap();
        assert!(learned.as_core_rules().is_empty());
    }
}
