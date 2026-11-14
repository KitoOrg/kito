//! The short form of `kito log`: one line for each run of saves of a file,
//! with what they meant for the symbols on the same line.
//!
//! Every save is an op, and somebody who saves often (or an editor that does
//! it by itself) fills pages with the same file. Here the saves of one file
//! that follow one another, by the same author and for the same reason, are
//! one line: the lines they added and removed are summed, and each symbol is
//! named once. Nothing is left out that `kito log -v` would show as another
//! file, another author or another moment between two checkpoints.

use kitod::repo::LogItem;

/// What a line says happened to its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    New,
    Removed,
    Renamed,
    Edited,
    /// Symbols recorded with no change to a file: the first analysis of
    /// files that were already there.
    Symbols,
}

/// What happened to a symbol, the most telling first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Created,
    Removed,
    Renamed,
    Moved,
    Signature,
    Changed,
}

struct Symbol {
    /// What tells a symbol from another one: its ID.
    key: String,
    name: String,
    renamed_from: Option<String>,
    file: Option<String>,
    verb: Verb,
    /// The file change the daemon put it under, if any.
    change: Option<String>,
}

struct Line {
    /// The most recent op of the run: where `kito rollback` returns to.
    id: String,
    at_ms: u64,
    author: String,
    path: String,
    what: What,
    /// `from old/path`, `executable`.
    note: Option<String>,
    added: usize,
    removed: usize,
    /// Some save had its lines counted.
    counted: bool,
    saves: usize,
    /// Most recent first.
    symbols: Vec<Symbol>,
    intent: Option<String>,
    /// The daemon put the symbols of a batch under this change: an older
    /// batch ends here.
    carrier: bool,
}

enum Entry {
    Checkpoint {
        /// The line as shown.
        text: String,
        label: String,
    },
    Line(Line),
}

/// How many file changes the items hold: what the daemon's limit counts.
pub fn changes(items: &[LogItem]) -> usize {
    items.iter().filter(|item| matches!(item, LogItem::Op { op, .. } if !crate::is_symbolic(op))).count()
}

fn verb(op: &str) -> Verb {
    match op {
        "AddSymbol" => Verb::Created,
        "RemoveSymbol" => Verb::Removed,
        "Rename" => Verb::Renamed,
        "Move" => Verb::Moved,
        "ModifySignature" => Verb::Signature,
        _ => Verb::Changed,
    }
}

/// A symbolic op waiting for the file change it came with.
struct Waiting {
    symbol: Symbol,
    id: String,
    at_ms: u64,
    author: String,
    intent: Option<String>,
}

/// The line of symbols that came with no file change.
fn alone(waiting: Vec<Waiting>) -> Option<Line> {
    // Going back in time the oldest comes last: the line is told as its.
    let oldest = waiting.last()?;
    let (id, at_ms, author, intent) =
        (oldest.id.clone(), oldest.at_ms, oldest.author.clone(), oldest.intent.clone());
    let symbols: Vec<Symbol> = waiting.into_iter().map(|waiting| waiting.symbol).collect();
    let mut files: Vec<&str> = symbols.iter().filter_map(|symbol| symbol.file.as_deref()).collect();
    files.sort_unstable();
    files.dedup();
    let path = match files.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        many => format!("{} files", many.len()),
    };
    Some(Line {
        id,
        at_ms,
        author,
        path,
        what: What::Symbols,
        note: None,
        added: 0,
        removed: 0,
        counted: false,
        saves: 1,
        symbols,
        intent,
        carrier: true,
    })
}

