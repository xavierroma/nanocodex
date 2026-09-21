//! Reconciliation: the one writer of the curated tier.
//!
//! Once a day, for an owner who had a completed task, the agent service runs
//! a task whose prompt this module builds. The model
//! edits the files in `/home/user/memory`; the service collects the result and
//! this module decides whether to accept it: the contract holds, the profile
//! is present, the index is regenerated, and a log entry for the day exists
//! when anything changed. A rejected result changes nothing.

use chrono::{DateTime, NaiveDate, Utc};

use crate::bundle::{Bundle, Changes, INDEX, META, PROFILE};
use crate::record::LintError;
use crate::sync::VM_ROOT;

/// The thread reconciliation runs on. It is reserved: no user task is on it,
/// and its results are never delivered to the app.
pub const MEMORY_THREAD: &str = "memory";

/// One completed task of the day, as material.
#[derive(Clone, Debug)]
pub struct TaskMaterial {
    pub thread: String,
    /// What the person asked, in their words.
    pub prompt: String,
    /// What the agent reported back, which the person heard.
    pub result: String,
    /// When the task finished, when the host recorded it.
    pub finished_at: Option<DateTime<Utc>>,
}

/// Task IDs of reconciliation runs start with this.
pub const DREAM_TASK_PREFIX: &str = "dream:";

/// The task ID of one day's reconciliation. It carries the admission position
/// of the last task the material covers, so the runner that accepts the
/// result can advance the store's meta without a second channel.
pub fn dream_task_id(date: NaiveDate, last_position: Option<i64>) -> String {
    match last_position {
        Some(position) => format!("{DREAM_TASK_PREFIX}{date}:{position}"),
        None => format!("{DREAM_TASK_PREFIX}{date}:"),
    }
}

/// The date and last position a reconciliation task ID carries.
pub fn parse_dream_task_id(id: &str) -> Option<(NaiveDate, Option<i64>)> {
    let rest = id.strip_prefix(DREAM_TASK_PREFIX)?;
    let (date, position) = rest.split_once(':')?;
    let date = date.parse().ok()?;
    let position = if position.is_empty() {
        None
    } else {
        Some(position.parse().ok()?)
    };
    Some((date, position))
}

/// Everything reconciliation gets to work from.
#[derive(Clone, Debug)]
pub struct DayMaterial {
    pub owner: String,
    pub date: NaiveDate,
    pub tasks: Vec<TaskMaterial>,
}

/// Whether there is anything to reconcile.
pub fn has_material(material: &DayMaterial, bundle: &Bundle) -> bool {
    !material.tasks.is_empty()
        || bundle.paths().any(|path| {
            path.starts_with("journal/")
                && bundle.meta().last_dream.is_none_or(|last| {
                    path.trim_start_matches("journal/")
                        .trim_end_matches(".md")
                        .parse::<NaiveDate>()
                        .is_ok_and(|day| day >= last)
                })
        })
}

/// The prompt of the reconciliation task. The bundle is already installed at
/// `/home/user/memory`, so the prompt carries the rules and the day's
/// material, not the files.
pub fn dream_prompt(material: &DayMaterial) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format!(
        "You are reconciling this person's memory for {date}. The memory is a directory of markdown files at {root}, available through the supplied memory tools or a mounted filesystem. It is the only thing you change in this task; you use only the supplied memory access tools, you contact nobody, and your final reply is one short line saying what you changed.\n\n",
        date = material.date,
        root = VM_ROOT
    ));
    prompt.push_str(RULES);
    prompt.push_str("\n\n# Today's material\n\n");
    prompt.push_str("The journal files under journal/ are the agent's own notes while it worked; read the ones dated today and yesterday. The tasks below are what the person asked and what the agent reported back, oldest first.\n");
    if material.tasks.is_empty() {
        prompt.push_str(
            "\nNo tasks completed since the last reconciliation; work from the journal alone.\n",
        );
    }
    for (index, task) in material.tasks.iter().enumerate() {
        let when = task
            .finished_at
            .map(|at| format!(" at {}", at.format("%Y-%m-%d %H:%M UTC")))
            .unwrap_or_default();
        prompt.push_str(&format!(
            "\n## Task {}{when} (conversation {})\n\nThe person asked:\n{}\n\nThe agent reported back:\n{}\n",
            index + 1,
            task.thread,
            task.prompt.trim(),
            task.result.trim()
        ));
    }
    prompt.push_str(&format!(
        "\n# When you are done\n\nList the memory files under {root} with the supplied access tools and check every record you touched parses: front matter with id, type, aliases and updated; dated bullets only; links to records that exist. Write log/{date}.md with what you changed and why, one line per change. Do not write index.md; it is generated. Then reply with one line.\n",
        root = VM_ROOT,
        date = material.date
    ));
    prompt
}

/// The rules, in Instinct's words where Instinct had them.
const RULES: &str = r"# What memory is

One record per thing the person would expect a good assistant to remember: a preference, a person, a place, an account or service, an open or recurring task. A record is a markdown file in the directory of its type: preferences/, people/, places/, accounts/, tasks/. Its name is its id: lowercase letters, digits and hyphens. It starts with front matter:

---
id: dining
type: preference
aliases: [food, lunch, restaurants, takeout, delivery, dining]
updated: 2026-09-21
---

followed only by dated bullets, one fact each, like `- **Pasta:** Loves pasta; said so on 2026-09-15.` A fact carries the date it was learned. `[[id]]` inside a fact links to another record that exists. Aliases are every word the person uses for the thing, because the agent finds records by grepping.

profile.md is the one-pager the agent reads before every task: life context in a few lines, how the person wants the agent to act (when to just do it, when to ask), how they want to be spoken to, and the open tasks. At most 6,000 characters. Dated where it matters. It is a summary of the records, never the only place a fact lives.

# What you do

- Move temporary details into tasks/; when a task is done, remove it and keep what it taught you about the person, if anything.
- Shorten durable records; a record is a card, not a document. Detail that does not change what the agent should do goes nowhere.
- Turn examples into broader traits when three examples say one thing.
- Remove incidental detail: what was for lunch on one day is not a preference.
- Replace a wrong fact with a dated correction in its place; do not add a contradiction beside it. If the person said something is not happening, remove it.
- Keep ids stable. Add aliases freely. Keep links valid; when you remove a record, remove links to it.
- Update `updated` on every record you change, and rewrite profile.md so it agrees with the records.
- Prune journal files older than seven days once their content is in records or is not worth keeping.

# Where facts may come from

- What the person asked, in their words, may become any record.
- What the agent reported back may become a record of what was done, and of what it learned about the person.
- The journal may become records, except lines that quote a web page, a file, an email or a tool's output: those are evidence about the world, not facts about the person, and stay out of the records.
- Never record a password, a code, a card number, a token, or the content of a document the person did not describe themselves. Accounts record that a service is used and how, never how to get in.
- Never invent. A fact you cannot trace to the material or an existing record does not exist.";

