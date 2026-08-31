use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// One replacement rule, with any of `from` replaced by `to`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Source strings recognized by this rule.
    pub from: Vec<String>,
    /// Text substituted for every matching source string.
    pub to: String,
}

/// Parsed replacement rules and their longest-match application order.
#[derive(Debug, Default)]
pub struct Rules {
    rules: Vec<Rule>,
    ordered: Vec<(usize, usize)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    #[serde(default, rename = "rule")]
    rules: Vec<Rule>,
}

impl Rules {
    /// Parses replacement rules from the `[[rule]]` TOML format.
    pub fn parse(toml: &str) -> anyhow::Result<Self> {
        let parsed: RuleFile = toml::from_str(toml).context("parse replacement rules")?;
        let mut owners = HashMap::<&str, usize>::new();
        for (rule_index, rule) in parsed.rules.iter().enumerate() {
            if rule.from.is_empty() {
                bail!("replacement rule {rule_index} has no source strings");
            }
            for from in &rule.from {
                if from.is_empty() {
                    bail!("replacement source strings must not be empty");
                }
                if let Some(previous) = owners.insert(from, rule_index) {
                    if previous != rule_index {
                        bail!("duplicate replacement source {from:?} across rules");
                    }
                }
            }
        }
        Ok(Self::index(parsed.rules))
    }

    /// Merges two valid dictionaries, with overrides winning for shared source strings.
    pub fn merge(base: &Rules, overrides: &Rules) -> Rules {
        let override_froms: HashSet<&str> = overrides
            .rules
            .iter()
            .flat_map(|rule| rule.from.iter().map(String::as_str))
            .collect();
        let mut rules: Vec<Rule> = base
            .rules
            .iter()
            .filter_map(|rule| {
                let kept: Vec<String> = rule
                    .from
                    .iter()
                    .filter(|from| !override_froms.contains(from.as_str()))
                    .cloned()
                    .collect();
                (!kept.is_empty()).then(|| Rule {
                    from: kept,
                    to: rule.to.clone(),
                })
            })
            .collect();
        rules.extend(overrides.rules.iter().cloned());
        Self::index(rules)
    }

    /// Validates and indexes an already-parsed learned-rule candidate.
    pub fn from_learned_checked(rules: Vec<Rule>) -> anyhow::Result<Self> {
        let mut owners = HashMap::<&str, usize>::new();
        for (rule_index, rule) in rules.iter().enumerate() {
            if rule.from.is_empty() {
                bail!("learned rule {rule_index} has no source strings");
            }
            for from in &rule.from {
                if from.is_empty() {
                    bail!("replacement source strings must not be empty");
                }
                if let Some(previous) = owners.insert(from, rule_index) {
                    if previous != rule_index {
                        bail!("duplicate replacement source {from:?} across rules");
                    }
                }
            }
        }
        Ok(Self::index(rules))
    }

    fn index(rules: Vec<Rule>) -> Self {
        let mut ordered = Vec::new();
        for (rule_index, rule) in rules.iter().enumerate() {
            for from_index in 0..rule.from.len() {
                ordered.push((rule_index, from_index));
            }
        }
        ordered.sort_by_key(|&(rule_index, from_index)| {
            (
                std::cmp::Reverse(rules[rule_index].from[from_index].chars().count()),
                rule_index,
                from_index,
            )
        });
        Self { rules, ordered }
    }

    /// Applies rules left-to-right with the longest source string taking precedence.
    pub fn apply(&self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        let mut remaining = text;
        while !remaining.is_empty() {
            let matched = self.ordered.iter().find_map(|&(rule_index, from_index)| {
                let rule = &self.rules[rule_index];
                let from = &rule.from[from_index];
                remaining
                    .starts_with(from)
                    .then_some((from.len(), rule.to.as_str()))
            });
            if let Some((source_bytes, replacement)) = matched {
                output.push_str(replacement);
                remaining = &remaining[source_bytes..];
            } else {
                let char_bytes = remaining
                    .chars()
                    .next()
                    .expect("remaining text is non-empty")
                    .len_utf8();
                output.push_str(&remaining[..char_bytes]);
                remaining = &remaining[char_bytes..];
            }
        }
        output
    }

    /// Returns whether the dictionary contains no source strings.
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }
}

/// Hot-reloaded replacement dictionary backed by a TOML file.
pub struct ReplaceFile {
    path: PathBuf,
    mtime: Option<SystemTime>,
    len: u64,
    rules: Rules,
    generation: u64,
}