/// The items as lines, one per file change, with the symbols where the
/// daemon put them.
fn entries(items: Vec<LogItem>) -> Vec<Entry> {
    let mut out = Vec::new();
    // Symbols waiting for the file change they came with, which (going back
    // in time) comes next.
    let mut pending: Vec<Waiting> = Vec::new();
    for item in items {
        match item {
            LogItem::Checkpoint { id, label, git_commit, .. } => {
                out.extend(alone(std::mem::take(&mut pending)).map(Entry::Line));
                let git =
                    git_commit.map(|c| format!("  (git {})", &c[..c.len().min(10)])).unwrap_or_default();
                out.push(Entry::Checkpoint { text: format!("◆ {id}  {label}{git}"), label });
            }
            LogItem::Op {
                id,
                at_ms,
                author,
                op,
                path,
                detail,
                lines_added,
                lines_removed,
                intent,
                change,
                symbol_id,
                symbol,
                renamed_from,
                ..
            } => {
                if crate::is_symbolic(&op) {
                    let name = symbol.or_else(|| detail.clone()).unwrap_or_else(|| op.clone());
                    let key = symbol_id.unwrap_or_else(|| name.clone());
                    let symbol = Symbol { key, name, renamed_from, file: path, verb: verb(&op), change };
                    pending.push(Waiting { symbol, id, at_ms, author, intent });
                    continue;
                }
                let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut pending)
                    .into_iter()
                    .partition(|waiting| waiting.symbol.change.as_deref() == Some(id.as_str()));
                out.extend(alone(others).map(Entry::Line));
                let what = match op.as_str() {
                    "FileAdd" => What::New,
                    "FileRemove" => What::Removed,
                    "FileRename" => What::Renamed,
                    _ => What::Edited,
                };
                out.push(Entry::Line(Line {
                    id,
                    at_ms,
                    author,
                    path: path.unwrap_or_default(),
                    what,
                    // For an edit the only detail is that it was not counted.
                    note: detail.filter(|_| what != What::Edited),
                    added: lines_added.unwrap_or(0),
                    removed: lines_removed.unwrap_or(0),
                    counted: lines_added.is_some(),
                    saves: 1,
                    carrier: !mine.is_empty(),
                    symbols: mine.into_iter().map(|waiting| waiting.symbol).collect(),
                    intent,
                }));
            }
        }
    }
    out.extend(alone(pending).map(Entry::Line));
    out
}

/// The symbols of a batch are all under its most recent file change: each
/// goes to the change of its own file, when the batch has one.
fn to_their_files(entries: &mut [Entry]) {
    for at in 0..entries.len() {
        let Entry::Line(line) = &mut entries[at] else { continue };
        if !line.carrier || line.what == What::Symbols {
            continue;
        }
        let path = line.path.clone();
        let (stay, leave): (Vec<Symbol>, Vec<Symbol>) = std::mem::take(&mut line.symbols)
            .into_iter()
            .partition(|symbol| symbol.file.as_deref().is_none_or(|file| file == path));
        let mut stay = stay;
        for symbol in leave {
            // The older changes of the same batch follow, up to the next
            // batch that has symbols of its own.
            let home = entries[at + 1..]
                .iter_mut()
                .map_while(|entry| match entry {
                    Entry::Line(line) if !line.carrier => Some(line),
                    _ => None,
                })
                .find(|line| Some(line.path.as_str()) == symbol.file.as_deref());
            match home {
                Some(line) => line.symbols.push(symbol),
                None => stay.push(symbol),
            }
        }
        let Entry::Line(line) = &mut entries[at] else { continue };
        line.symbols = stay;
    }
}

/// Saves of one file that follow one another become one line.
fn fold(entries: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for entry in entries {
        let Entry::Line(older) = entry else {
            out.push(entry);
            continue;
        };
        match out.last_mut() {
            Some(Entry::Line(line))
                if line.what == What::Edited
                    && matches!(older.what, What::Edited | What::New)
                    && line.path == older.path
                    && line.author == older.author
                    && line.intent == older.intent =>
            {
                line.saves += older.saves;
                line.added += older.added;
                line.removed += older.removed;
                line.counted |= older.counted;
                line.symbols.extend(older.symbols);
                if older.what == What::New {
                    // A file created and then worked on: it is new.
                    line.what = What::New;
                    line.note = older.note;
                }
            }
            _ => out.push(Entry::Line(older)),
        }
    }
    out
}

