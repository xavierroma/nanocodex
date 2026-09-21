//! One record: a thing the agent knows about the person, as a markdown file.
//!
//! The shape is Instinct's: front matter with an id, a type and aliases,
//! then dated facts as bullets, with `[[id]]` links to other records. The
//! parser is strict about the front matter and lenient about the body, and
//! the renderer writes the one canonical form, so a round trip normalizes a
//! record without changing what it says.

use std::fmt;

use chrono::NaiveDate;

/// The kinds of record, one directory each.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Kind {
    /// What the person likes and wants, and how the agent should behave.
    Preference,
    /// People in the person's life and how they relate to them.
    Person,
    /// Home, work, places they go, addresses.
    Place,
    /// Services and websites the person uses. Never a credential.
    Account,
    /// Things asked for that are not finished, or that recur.
    Task,
}

impl Kind {
    /// Every kind, in the order the index lists them.
    pub const ALL: [Self; 5] = [
        Self::Preference,
        Self::Person,
        Self::Place,
        Self::Account,
        Self::Task,
    ];

    /// The directory records of this kind live in.
    pub const fn directory(self) -> &'static str {
        match self {
            Self::Preference => "preferences",
            Self::Person => "people",
            Self::Place => "places",
            Self::Account => "accounts",
            Self::Task => "tasks",
        }
    }

    /// The value of the `type` front-matter key.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Person => "person",
            Self::Place => "place",
            Self::Account => "account",
            Self::Task => "task",
        }
    }

    /// The kind whose directory this is.
    pub fn from_directory(directory: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.directory() == directory)
    }

    /// The kind with this `type` value.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// One record file, parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    /// The file name without `.md`, and what `[[links]]` name.
    pub id: String,
    pub kind: Kind,
    /// Every name the person uses for this thing, so a grep finds it.
    pub aliases: Vec<String>,
    /// The day the record last changed.
    pub updated: NaiveDate,
    /// Dated facts, one per bullet, in the file's order.
    pub facts: Vec<String>,
}

/// Why a record does not meet the contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LintError {
    pub path: String,
    pub problem: String,
}

impl fmt::Display for LintError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.problem)
    }
}

impl std::error::Error for LintError {}

impl Record {
    /// The record's path inside the bundle.
    pub fn path(&self) -> String {
        format!("{}/{}.md", self.kind.directory(), self.id)
    }

    /// Every `[[id]]` this record names, in order, without repeats.
    pub fn links(&self) -> Vec<String> {
        let mut links = Vec::new();
        for fact in &self.facts {
            for link in links_in(fact) {
                if !links.contains(&link) {
                    links.push(link);
                }
            }
        }
        links
    }

    /// The one-line summary the index carries: the first fact, shortened.
    pub fn summary(&self) -> String {
        let first = self.facts.first().map(String::as_str).unwrap_or("");
        let first = first.replace("**", "");
        let mut summary: String = first.chars().take(120).collect();
        if summary.len() < first.len() {
            summary.push('…');
        }
        summary
    }

    /// Parses the file at `path`. The path decides the expected kind and id,
    /// and the front matter has to agree with it.
    pub fn parse(path: &str, text: &str) -> Result<Self, LintError> {
        let lint = |problem: String| LintError {
            path: path.to_owned(),
            problem,
        };
        let (directory, file) = path
            .split_once('/')
            .ok_or_else(|| lint("a record lives in its type's directory".into()))?;
        let kind = Kind::from_directory(directory)
            .ok_or_else(|| lint(format!("unknown record directory {directory}")))?;
        let id = file
            .strip_suffix(".md")
            .ok_or_else(|| lint("a record file ends in .md".into()))?;
        if !is_valid_id(id) {
            return Err(lint(
                "an id is lowercase letters, digits and hyphens, starting with a letter".into(),
            ));
        }
        let (front, body) = split_front_matter(text)
            .ok_or_else(|| lint("a record starts with a --- front matter block".into()))?;
        let mut declared_id = None;
        let mut declared_type = None;
        let mut aliases = None;
        let mut updated = None;
        for line in front.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| lint(format!("front matter line without a key: {line}")))?;
            let value = value.trim();
            match key.trim() {
                "id" => declared_id = Some(value.to_owned()),
                "type" => declared_type = Some(value.to_owned()),
                "aliases" => aliases = Some(parse_list(value)),
                "updated" => {
                    updated =
                        Some(NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
                            lint(format!("updated is not a YYYY-MM-DD date: {value}"))
                        })?)
                }
                other => return Err(lint(format!("unknown front matter key {other}"))),
            }
        }
        let declared_id = declared_id.ok_or_else(|| lint("front matter has no id".into()))?;
        if declared_id != id {
            return Err(lint(format!(
                "front matter id {declared_id} does not match the file name {id}"
            )));
        }
        let declared_type = declared_type.ok_or_else(|| lint("front matter has no type".into()))?;
        if Kind::from_name(&declared_type) != Some(kind) {
            return Err(lint(format!(
                "front matter type {declared_type} does not match the directory {directory}"
            )));
        }
        let aliases = aliases.ok_or_else(|| lint("front matter has no aliases".into()))?;
        if aliases.is_empty() {
            return Err(lint("a record has at least one alias".into()));
        }
        let updated = updated.ok_or_else(|| lint("front matter has no updated date".into()))?;
        let mut facts = Vec::new();
        for line in body.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let Some(fact) = line.strip_prefix("- ") else {
                return Err(lint(format!("a record body is dated bullets only: {line}")));
            };
            let fact = fact.trim();
            if fact.is_empty() {
                return Err(lint("an empty bullet".into()));
            }
            if !has_date(fact) {
                return Err(lint(format!(
                    "a fact carries the date it was learned: {fact}"
                )));
            }
            facts.push(fact.to_owned());
        }
        if facts.is_empty() {
            return Err(lint("a record has at least one fact".into()));
        }
        Ok(Self {
            id: id.to_owned(),
            kind,
            aliases,
            updated,
            facts,
        })
    }

    /// The canonical file text.
    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str("---\n");
        text.push_str(&format!("id: {}\n", self.id));
        text.push_str(&format!("type: {}\n", self.kind.name()));
        text.push_str(&format!("aliases: [{}]\n", self.aliases.join(", ")));
        text.push_str(&format!("updated: {}\n", self.updated));
        text.push_str("---\n");
        for fact in &self.facts {
            text.push_str("- ");
            text.push_str(fact);
            text.push('\n');
        }
        text
    }
}

