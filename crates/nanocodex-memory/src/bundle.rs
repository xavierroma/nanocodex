//! An owner's memory as one set of files, and the rules the set obeys.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::record::{Kind, LintError, Record};

/// The generated one-pager the agent reads first.
pub const PROFILE: &str = "profile.md";
/// The generated list of every record.
pub const INDEX: &str = "index.md";
/// The store's own bookkeeping; never installed in the VM.
pub const META: &str = "meta.json";
/// The agent's dated notes.
pub const JOURNAL: &str = "journal";
/// What reconciliation changed, by day.
pub const LOG: &str = "log";

/// The profile is read every task, so it stays short.
pub const MAX_PROFILE_CHARS: usize = 6_000;
/// A record is a card, not a document.
pub const MAX_RECORD_CHARS: usize = 4_000;
/// A day's journal or log.
pub const MAX_NOTE_CHARS: usize = 60_000;
/// The whole bundle, so installing it in the VM stays quick.
pub const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
/// The instructions block built from the profile and the index.
pub const MAX_INSTRUCTIONS_CHARS: usize = 9_000;

/// What the store remembers about an owner between runs.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Meta {
    /// The admission position of the last task reconciliation has seen.
    #[serde(default)]
    pub last_task_position: Option<i64>,
    /// The day reconciliation last ran.
    #[serde(default)]
    pub last_dream: Option<NaiveDate>,
}

/// One owner's files, by path relative to the memory root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bundle {
    files: BTreeMap<String, String>,
}