/// How many names a line gives for each thing that happened.
const NAMES: usize = 3;

/// What the saves of a line meant for the symbols, each symbol once.
fn symbols(line: &Line) -> String {
    struct Seen {
        key: String,
        name: String,
        /// The name comes from an op that gave it: a later op, which knows
        /// only the name the symbol has today, does not replace it.
        named: bool,
        renamed_from: Option<String>,
        file: Option<String>,
        verbs: Vec<Verb>,
    }
    let mut seen: Vec<Seen> = Vec::new();
    // Oldest first: the order things happened in.
    for symbol in line.symbols.iter().rev() {
        let gives_name = matches!(symbol.verb, Verb::Created | Verb::Renamed);
        let at = match seen.iter().position(|s| s.key == symbol.key) {
            Some(at) => at,
            None => {
                seen.push(Seen {
                    key: symbol.key.clone(),
                    name: symbol.name.clone(),
                    named: false,
                    renamed_from: None,
                    file: symbol.file.clone(),
                    verbs: Vec::new(),
                });
                seen.len() - 1
            }
        };
        let entry = &mut seen[at];
        if gives_name || !entry.named {
            entry.name = symbol.name.clone();
        }
        entry.named |= gives_name;
        if symbol.verb == Verb::Renamed && entry.renamed_from.is_none() {
            entry.renamed_from = symbol.renamed_from.clone();
        }
        entry.verbs.push(symbol.verb);
    }

    // For each thing that happened, in the order it first did, to whom.
    let mut groups: Vec<(Verb, Vec<String>)> = Vec::new();
    for symbol in seen {
        let has = |verb: Verb| symbol.verbs.contains(&verb);
        if has(Verb::Created) && has(Verb::Removed) {
            // It came and went within the line.
            continue;
        }
        // The most telling of the things that happened to it.
        let verb = [Verb::Created, Verb::Removed, Verb::Renamed, Verb::Moved, Verb::Signature]
            .into_iter()
            .find(|verb| has(*verb))
            .unwrap_or(Verb::Changed);
        let mut name = match (&symbol.renamed_from, verb) {
            (Some(from), Verb::Renamed) => format!("{from} → {}", symbol.name),
            _ => symbol.name,
        };
        // A symbol of another file than the line's is told with its file.
        if let Some(file) = symbol.file.filter(|file| *file != line.path && line.what != What::Symbols) {
            name.push_str(&format!(" ({file})"));
        }
        match groups.iter_mut().find(|(known, _)| *known == verb) {
            Some((_, names)) => names.push(name),
            None => groups.push((verb, vec![name])),
        }
    }
    groups
        .into_iter()
        .map(|(verb, names)| {
            let mut text = names.iter().take(NAMES).cloned().collect::<Vec<_>>().join(", ");
            if names.len() > NAMES {
                text.push_str(&format!(" and {} more", names.len() - NAMES));
            }
            match verb {
                Verb::Created => format!("{text} created"),
                Verb::Removed => format!("{text} removed"),
                Verb::Renamed => text,
                Verb::Moved => format!("{text} moved"),
                Verb::Signature => format!("{text} signature changed"),
                Verb::Changed => format!("{text} changed"),
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// What happened to the file of a line, in a few words.
fn change(line: &Line) -> String {
    let note = |word: &str| match &line.note {
        Some(note) => format!("{word} ({note})"),
        None => word.to_owned(),
    };
    let mut text = match line.what {
        What::New => note("new"),
        What::Removed => "removed".to_owned(),
        What::Renamed => match &line.note {
            Some(from) => format!("renamed {from}"),
            None => "renamed".to_owned(),
        },
        What::Edited if line.counted => format!("+{} −{}", line.added, line.removed),
        What::Edited => "changed".to_owned(),
        What::Symbols => "symbols".to_owned(),
    };
    if line.saves > 1 {
        text.push_str(&format!(", {} saves", line.saves));
    }
    text
}

/// A column is as wide as its widest cell, up to a point: one long path
/// does not push every line to the right.
fn width<'a>(cells: impl Iterator<Item = &'a str>, most: usize) -> usize {
    cells.map(|cell| cell.chars().count()).filter(|n| *n <= most).max().unwrap_or(0)
}

/// The log in its short form: at most `limit` lines, checkpoints aside.
/// Returns the lines, and how many there would be without the limit: when
/// that is not more than `limit` and the items were not the whole log, the
/// last line may be a run cut short, and more items are needed.
pub fn compact(items: Vec<LogItem>, limit: usize) -> (Vec<String>, usize) {
    let mut entries = entries(items);
    to_their_files(&mut entries);
    let entries = fold(entries);
    let lines = entries.iter().filter(|entry| matches!(entry, Entry::Line(_))).count();

    // What is shown: up to the last line the limit allows.
    let mut shown = Vec::new();
    let mut left = limit;
    for entry in entries {
        if left == 0 {
            break;
        }
        if matches!(entry, Entry::Line(_)) {
            left -= 1;
        }
        shown.push(entry);
    }
    let rows: Vec<Option<(&Line, String, String)>> = shown
        .iter()
        .map(|entry| match entry {
            Entry::Line(line) => Some((line, change(line), symbols(line))),
            Entry::Checkpoint { .. } => None,
        })
        .collect();
    // The label of the checkpoint a line is under.
    let mut under: Option<&str> = None;
    let authors = width(rows.iter().flatten().map(|(line, ..)| line.author.as_str()), 16);
    let paths = width(rows.iter().flatten().map(|(line, ..)| line.path.as_str()), 40);
    let changes = width(rows.iter().flatten().map(|(_, change, _)| change.as_str()), 24);
    let out = shown
        .iter()
        .zip(&rows)
        .map(|(entry, row)| match (entry, row) {
            (Entry::Line(line), Some((_, change, symbols))) => {
                let when = crate::utc(line.at_ms);
                let mut text = format!(
                    "  {}  {}  {:<authors$}  {:<paths$}  {change:<changes$}",
                    &line.id[..line.id.len().min(8)],
                    // Month, day, hour and minute.
                    when.get(5..16).unwrap_or(&when),
                    line.author,
                    line.path,
                );
                if !symbols.is_empty() {
                    text.push_str(&format!("  {symbols}"));
                }
                // An integration is a checkpoint with the lane's task as
                // its label, and the reason of its ops says the same again.
                let said = |intent: &str| {
                    under.is_some_and(|label| {
                        intent == label
                            || (intent.starts_with("integrate ") && intent.ends_with(&format!(": {label}")))
                    })
                };
                if let Some(intent) = line.intent.as_deref().filter(|intent| !said(intent)) {
                    text.push_str(&format!("  [{intent}]"));
                }
                text.trim_end().to_owned()
            }
            (Entry::Checkpoint { text, label }, _) => {
                under = Some(label);
                text.clone()
            }
            (Entry::Line(_), None) => String::new(),
        })
        .collect();
    (out, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log {
        items: Vec<LogItem>,
        next: u64,
    }

    /// Builds a log the way things happen, oldest first; `items` gives it as
    /// the daemon does, most recent first, the symbols of a batch before its
    /// most recent file change.
    impl Log {
        fn id(&mut self) -> String {
            self.next += 1;
            format!("{:08x}{:056x}", self.next, 0)
        }

        fn file(&mut self, author: &str, op: &str, path: &str, stat: Option<(usize, usize)>) -> String {
            let id = self.id();
            self.items.push(LogItem::Op {
                id: id.clone(),
                // One minute after the other, from 2026-01-02 03:00.
                at_ms: (1_767_322_800 + self.next * 60) * 1000,
                author: author.to_owned(),
                lane: "main".to_owned(),
                op: op.to_owned(),
                path: Some(path.to_owned()),
                detail: None,
                lines_added: stat.map(|s| s.0),
                lines_removed: stat.map(|s| s.1),
                intent: None,
                change: None,
                symbol_id: None,
                symbol: None,
                renamed_from: None,
            });
            id
        }

        fn edit(&mut self, author: &str, path: &str, added: usize, removed: usize) -> String {
            self.file(author, "TextEdit", path, Some((added, removed)))
        }

        fn symbol(
            &mut self,
            change: Option<&str>,
            op: &str,
            file: &str,
            key: &str,
            name: &str,
            from: Option<&str>,
        ) {
            let id = self.id();
            // In what the daemon gives, the symbols of a change come after
            // it in time and so, most recent first, before it.
            self.items.push(LogItem::Op {
                id,
                at_ms: (1_767_322_800 + self.next * 60) * 1000,
                author: "ada".to_owned(),
                lane: "main".to_owned(),
                op: op.to_owned(),
                path: Some(file.to_owned()),
                detail: None,
                lines_added: None,
                lines_removed: None,
                intent: None,
                change: change.map(str::to_owned),
                symbol_id: Some(key.to_owned()),
                symbol: Some(name.to_owned()),
                renamed_from: from.map(str::to_owned),
            });
        }

        fn checkpoint(&mut self, id: &str, label: &str) {
            self.items.push(LogItem::Checkpoint {
                id: id.to_owned(),
                label: label.to_owned(),
                author: "ada".to_owned(),
                head: String::new(),
                git_commit: Some("0123456789abcdef".to_owned()),
            });
        }

        fn with_intent(&mut self, intent: &str) {
            if let Some(LogItem::Op { intent: slot, .. }) = self.items.last_mut() {
                *slot = Some(intent.to_owned());
            }
        }

        fn items(&self) -> Vec<LogItem> {
            self.items.iter().rev().cloned().collect()
        }

        fn lines(&self, limit: usize) -> Vec<String> {
            compact(self.items(), limit).0
        }
    }

    #[test]
    fn saves_of_one_file_that_follow_one_another_are_one_line() {
        let mut log = Log::default();
        for _ in 0..3 {
            let change = log.edit("ada", "src/auth.ts", 1, 0);
            log.symbol(Some(&change), "ModifyBody", "src/auth.ts", "0#0", "getUser", None);
        }
        let change = log.edit("ada", "src/auth.ts", 4, 1);
        log.symbol(Some(&change), "AddSymbol", "src/auth.ts", "0#1", "getAdmin", None);
        let change = log.edit("ada", "src/web.ts", 1, 1);
        log.symbol(Some(&change), "ModifyBody", "src/web.ts", "1#0", "profile", None);

        assert_eq!(
            log.lines(30),
            [
                "  00000009  01-02 03:09  ada  src/web.ts   +1 −1           profile changed",
                "  00000007  01-02 03:07  ada  src/auth.ts  +7 −1, 4 saves  getUser changed; getAdmin created",
            ]
        );
        // The limit counts lines, and says how many there are in all.
        let (lines, all) = compact(log.items(), 1);
        assert_eq!((lines.len(), all), (1, 2));
        assert_eq!(changes(&log.items()), 5);
    }

    #[test]
    fn another_author_another_reason_or_a_checkpoint_begin_a_new_line() {
        let mut log = Log::default();
        log.edit("ada", "a.txt", 1, 0);
        log.edit("ada", "a.txt", 1, 0);
        // What an integration records: its reason says what the label says.
        log.with_intent("integrate lane/1: first");
        log.checkpoint("ckpt#1", "first");
        log.edit("ada", "a.txt", 1, 0);
        log.edit("bob", "a.txt", 2, 0);
        log.edit("bob", "a.txt", 3, 0);
        log.with_intent("tidy up");
        log.edit("bob", "a.txt", 4, 0);
        log.with_intent("tidy up");

        assert_eq!(
            log.lines(30),
            [
                "  00000006  01-02 03:06  bob  a.txt  +7 −0, 2 saves  [tidy up]",
                "  00000004  01-02 03:04  bob  a.txt  +2 −0",
                "  00000003  01-02 03:03  ada  a.txt  +1 −0",
                "◆ ckpt#1  first  (git 0123456789)",
                "  00000002  01-02 03:02  ada  a.txt  +1 −0",
                "  00000001  01-02 03:01  ada  a.txt  +1 −0",
            ]
        );
    }

    #[test]
    fn a_file_created_and_then_worked_on_is_new_and_each_symbol_is_told_once() {
        let mut log = Log::default();
        let change = log.file("ada", "FileAdd", "src/billing.ts", None);
        for (key, name) in
            [("2#0", "invoice"), ("2#1", "total"), ("2#2", "tax"), ("2#3", "round"), ("2#4", "scrap")]
        {
            log.symbol(Some(&change), "AddSymbol", "src/billing.ts", key, name, None);
        }
        let change = log.edit("ada", "src/billing.ts", 3, 3);
        // Known by the name it has today, changed, renamed, and one that goes.
        log.symbol(Some(&change), "ModifyBody", "src/billing.ts", "2#0", "invoiceFor", None);
        log.symbol(Some(&change), "RemoveSymbol", "src/billing.ts", "2#4", "scrap", None);
        let change = log.edit("ada", "src/billing.ts", 1, 1);
        log.symbol(Some(&change), "Rename", "src/billing.ts", "2#1", "sum", Some("total"));
        log.file("ada", "FileRename", "src/new.ts", None);
        log.file("ada", "FileRemove", "old.txt", None);

        assert_eq!(
            log.lines(30),
            [
                "  0000000d  01-02 03:13  ada  old.txt         removed",
                "  0000000c  01-02 03:12  ada  src/new.ts      renamed",
                "  0000000a  01-02 03:10  ada  src/billing.ts  new, 3 saves  invoice, sum, tax and 1 more created",
            ]
        );
    }

    #[test]
    fn a_rename_and_symbols_of_other_files() {
        let mut log = Log::default();
        // One batch, two files: the daemon puts both symbols under the most
        // recent change.
        log.edit("ada", "src/auth.ts", 1, 1);
        let change = log.edit("ada", "src/web.ts", 2, 2);
        log.symbol(Some(&change), "Rename", "src/auth.ts", "0#0", "fetchUser", Some("getUser"));
        log.symbol(Some(&change), "ModifyBody", "src/web.ts", "1#0", "profile", None);
        // A symbol of a file the batch did not change stays where it is.
        let change = log.edit("ada", "src/web.ts", 1, 0);
        log.symbol(Some(&change), "Move", "src/other.ts", "3#0", "helper", None);
        // Symbols with no file change: the first analysis.
        log.symbol(None, "AddSymbol", "lib/a.py", "4#0", "main", None);
        log.with_intent("first analysis of the symbols");
        log.symbol(None, "AddSymbol", "lib/b.py", "5#0", "run", None);
        log.with_intent("first analysis of the symbols");

        assert_eq!(
            log.lines(30),
            [
                "  00000007  01-02 03:07  ada  2 files      symbols         main, run created  [first analysis of the symbols]",
                // The two saves of web.ts follow one another: one line.
                "  00000005  01-02 03:05  ada  src/web.ts   +3 −2, 2 saves  profile changed; helper (src/other.ts) moved",
                "  00000001  01-02 03:01  ada  src/auth.ts  +1 −1           getUser → fetchUser",
            ]
        );
    }
}