/// Whether `id` is a valid record id: lowercase letters, digits and hyphens,
/// starting with a letter, at most 64 characters.
pub fn is_valid_id(id: &str) -> bool {
    let mut characters = id.chars();
    matches!(characters.next(), Some('a'..='z'))
        && id.len() <= 64
        && characters.all(|character| matches!(character, 'a'..='z' | '0'..='9' | '-'))
        && !id.ends_with('-')
        && !id.contains("--")
}

/// The `[[id]]` links in one line of text.
pub fn links_in(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else { break };
        let link = after[..end].trim();
        if is_valid_id(link) {
            links.push(link.to_owned());
        }
        rest = &after[end + 2..];
    }
    links
}

/// Whether the text contains a `YYYY-MM-DD` date anywhere.
pub fn has_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.windows(10).any(|window| {
        window[4] == b'-'
            && window[7] == b'-'
            && [0, 1, 2, 3, 5, 6, 8, 9]
                .iter()
                .all(|&index| window[index].is_ascii_digit())
    })
}

fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = &rest[end + 4..];
    let body = body
        .strip_prefix('\n')
        .or_else(|| body.strip_prefix("\r\n"))
        .unwrap_or(body);
    Some((front, body))
}

fn parse_list(value: &str) -> Vec<String> {
    let inner = value
        .trim()
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(|alias| alias.trim().trim_matches('"').trim_matches('\'').to_owned())
        .filter(|alias| !alias.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DINING: &str = "---\nid: dining\ntype: preference\naliases: [food, lunch, restaurants]\nupdated: 2026-09-21\n---\n- **Pasta:** Loves pasta; said so on 2026-09-15.\n- Orders dinner around 19:30 on weekdays (2026-09-18), see [[home]].\n";

    #[test]
    fn a_record_round_trips_through_its_canonical_form() {
        let record = Record::parse("preferences/dining.md", DINING).unwrap();
        assert_eq!(record.id, "dining");
        assert_eq!(record.kind, Kind::Preference);
        assert_eq!(record.aliases, ["food", "lunch", "restaurants"]);
        assert_eq!(record.facts.len(), 2);
        assert_eq!(record.links(), ["home"]);
        assert_eq!(
            record.summary(),
            "Pasta: Loves pasta; said so on 2026-09-15."
        );
        assert_eq!(record.render(), DINING);
        assert_eq!(
            Record::parse("preferences/dining.md", &record.render()).unwrap(),
            record
        );
    }

    #[test]
    fn the_path_decides_the_kind_and_the_front_matter_has_to_agree() {
        let wrong_directory = Record::parse("people/dining.md", DINING).unwrap_err();
        assert!(
            wrong_directory
                .problem
                .contains("does not match the directory")
        );
        let wrong_name = Record::parse("preferences/food.md", DINING).unwrap_err();
        assert!(wrong_name.problem.contains("does not match the file name"));
        assert!(Record::parse("notes/dining.md", DINING).is_err());
        assert!(Record::parse("preferences/Dining.md", DINING).is_err());
    }

    #[test]
    fn facts_are_dated_bullets_and_nothing_else() {
        let undated = DINING.replace(
            "- Orders dinner around 19:30 on weekdays (2026-09-18), see [[home]].\n",
            "- Orders dinner late.\n",
        );
        assert!(
            Record::parse("preferences/dining.md", &undated)
                .unwrap_err()
                .problem
                .contains("date")
        );
        let prose = format!("{DINING}Some prose about dinner 2026-09-01.\n");
        assert!(Record::parse("preferences/dining.md", &prose).is_err());
        let no_alias = DINING.replace("aliases: [food, lunch, restaurants]", "aliases: []");
        assert!(Record::parse("preferences/dining.md", &no_alias).is_err());
    }

    #[test]
    fn ids_links_and_dates_are_recognized_strictly() {
        assert!(is_valid_id("partner-maria"));
        assert!(!is_valid_id("Maria"));
        assert!(!is_valid_id("-maria"));
        assert!(!is_valid_id("maria--x"));
        assert!(!is_valid_id("1maria"));
        assert_eq!(
            links_in("see [[home]] and [[ partner-maria ]] and [[Bad]] and [[unclosed"),
            ["home", "partner-maria"]
        );
        assert!(has_date("said on 2026-09-15."));
        assert!(!has_date("said on 2026/09/15."));
        assert!(!has_date("2026-9-15"));
    }
}
