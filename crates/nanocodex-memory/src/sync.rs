//! Moving a bundle into the owner's VM and reading the agent's journal back.
//!
//! The VM's shell is reached one command at a time, with the command text
//! going in and at most 128 KiB of output coming back, so a bundle travels as
//! base64 inside commands, a few files per command, and comes back one file
//! per command. Paths are the bundle's own and are shell-quoted regardless.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::bundle::Bundle;

/// Where the bundle lives in the VM.
pub const VM_ROOT: &str = "/home/user/memory";
/// One install command stays well under the shell transport's limits.
const MAX_COMMAND_BYTES: usize = 90_000;
/// The shell returns at most this much output, so a collected file must
/// encode within it.
pub const MAX_COLLECT_BYTES: usize = 96_000;

/// The commands that replace whatever is in the VM's memory directory with
/// the bundle. Run them in order; each is independent of the shell's state.
pub fn install_commands(bundle: &Bundle) -> Vec<String> {
    let mut commands = vec![format!(
        "rm -rf -- {root} && mkdir -p -- {root}",
        root = quote(VM_ROOT)
    )];
    let mut current = String::new();
    for (path, content) in bundle.files() {
        if !Bundle::is_installed(path) {
            continue;
        }
        let full = format!("{VM_ROOT}/{path}");
        let directory = full
            .rsplit_once('/')
            .map(|(directory, _)| directory)
            .unwrap_or(VM_ROOT);
        let step = format!(
            "mkdir -p -- {dir} && printf '%s' '{data}' | base64 -d > {file}",
            dir = quote(directory),
            data = STANDARD.encode(content),
            file = quote(&full)
        );
        if !current.is_empty() && current.len() + step.len() + 4 > MAX_COMMAND_BYTES {
            commands.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(" && ");
        }
        current.push_str(&step);
    }
    if !current.is_empty() {
        commands.push(current);
    }
    commands
}

/// The command that lists every markdown file in the VM's memory directory,
/// one relative path per line.
pub fn list_command() -> String {
    format!(
        "cd {root} 2>/dev/null && find . -type f -name '*.md' | sed 's|^\\./||' | LC_ALL=C sort",
        root = quote(VM_ROOT)
    )
}

/// The paths a `list_command` printed.
pub fn parse_listing(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The command that prints one file's contents as base64, or nothing when the
/// file does not exist.
pub fn collect_command(path: &str) -> String {
    let full = format!("{VM_ROOT}/{path}");
    format!(
        "if [ -f {file} ]; then base64 -w0 -- {file}; fi",
        file = quote(&full)
    )
}

/// The file a `collect_command` printed, or `None` when it did not exist.
pub fn decode_collected(stdout: &str) -> Result<Option<String>, String> {
    let data: String = stdout
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if data.is_empty() {
        return Ok(None);
    }
    if data.len() > MAX_COLLECT_BYTES {
        return Err(format!(
            "collected file exceeds {MAX_COLLECT_BYTES} encoded bytes"
        ));
    }
    let bytes = STANDARD
        .decode(data.as_bytes())
        .map_err(|error| format!("collected file is not base64: {error}"))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "collected file is not UTF-8".to_owned())
}

/// Single quotes for bash: the one form no content can break out of.
pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{META, fixtures::sample};

    #[test]
    fn install_starts_clean_and_carries_every_installed_file_as_base64() {
        let bundle = sample();
        let commands = install_commands(&bundle);
        assert_eq!(
            commands[0],
            "rm -rf -- '/home/user/memory' && mkdir -p -- '/home/user/memory'"
        );
        let joined = commands.join("\n");
        for (path, content) in bundle.files() {
            let encoded = STANDARD.encode(content);
            assert_eq!(
                joined.contains(&encoded),
                Bundle::is_installed(path),
                "{path}"
            );
        }
        assert!(!joined.contains(META));
        assert!(joined.contains("mkdir -p -- '/home/user/memory/preferences' && printf '%s' '"));
        assert!(
            commands
                .iter()
                .all(|command| command.len() <= MAX_COMMAND_BYTES + 200)
        );
    }

    #[test]
    fn large_bundles_split_across_commands() {
        let mut bundle = Bundle::new();
        for index in 0..40 {
            bundle.insert(
                format!("journal/2026-08-{:02}.md", index % 28 + 1),
                "x".repeat(5_000) + &index.to_string(),
            );
        }
        let commands = install_commands(&bundle);
        assert!(commands.len() > 2, "{}", commands.len());
        assert!(
            commands
                .iter()
                .skip(1)
                .all(|command| command.len() <= MAX_COMMAND_BYTES)
        );
    }

    #[test]
    fn collection_round_trips_and_rejects_what_it_cannot_trust() {
        let path = "journal/2026-09-21.md";
        let command = collect_command(path);
        assert_eq!(
            command,
            "if [ -f '/home/user/memory/journal/2026-09-21.md' ]; then base64 -w0 -- '/home/user/memory/journal/2026-09-21.md'; fi"
        );
        let encoded = STANDARD.encode("- note (2026-09-21)\n");
        assert_eq!(
            decode_collected(&format!("{encoded}\n")).unwrap().unwrap(),
            "- note (2026-09-21)\n"
        );
        assert_eq!(decode_collected("").unwrap(), None);
        assert!(decode_collected("not base64!").is_err());
        assert!(decode_collected(&"A".repeat(MAX_COLLECT_BYTES + 4)).is_err());
        assert_eq!(
            parse_listing("index.md\njournal/2026-09-21.md\n\n"),
            ["index.md", "journal/2026-09-21.md"]
        );
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
