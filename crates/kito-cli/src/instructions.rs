//! The files assistants read their instructions from, written by Kito
//! from what it keeps (ADR 0012): `CLAUDE.md` for Claude Code, `AGENTS.md`
//! for Codex, ZCode and the others that read it.
//!
//! Each file is Kito's rules, then the instructions for every assistant
//! (`all/`), then those of the assistants that read it, between two marks.
//! What the file says outside the marks is the person's own: it stays, and
//! is not shared. The first time Kito writes a file that was there before,
//! what it said becomes an instruction of the repository, for everyone.

use kitod::client::Client;
use serde_json::{json, Value};
use std::path::Path;

/// How Kito's part of the file begins: the plugins that write `AGENTS.md`
/// know a part that begins `<!-- kito: begin` as Kito's, and leave it be.
pub const BEGIN: &str = kitod::instructions::BEGIN;
/// How every part written by Kito or by one of its plugins begins.
const MARK: &str = "<!-- kito: begin";
const END: &str = kitod::instructions::END;

/// A file an assistant reads, and the folders of instructions it gets.
struct Target {
    file: &'static str,
    folders: Vec<&'static str>,
    /// Where what the file said before Kito wrote it is kept.
    adopted: &'static str,
}

fn mentions(path: &Path, what: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.contains(what))
}

/// The files of the assistants set up for Kito in `root`, and which are not.
fn targets(root: &Path) -> Vec<(Target, bool)> {
    let claude = mentions(&root.join(".claude/settings.json"), "kito hook");
    let codex = mentions(&root.join(".codex/config.toml"), "kito-mcp");
    let zcode = mentions(&root.join(".zcode/config.json"), "kito-mcp");
    let mut agents = vec!["all"];
    agents.extend(codex.then_some("codex"));
    agents.extend(zcode.then_some("zcode"));
    vec![
        (Target { file: "CLAUDE.md", folders: vec!["all", "claude"], adopted: "claude/project" }, claude),
        (Target { file: "AGENTS.md", folders: agents, adopted: "all/project" }, codex || zcode),
    ]
}

/// `text` without the parts between Kito's marks, and without the rules
/// older versions of Kito wrote in `CLAUDE.md`.
pub fn own_part(text: &str) -> String {
    let mut rest = text.to_owned();
    while let Some(from) = rest.find(MARK) {
        // A mark whose part was taken out by another program: the mark alone.
        let to = match rest[from..].find(END) {
            Some(to) => from + to + END.len(),
            None => rest[from..].find('\n').map_or(rest.len(), |to| from + to),
        };
        rest.replace_range(from..to, "");
    }
    let rest = crate::claude::without_rules(&rest);
    let lines: Vec<&str> = rest.lines().filter(|line| line.trim() != END).collect();
    let text = lines.join("\n");
    let text = text.trim();
    match text.is_empty() {
        true => String::new(),
        false => format!("{text}\n"),
    }
}

/// What an assistant is told: Kito's rules, then each instruction of the
/// folders it reads.
pub fn compose(items: &[Value], folders: &[&str]) -> String {
    let pairs: Vec<(&str, &str)> = items
        .iter()
        .map(|item| (item["name"].as_str().unwrap_or_default(), item["text"].as_str().unwrap_or_default()))
        .collect();
    kitod::instructions::compose(&pairs, folders)
}

/// Writes the files of the assistants set up in `root`, takes away those
/// of the assistants that are not, and answers what was done. Nothing is
/// done when the daemon does not keep instructions (an older one).
pub fn write(root: &Path, client: &mut Client) -> Result<Vec<String>, String> {
    let mut said = Vec::new();
    let Ok(mut got) = client.call::<Value>("instructions", json!({})) else { return Ok(said) };
    for (target, wanted) in targets(root) {
        let path = root.join(target.file);
        let was = std::fs::read_to_string(&path).ok();
        let own = was.as_deref().map(own_part).unwrap_or_default();
        let ours = was.as_deref().is_some_and(|text| text.contains(BEGIN));
        if !wanted {
            // Kito's file, and nobody reads it any more.
            if ours && own.is_empty() {
                std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                said.push(format!("removed {}", target.file));
            }
            continue;
        }
        // Written by hand, or by an older Kito: shared from now on.
        let mut kept_outside = own.clone();
        if !ours && !own.is_empty() {
            let name = kitod::instructions::name_of(target.adopted)?;
            let taken = got["items"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| item["name"] == name.as_str() && item["from"] == "repository");
            if !taken {
                let _: Value = client
                    .call("instructions_set", json!({ "name": name, "text": own }))
                    .map_err(|e| e.to_string())?;
                got = client.call("instructions", json!({})).map_err(|e| e.to_string())?;
                said.push(format!(
                    "what {} said is the instruction {name} now, shared with everyone who works here",
                    target.file
                ));
                kept_outside.clear();
            }
        }
        let items: Vec<Value> = got["items"].as_array().cloned().unwrap_or_default();
        let mut text = compose(&items, &target.folders);
        if !kept_outside.is_empty() {
            text = format!("{}\n\n{text}", kept_outside.trim_end());
        }
        if was.as_deref() != Some(text.as_str()) {
            std::fs::write(&path, &text).map_err(|e| format!("{}: {e}", path.display()))?;
            said.push(format!("wrote {}", target.file));
        }
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_own_part_is_what_is_outside_the_marks_and_the_old_rules() {
        let text = format!("# Shop\n\nUse tabs.\n\n{BEGIN}\nrules\n{END}\n");
        assert_eq!(own_part(&text), "# Shop\n\nUse tabs.\n");
        let plugin =
            "<!-- kito: begin (written by `kito assistant add codex`) -->\nrules\n<!-- kito: end -->\n";
        assert_eq!(own_part(plugin), "");
        let old = format!("# Shop\n\n{}", crate::claude::RULES);
        assert_eq!(own_part(&old), "# Shop\n");
        // What is left once another program took the rules out.
        assert_eq!(own_part(&format!("{BEGIN}\n")), "");
        assert_eq!(own_part(&format!("# Shop\n\n{BEGIN}\n\n# More\n")), "# Shop\n\n\n\n# More\n");
        assert_eq!(own_part(&format!("# Shop\n{END}\n")), "# Shop\n");
    }

    #[test]
    fn each_file_gets_the_rules_then_its_folders() {
        let items = vec![
            json!({ "name": "all/style.md", "text": "Tabs.\n" }),
            json!({ "name": "claude/brief.md", "text": "Be brief.\n" }),
            json!({ "name": "codex/plan.md", "text": "Plan first.\n" }),
        ];
        let claude = compose(&items, &["all", "claude"]);
        assert!(claude.starts_with(BEGIN));
        assert!(claude.contains(crate::claude::RULES));
        assert!(claude.contains("\nTabs.\n\nBe brief.\n"));
        assert!(!claude.contains("Plan first."));
        assert!(claude.ends_with("<!-- kito: end -->\n"));
        assert_eq!(own_part(&claude), "");
    }
}