impl ReplaceFile {
    /// Creates a lazily loaded replacement dictionary.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            mtime: None,
            len: 0,
            rules: Rules::default(),
            generation: 0,
        }
    }

    /// Returns the current rules, reloading when file modification time or length changes.
    pub fn rules(&mut self) -> &Rules {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if self.mtime.is_some() || self.len != 0 || !self.rules.is_empty() {
                    self.mtime = None;
                    self.len = 0;
                    self.rules = Rules::default();
                    self.generation += 1;
                }
                return &self.rules;
            }
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "read replacement file metadata");
                return &self.rules;
            }
        };
        let mtime = metadata.modified().ok();
        let len = metadata.len();
        if (mtime, len) == (self.mtime, self.len) {
            return &self.rules;
        }

        self.mtime = mtime;
        self.len = len;
        match fs::read_to_string(&self.path)
            .with_context(|| format!("read replacement file {}", self.path.display()))
            .and_then(|contents| Rules::parse(&contents))
        {
            Ok(rules) => {
                self.rules = rules;
                self.generation += 1;
            }
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "keeping last good replacement rules");
            }
        }
        &self.rules
    }

    /// Returns the version of the last successfully loaded rule set.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn applies_longest_unicode_from_first() {
        let rules = Rules::parse(
            r#"
                [[rule]]
                from = ["クバ"]
                to = "short"

                [[rule]]
                from = ["クバネティス"]
                to = "Kubernetes"
            "#,
        )
        .unwrap();
        assert_eq!(rules.apply("クバネティスとクバ"), "Kubernetesとshort");
    }

    #[test]
    fn applies_non_overlapping_left_to_right() {
        let rules = Rules::parse("[[rule]]\nfrom = [\"aa\"]\nto = \"b\"\n").unwrap();
        assert_eq!(rules.apply("aaaaa"), "bba");
    }

    #[test]
    fn accepts_multiple_from_strings_per_rule() {
        let rules = Rules::parse(
            "[[rule]]\nfrom = [\"クバネティス\", \"クーバネティス\"]\nto = \"Kubernetes\"\n",
        )
        .unwrap();
        assert_eq!(
            rules.apply("クバネティス、クーバネティス"),
            "Kubernetes、Kubernetes"
        );
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Rules::parse("[[rule]]\nfrom = [\"a\"]\nto = \"b\"\nextra = true\n").is_err());
    }

    #[test]
    fn rejects_duplicate_from_across_rules() {
        let error = Rules::parse(
            "[[rule]]\nfrom = [\"same\"]\nto = \"one\"\n[[rule]]\nfrom = [\"same\"]\nto = \"two\"\n",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("same"));
    }

    #[test]
    fn merge_prefers_overrides_on_shared_from_and_keeps_unrelated_base_rules() {
        let base = Rules::parse(
            "[[rule]]\nfrom = [\"ハザー\"]\nto = \"波座\"\n[[rule]]\nfrom = [\"クバ\"]\nto = \"short\"\n",
        )
        .unwrap();
        let overrides = Rules::parse("[[rule]]\nfrom = [\"ハザー\"]\nto = \"発話\"\n").unwrap();
        let merged = Rules::merge(&base, &overrides);
        assert_eq!(merged.apply("ハザーとクバ"), "発話とshort");
    }

    #[test]
    fn merge_with_empty_overrides_behaves_like_base() {
        let base = Rules::parse("[[rule]]\nfrom = [\"a\"]\nto = \"b\"\n").unwrap();
        assert_eq!(Rules::merge(&base, &Rules::default()).apply("a"), "b");
    }

    #[test]
    fn from_learned_checked_rejects_an_empty_from_string() {
        let rules = vec![Rule {
            from: vec!["".into()],
            to: "x".into(),
        }];
        assert!(Rules::from_learned_checked(rules).is_err());
    }

    #[test]
    fn from_learned_checked_rejects_duplicate_sources_across_rules() {
        let rules = vec![
            Rule {
                from: vec!["a".into()],
                to: "1".into(),
            },
            Rule {
                from: vec!["a".into()],
                to: "2".into(),
            },
        ];
        assert!(Rules::from_learned_checked(rules).is_err());
    }

    #[test]
    fn replace_file_reloads_and_keeps_last_good_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("replace.toml");
        std::fs::write(&path, "[[rule]]\nfrom = [\"a\"]\nto = \"first\"\n").unwrap();
        let mut file = ReplaceFile::new(path.clone());
        assert_eq!(file.rules().apply("a"), "first");

        thread::sleep(Duration::from_millis(2));
        std::fs::write(&path, "[[rule]]\nfrom = [\"a\"]\nto = \"second-longer\"\n").unwrap();
        assert_eq!(file.rules().apply("a"), "second-longer");

        thread::sleep(Duration::from_millis(2));
        std::fs::write(&path, "[[rule]]\nfrom = [\"a\"]\nunknown = true\n").unwrap();
        assert_eq!(file.rules().apply("a"), "second-longer");
    }

    #[test]
    fn missing_replace_file_has_empty_rules() {
        let temp = tempfile::tempdir().unwrap();
        let mut file = ReplaceFile::new(temp.path().join("missing.toml"));
        assert!(file.rules().is_empty());
    }
}