impl Bundle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_files(files: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            files: files.into_iter().collect(),
        }
    }

    pub fn insert(&mut self, path: impl Into<String>, content: impl Into<String>) {
        self.files.insert(path.into(), content.into());
    }

    pub fn remove(&mut self, path: &str) -> Option<String> {
        self.files.remove(path)
    }

    pub fn get(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    pub fn files(&self) -> impl Iterator<Item = (&str, &str)> {
        self.files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str()))
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn size_bytes(&self) -> usize {
        self.files
            .iter()
            .map(|(path, content)| path.len() + content.len())
            .sum()
    }

    pub fn profile(&self) -> Option<&str> {
        self.get(PROFILE)
    }

    pub fn meta(&self) -> Meta {
        self.get(META)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or_default()
    }

    pub fn set_meta(&mut self, meta: &Meta) {
        self.insert(
            META,
            serde_json::to_string_pretty(meta).expect("meta serializes"),
        );
    }

    /// The path of one day's journal.
    pub fn journal_path(date: NaiveDate) -> String {
        format!("{JOURNAL}/{date}.md")
    }

    /// The path of one day's reconciliation log.
    pub fn log_path(date: NaiveDate) -> String {
        format!("{LOG}/{date}.md")
    }

    /// Whether a file belongs in the VM. The store's bookkeeping does not.
    pub fn is_installed(path: &str) -> bool {
        path != META
    }

    /// The one path the agent may change during a task: today's journal.
    /// Everything else it writes is discarded at the next install.
    pub fn is_agent_writable(path: &str, today: NaiveDate) -> bool {
        path == Self::journal_path(today)
    }

    /// Every record file, parsed, or every reason one could not be.
    pub fn records(&self) -> Result<Vec<Record>, Vec<LintError>> {
        let mut records = Vec::new();
        let mut errors = Vec::new();
        for (path, content) in &self.files {
            let Some((directory, _)) = path.split_once('/') else {
                continue;
            };
            if Kind::from_directory(directory).is_none() {
                continue;
            }
            match Record::parse(path, content) {
                Ok(record) => records.push(record),
                Err(error) => errors.push(error),
            }
        }
        if errors.is_empty() {
            Ok(records)
        } else {
            Err(errors)
        }
    }

    /// Every way this bundle breaks the contract. Empty means it holds.
    pub fn lint(&self) -> Vec<LintError> {
        let mut errors = Vec::new();
        let lint = |path: &str, problem: String| LintError {
            path: path.to_owned(),
            problem,
        };
        if self.size_bytes() > MAX_BUNDLE_BYTES {
            errors.push(lint("", format!("bundle exceeds {MAX_BUNDLE_BYTES} bytes")));
        }
        match self.profile() {
            None => errors.push(lint(PROFILE, "the profile is missing".into())),
            Some(profile) if profile.trim().is_empty() => {
                errors.push(lint(PROFILE, "the profile is empty".into()))
            }
            Some(profile) if profile.chars().count() > MAX_PROFILE_CHARS => errors.push(lint(
                PROFILE,
                format!("the profile exceeds {MAX_PROFILE_CHARS} characters"),
            )),
            Some(_) => {}
        }
        for (path, content) in &self.files {
            if path.is_empty()
                || path.starts_with('/')
                || path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                errors.push(lint(path, "invalid path".into()));
                continue;
            }
            match path.split_once('/') {
                None => {
                    if ![PROFILE, INDEX, META].contains(&path.as_str()) {
                        errors.push(lint(path, "unknown top-level file".into()));
                    }
                }
                Some((directory, file)) => {
                    if !file.ends_with(".md") || file.contains('/') {
                        errors.push(lint(
                            path,
                            "a memory file is a markdown file one level deep".into(),
                        ));
                        continue;
                    }
                    if directory == JOURNAL || directory == LOG {
                        let stem = file.trim_end_matches(".md");
                        if NaiveDate::parse_from_str(stem, "%Y-%m-%d").is_err() {
                            errors
                                .push(lint(path, "journal and log files are named by date".into()));
                        }
                        if content.chars().count() > MAX_NOTE_CHARS {
                            errors.push(lint(path, format!("exceeds {MAX_NOTE_CHARS} characters")));
                        }
                    } else if Kind::from_directory(directory).is_some() {
                        if content.chars().count() > MAX_RECORD_CHARS {
                            errors
                                .push(lint(path, format!("exceeds {MAX_RECORD_CHARS} characters")));
                        }
                    } else {
                        errors.push(lint(path, format!("unknown directory {directory}")));
                    }
                }
            }
        }
        match self.records() {
            Ok(records) => {
                let ids: BTreeSet<&str> = records.iter().map(|record| record.id.as_str()).collect();
                for record in &records {
                    for link in record.links() {
                        if !ids.contains(link.as_str()) {
                            errors.push(lint(
                                &record.path(),
                                format!("links to a record that does not exist: [[{link}]]"),
                            ));
                        }
                    }
                }
            }
            Err(record_errors) => errors.extend(record_errors),
        }
        errors
    }

    /// The index, generated from the records: one line per record, grouped
    /// by kind, so the agent knows what memory exists before it greps.
    pub fn render_index(&self) -> Result<String, Vec<LintError>> {
        let records = self.records()?;
        let mut text = String::from(
            "# Index of memory\n\nOne line per record. Read the file for the facts; grep by alias.\n",
        );
        for kind in Kind::ALL {
            let mut of_kind: Vec<&Record> = records
                .iter()
                .filter(|record| record.kind == kind)
                .collect();
            if of_kind.is_empty() {
                continue;
            }
            of_kind.sort_by(|a, b| a.id.cmp(&b.id));
            text.push_str(&format!("\n## {}\n", kind.directory()));
            for record in of_kind {
                text.push_str(&format!(
                    "- {} ({}): {}\n",
                    record.path(),
                    record.aliases.join(", "),
                    record.summary()
                ));
            }
        }
        Ok(text)
    }

    /// Regenerates the index in place.
    pub fn refresh_index(&mut self) -> Result<(), Vec<LintError>> {
        let index = self.render_index()?;
        self.insert(INDEX, index);
        Ok(())
    }

    /// What the agent's instructions carry: the profile, then the index,
    /// within a fixed budget. Journals and records are read through the VM.
    pub fn instructions_block(&self) -> Option<String> {
        let profile = self.profile()?.trim();
        if profile.is_empty() {
            return None;
        }
        let mut block = String::from(
            "What you remember about this person, as of the last reconciliation. Remembered, not verified: an entry may be stale or wrong, dates say how old it is, and what the person says now wins. The full memory is in /home/user/memory.\n\n",
        );
        block.push_str(profile);
        if let Some(index) = self.get(INDEX) {
            let remaining = MAX_INSTRUCTIONS_CHARS.saturating_sub(block.chars().count() + 2);
            if remaining > 200 {
                block.push_str("\n\n");
                let mut taken = 0;
                for line in index.lines() {
                    let cost = line.chars().count() + 1;
                    if taken + cost > remaining {
                        block.push_str("… (more in /home/user/memory/index.md)\n");
                        break;
                    }
                    block.push_str(line);
                    block.push('\n');
                    taken += cost;
                }
            }
        }
        Some(block)
    }
}

/// The paths a new bundle differs from an old one in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub put: BTreeMap<String, String>,
    pub delete: Vec<String>,
}

impl Changes {
    pub fn between(before: &Bundle, after: &Bundle) -> Self {
        let mut changes = Self::default();
        for (path, content) in &after.files {
            if before.files.get(path) != Some(content) {
                changes.put.insert(path.clone(), content.clone());
            }
        }
        for path in before.files.keys() {
            if !after.files.contains_key(path) {
                changes.delete.push(path.clone());
            }
        }
        changes
    }

    pub fn is_empty(&self) -> bool {
        self.put.is_empty() && self.delete.is_empty()
    }

