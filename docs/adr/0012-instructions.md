# 0012 · Instructions: what agents are told, kept by Kito and out of the project

Status: accepted
Date: 2026-11-21

## Context

Every assistant reads its instructions and settings from files in the
project folder: `CLAUDE.md`, `AGENTS.md`, `.mcp.json`, `.claude/`,
`.codex/`, `.zcode/`, and others. Kito wrote some of them itself and
recorded them like any file, so they went into the project's history and
its git repository. The person asked on 2026-11-21 that such files be
kept out of the repository by default; that what agents are told be
shared across agents and people through Kito and its server instead, as
metadata; that there be one standard for it; that a kitchen be attached
to every repository, and that a repository's own instructions override
its kitchen's.

## Decision

- **Instructions have one standard layout**: markdown files named
  `<folder>/<file>.md`. The folder `all` is for every assistant; a folder
  named after an assistant (`claude`, `codex`, `zcode`, …) is for that one
  only. File names are lower case letters, digits, `-`, `_` and `.`.
- **Three levels, the later winning**:
  1. Kito's rules (how to work in lanes): always first, never replaced.
  2. The **kitchen** attached to the repository: its `instructions/`
     folder, in the same layout. A repository with none attached has the
     standard kitchen, which is Kito's rules alone.
  3. The **repository's own** instructions, kept by Kito as metadata
     (`.kito/instructions/`), never in the project's files. A file of the
     repository replaces the kitchen's file of the same name; an empty
     one takes it away.
- **Kito writes the files the assistants read, and never records them.**
  `CLAUDE.md` is Kito's rules, then `all/`, then `claude/`. `AGENTS.md`
  (read by Codex, ZCode and others) is Kito's rules, then `all/`, then
  the folder of each such assistant set up here. Kito's part is between
  two marks, the first of which says it is written by Kito and how to
  change it; the plugins that write `AGENTS.md` already know a part that
  begins `<!-- kito: begin` as Kito's and leave it be. What a file says
  outside the marks is the person's own: kept, and not shared. The first
  time Kito writes a file that was there without its marks, what it said
  becomes an instruction of the repository (`claude/project.md` for
  `CLAUDE.md`, `all/project.md` for `AGENTS.md`). They are written when
  an assistant is set up or taken out, when an instruction changes, and
  when a session starts.
- **Assistant files stay out of the history and of git by default.**
  Kito does not record `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `.mcp.json`,
  `.claude/`, `.codex/`, `.zcode/`, `.cursor/`, `.cursorrules`,
  `.windsurfrules` and `.github/copilot-instructions.md` at the root of a
  repository or of a lane; and `.gitignore` gets them, for whoever uses
  git by hand. `[agents] files_in_history = true` brings back the old way.
  A file that is already in the history stays recorded, as any tracked
  file that an ignore rule names; what it said becomes a repository
  instruction when Kito first writes it.
- **Shared through the server.** In a directory connected to a server the
  instructions are the server's: reading and changing them goes to the
  server, as the person, with the person's role (reading needs `read`,
  changing needs `write`). Everyone connected gets the same files at the
  next session start. On the server, `kito-server repo set <repo>
  --kitchen <kitchen repo>` attaches a kitchen kept as a repository of the
  server; on one's own machine `kito instructions kitchen <dir>` attaches
  a directory.
- **Commands**: `kito instructions` lists them and where each comes from;
  `show [--for <assistant>]` prints what an assistant is told; `set
  <name>` (from a file or the standard input) and `remove <name>` change
  the repository's own; `kitchen <dir> | --none` attaches one;
  `write` writes the files now.
- **The kitchen's standard**: a kitchen (kito-kitchen) keeps its
  instructions in `instructions/`, in this layout, next to its behaviour,
  concepts and recipes.

## Consequences

- A project's history and its git repository hold no assistant files,
  unless the repository asks for them; a clone has none until its person
  runs `kito assistant add`.
- What agents are told is changed in one place, for everyone, and has no
  merge conflicts: it is not part of the code. It has no history either:
  each change is told as an event (`instructions`), and that is all.
- A kitchen kept as a directory on one's own machine is read where it is;
  a kitchen on a server is a repository of that server, its main lane as
  it is.
- Projects that recorded `CLAUDE.md` before keep it recorded until it is
  removed from the project in a lane; then Kito writes it.
- Amended 2026-11-22, with the web client's instructions page: Kito's
  rules and the writing of a file are the daemon's (`instructions_text`
  gives what an assistant reads), and each change of an instruction is
  kept with who made it and the text before and after
  (`.kito/instructions-history.jsonl`, the last 200;
  `instructions_history`), so an earlier text can be put back.
- Hand edits between the marks are lost at the next writing: the first
  mark says so, and `kito instructions set` is the way. Lines outside the
  marks stay, on that machine only.
- Setting Claude Code up on a protected main lane no longer needs the
  protection off: its files are not recorded.
