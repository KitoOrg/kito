# 0016 · Search and numbers over what a repository did

Status: accepted
Date: 2026-11-22

## Context

Kito keeps why every change was made, by whom and for whom, and the
lanes, tasks, comments and notes around them; `reasons` found earlier
work by its words. Nothing searched the rest, and nothing counted how
work flows, how each assistant and model does, where code is busy, or
how review goes. Some moments that matter for those numbers (an edit
refused or held back, checks that failed, conflicts) were answered and
told as events, and then forgotten. The person asked on 2026-11-22 for
search and analytics, in the command line, the web client and for
agents, per repository and across a server, seen by whoever may read.

## Decision

- **Activity is kept**: every event the daemon tells but saves (which
  the history holds) is appended to `.kito/activity.jsonl`, metadata out
  of the project's files. An edit held back once for reasons the agent
  was not told is now an event too (`edit_held`).
- **`search`** (read): what matches every word, among reasons and what
  they changed, lanes, tasks, comments (on lanes and tasks) and notes;
  filtered by kind, time, who (a person, an agent, an assistant or a
  model), where (a file, a directory, a function) and outcome
  (`integrated`, `given_up`, `open`, `refused`, `conflicted`,
  `failed_checks`), the best first, then the latest.
- **`stats`** (read): lanes opened, integrated and given up week by
  week; the median time from open to integrated, waiting to go in, and
  to a first review; per assistant and model, lanes, integration rate,
  conflicts, refusals, held edits and failed checks; the busiest files
  and functions; per reviewer, approvals, changes asked for and time to
  a first look.
- **Who sees them**: whoever may read the repository, through the
  server as anything else.
- **Across a server**: the command line asks each repository the person
  may use (`--everywhere`), the web client the same; the server adds
  nothing of its own.
- Commands: `kito search <words> [--kind] [--since] [--until] [--who]
  [--where] [--outcome] [--everywhere]`, `kito stats [--since] [--until]
  [--everywhere]`; the MCP tool `search`.

## Consequences

- Numbers that need the activity start when a repository first runs this
  version: before, there is nothing to count; the answer says from when.
- The log grows with the work, by a line per event; nothing prunes it
  yet.
- A closed lane keeps its scope and not its files: what it changed is
  found through the reasons it landed.