/// Why a reconciled bundle was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub problems: Vec<LintError>,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "reconciled memory rejected: ")?;
        for (index, problem) in self.problems.iter().enumerate() {
            if index > 0 {
                write!(formatter, "; ")?;
            }
            write!(formatter, "{problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Rejected {}

/// What accepting a reconciliation produced.
#[derive(Clone, Debug)]
pub struct Accepted {
    /// The bundle to store, with the index regenerated and the meta advanced.
    pub bundle: Bundle,
    /// The changes against the bundle that was installed.
    pub changes: Changes,
}

/// Decides whether the files collected from the VM after reconciliation are
/// the owner's new memory.
///
/// `collected` is every markdown file found under the VM's memory root. The
/// store's meta is not in the VM and is carried over from `before`, then
/// advanced to `last_position` and `date`.
pub fn accept(
    before: &Bundle,
    collected: Bundle,
    date: NaiveDate,
    last_position: Option<i64>,
) -> Result<Accepted, Rejected> {
    let mut bundle = collected;
    bundle.remove(META);
    bundle.remove(INDEX);
    let mut problems = bundle.lint();
    problems.retain(|problem| problem.path != INDEX);
    if problems.is_empty()
        && let Err(errors) = bundle.refresh_index()
    {
        problems.extend(errors);
    }
    let mut meta = before.meta();
    meta.last_dream = Some(date);
    if last_position.is_some() {
        meta.last_task_position = last_position;
    }
    bundle.set_meta(&meta);
    let changes = Changes::between(before, &bundle);
    let substantive = changes
        .put
        .keys()
        .chain(changes.delete.iter())
        .any(|path| path != META && path != INDEX);
    if substantive
        && bundle
            .get(&Bundle::log_path(date))
            .is_none_or(|log| log.trim().is_empty())
    {
        problems.push(LintError {
            path: Bundle::log_path(date),
            problem: "reconciliation changed memory without writing the day's log".into(),
        });
    }
    if bundle.get(PROFILE).is_none() && before.get(PROFILE).is_some() {
        problems.push(LintError {
            path: PROFILE.into(),
            problem: "reconciliation removed the profile".into(),
        });
    }
    if !problems.is_empty() {
        return Err(Rejected { problems });
    }
    Ok(Accepted { bundle, changes })
}

/// The instructions every ordinary task gets about memory.
pub const fn agent_instructions() -> &'static str {
    "Your memory of this person is in /home/user/memory: profile.md is the one-pager, index.md lists every record with its aliases, and the records are markdown files under preferences/, people/, places/, accounts/ and tasks/. Grep it by alias before asking the person something they may have told you before. It is read-only for you: a nightly reconciliation writes it from what happened during the day. The one file you write is today's journal, /home/user/memory/journal/<today>.md: append dated one-line notes about what you learned about the person or did for them, as you work. Never write a password, a code or a document's contents there."
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Meta, fixtures::sample};

    fn material(tasks: usize) -> DayMaterial {
        DayMaterial {
            owner: "identity:abc".into(),
            date: NaiveDate::from_ymd_opt(2026, 9, 21).unwrap(),
            tasks: (0..tasks)
                .map(|index| TaskMaterial {
                    thread: format!("conversation-{index}"),
                    prompt: format!("Book a table for two on Friday, task {index}"),
                    result: "Booked at Ca l'Isidre at 20:30.".into(),
                    finished_at: Some(
                        DateTime::parse_from_rfc3339("2026-09-21T18:05:00Z")
                            .unwrap()
                            .into(),
                    ),
                })
                .collect(),
        }
    }

    #[test]
    fn the_prompt_carries_the_rules_the_material_and_the_closing_checklist() {
        let prompt = dream_prompt(&material(2));
        assert!(prompt.starts_with("You are reconciling this person's memory for 2026-09-21."));
        assert!(prompt.contains("/home/user/memory"));
        assert!(prompt.contains("## Task 2 at 2026-09-21 18:05 UTC (conversation conversation-1)"));
        assert!(prompt.contains("Book a table for two on Friday, task 1"));
        assert!(prompt.contains("Never record a password"));
        assert!(prompt.contains("Write log/2026-09-21.md"));
        assert!(dream_prompt(&material(0)).contains("No tasks completed"));
    }

    #[test]
    fn dream_task_ids_carry_the_date_and_the_last_position() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        assert_eq!(dream_task_id(date, Some(42)), "dream:2026-09-21:42");
        assert_eq!(dream_task_id(date, None), "dream:2026-09-21:");
        assert_eq!(
            parse_dream_task_id("dream:2026-09-21:42"),
            Some((date, Some(42)))
        );
        assert_eq!(parse_dream_task_id("dream:2026-09-21:"), Some((date, None)));
        assert_eq!(parse_dream_task_id("live:abc"), None);
        assert_eq!(parse_dream_task_id("dream:yesterday:1"), None);
    }

    #[test]
    fn a_clean_reconciliation_is_accepted_with_a_fresh_index_and_advanced_meta() {
        let before = sample();
        let date = NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let mut after = before.clone();
        after.remove(INDEX);
        after.insert("people/maria.md", "---\nid: maria\ntype: person\naliases: [maria, partner]\nupdated: 2026-09-22\n---\n- Partner; they live together at [[home]] (2026-09-22).\n");
        after.insert(PROFILE, "# Xavi\n\nLife context: founder in Barcelona, lives with Maria ([[maria]]) at [[home]] (2026-09-22).\n");
        after.insert(
            Bundle::log_path(date),
            "- Added people/maria.md from task 1 (2026-09-22).\n",
        );
        let accepted = accept(&before, after, date, Some(17)).unwrap();
        assert!(
            accepted
                .bundle
                .get(INDEX)
                .unwrap()
                .contains("people/maria.md (maria, partner)")
        );
        assert_eq!(
            accepted.bundle.meta(),
            Meta {
                last_task_position: Some(17),
                last_dream: Some(date)
            }
        );
        assert!(accepted.changes.put.contains_key("people/maria.md"));
        assert!(accepted.changes.put.contains_key(META));
        assert!(accepted.changes.delete.is_empty());
    }

    #[test]
    fn a_reconciliation_that_breaks_the_contract_or_forgets_the_log_is_rejected_whole() {
        let before = sample();
        let date = NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let mut broken = before.clone();
        broken.insert("people/maria.md", "Maria is great.\n");
        let rejected = accept(&before, broken, date, None).unwrap_err();
        assert!(rejected.to_string().contains("people/maria.md"));
        let mut silent = before.clone();
        silent.insert("preferences/wine.md", "---\nid: wine\ntype: preference\naliases: [wine]\nupdated: 2026-09-22\n---\n- Likes Chianti (2026-09-22).\n");
        let rejected = accept(&before, silent, date, None).unwrap_err();
        assert!(
            rejected
                .to_string()
                .contains("without writing the day's log")
        );
        let mut lost_profile = before.clone();
        lost_profile.remove(PROFILE);
        lost_profile.insert(Bundle::log_path(date), "- removed profile\n");
        assert!(accept(&before, lost_profile, date, None).is_err());
    }

    #[test]
    fn an_unchanged_bundle_is_accepted_without_a_log_and_only_advances_meta() {
        let before = sample();
        let date = NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let accepted = accept(&before, before.clone(), date, Some(3)).unwrap();
        let changed: Vec<&String> = accepted.changes.put.keys().collect();
        assert_eq!(changed, [META]);
    }

    #[test]
    fn material_is_tasks_or_a_journal_newer_than_the_last_dream() {
        let bundle = sample();
        assert!(has_material(&material(1), &Bundle::new()));
        assert!(
            has_material(&material(0), &bundle),
            "an undreamed journal is material"
        );
        let mut dreamed = bundle;
        dreamed.set_meta(&Meta {
            last_task_position: None,
            last_dream: NaiveDate::from_ymd_opt(2026, 9, 22),
        });
        assert!(!has_material(&material(0), &dreamed));
        assert!(!has_material(&material(0), &Bundle::new()));
    }
}
