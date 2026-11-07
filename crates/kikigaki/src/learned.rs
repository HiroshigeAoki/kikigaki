//! Hand-edited personal replacement rules.

use std::path::PathBuf;
use std::sync::Arc;

use kikigaki_core::replace::{Rule, Rules};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LearnedRule {
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
        }
    }

    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let rules = match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str::<LearnedFile>(&contents)?.rules,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, rules })
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learned(temp: &tempfile::TempDir) -> Learned {
        Learned::load(temp.path().join("learned.toml")).unwrap()
    }

    #[test]
    fn loads_hand_edited_rules() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("learned.toml"),
            "[[rule]]\nfrom = [\"ハザー\"]\nto = \"発話\"\n",
        )
        .unwrap();
        let merged = learned(&temp).as_core_rules();
        let base =
            kikigaki_core::replace::Rules::parse("[[rule]]\nfrom = [\"ハザー\"]\nto = \"波座\"\n")
                .unwrap();
        assert_eq!(
            kikigaki_core::replace::Rules::merge(&base, &merged).apply("ハザー"),
            "発話"
        );
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