    /// Applies the changes to a bundle.
    pub fn apply(&self, bundle: &mut Bundle) {
        for (path, content) in &self.put {
            bundle.insert(path.clone(), content.clone());
        }
        for path in &self.delete {
            bundle.remove(path);
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub fn sample() -> Bundle {
        let mut bundle = Bundle::new();
        bundle.insert(PROFILE, "# Xavi\n\nLife context: founder, lives in Barcelona with Maria (2026-09-01).\n\nHow to act: do things without asking twice.\n\nHow to talk: short.\n\nOpen tasks: none.\n");
        bundle.insert("preferences/dining.md", "---\nid: dining\ntype: preference\naliases: [food, lunch, restaurants]\nupdated: 2026-09-21\n---\n- **Pasta:** Loves pasta; said so on 2026-09-15.\n- Orders dinner around 19:30 (2026-09-18), see [[home]].\n");
        bundle.insert("places/home.md", "---\nid: home\ntype: place\naliases: [home, flat, barcelona]\nupdated: 2026-09-10\n---\n- Lives in Barcelona, Eixample (2026-09-10).\n");
        bundle.insert(
            "journal/2026-09-21.md",
            "- 10:02 Looked up pasta places near [[home]] (2026-09-21).\n",
        );
        bundle.refresh_index().unwrap();
        bundle
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::sample;
    use super::*;

    #[test]
    fn a_well_formed_bundle_lints_clean_and_indexes_its_records() {
        let bundle = sample();
        assert_eq!(bundle.lint(), Vec::<LintError>::new());
        let index = bundle.get(INDEX).unwrap();
        assert!(index.contains("## preferences\n- preferences/dining.md (food, lunch, restaurants): Pasta: Loves pasta; said so on 2026-09-15."));
        assert!(index.contains("## places\n- places/home.md (home, flat, barcelona)"));
        assert!(!index.contains("journal"));
    }

    #[test]
    fn lint_catches_missing_profile_bad_paths_dangling_links_and_size() {
        let mut bundle = sample();
        bundle.remove(PROFILE);
        bundle.insert("notes/random.md", "x");
        bundle.insert("journal/today.md", "x");
        bundle.insert("people/maria.md", "---\nid: maria\ntype: person\naliases: [maria]\nupdated: 2026-09-21\n---\n- Partner, see [[ghost]] (2026-09-21).\n");
        bundle.insert("preferences/long.md", format!("---\nid: long\ntype: preference\naliases: [long]\nupdated: 2026-09-21\n---\n- {} (2026-09-21).\n", "x".repeat(MAX_RECORD_CHARS)));
        let problems: Vec<String> = bundle
            .lint()
            .into_iter()
            .map(|error| error.to_string())
            .collect();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("profile is missing")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("unknown directory notes"))
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("named by date"))
        );
        assert!(problems.iter().any(|problem| problem.contains("[[ghost]]")));
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("preferences/long.md: exceeds"))
        );
    }

    #[test]
    fn only_todays_journal_is_the_agents_to_write_and_meta_stays_out_of_the_vm() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        assert!(Bundle::is_agent_writable("journal/2026-09-21.md", today));
        assert!(!Bundle::is_agent_writable("journal/2026-09-20.md", today));
        assert!(!Bundle::is_agent_writable("preferences/dining.md", today));
        assert!(!Bundle::is_agent_writable(PROFILE, today));
        assert!(Bundle::is_installed(PROFILE));
        assert!(!Bundle::is_installed(META));
        let mut bundle = Bundle::new();
        assert_eq!(bundle.meta(), Meta::default());
        bundle.set_meta(&Meta {
            last_task_position: Some(42),
            last_dream: today.into(),
        });
        assert_eq!(bundle.meta().last_task_position, Some(42));
    }

    #[test]
    fn the_instructions_block_carries_the_profile_then_as_much_index_as_fits() {
        let bundle = sample();
        let block = bundle.instructions_block().unwrap();
        assert!(block.starts_with("What you remember about this person"));
        assert!(block.contains("Life context: founder"));
        assert!(block.contains("preferences/dining.md"));
        assert!(block.chars().count() <= MAX_INSTRUCTIONS_CHARS);
        assert!(Bundle::new().instructions_block().is_none());
    }

    #[test]
    fn changes_are_the_difference_and_apply_back() {
        let before = sample();
        let mut after = before.clone();
        after.insert("people/maria.md", "---\nid: maria\ntype: person\naliases: [maria]\nupdated: 2026-09-21\n---\n- Partner (2026-09-21).\n");
        after.remove("journal/2026-09-21.md");
        after.insert(PROFILE, "# Xavi\n\nNew profile 2026-09-22.\n");
        let changes = Changes::between(&before, &after);
        assert_eq!(
            changes.put.keys().collect::<Vec<_>>(),
            ["people/maria.md", PROFILE]
        );
        assert_eq!(changes.delete, ["journal/2026-09-21.md"]);
        let mut rebuilt = before.clone();
        changes.apply(&mut rebuilt);
        assert_eq!(rebuilt, after);
        assert!(Changes::between(&before, &before).is_empty());
    }
}
