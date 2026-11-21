# 0010 · A lane says which assistant works in it

Status: accepted
Date: 2026-11-21

## Context

A lane says who works in it by a name the agent chose (`alice`) and by
the person it works for. It does not say what that agent is: Claude
Code, Codex, ZCode, and on which model. A person who watches several
lanes reads their work differently by that, and a team that runs more
than one assistant wants to see which did what. The person asked for it
on 2026-11-21: a mark on each lane for the assistant, and the model
where it can be known.

Kito is told neither today. What each side can say, as their own
documentation has it (read on 2026-11-21):

- An MCP client names itself when it connects (`clientInfo`: a name and
  a version). Every assistant does, with nothing to set up. It does not
  say its model.
- Claude Code gives its hooks a `session_id` every time, and `model`
  when a session starts (not always: not after `/clear`). It marks what
  it runs with `CLAUDECODE=1`. The hook before an edit is not given the
  model.
- Codex gives its hooks `session_id` and `model` every time.
- The agent itself often knows the model it is, from what it was told
  when it started. It can also be wrong.

## Decision

- **A lane keeps an `assistant`**: a name (`claude-code`), and when
  known a version and a model. It is in the lane's record (`Lane` in
  `kito-core`), optional, and in what `lanes`, `lane_status`, `board`
  and `open_lane` answer. A lane nobody said anything of has none.
- **It is said by the assistant's side, in three places**:
  - `open_lane` takes `assistant` (`name`, `version`, `model`).
    `kito-mcp` fills it with the name and version the program gave when
    it connected, and with `model` if the agent gave the tool one (a new
    optional argument, to leave out rather than guess). `kito lane open
    --assistant NAME [--model M]` is the same by hand.
  - `notices` takes `assistant`: `kito-mcp` asks it after every tool
    that names a lane, and says which program asks. A lane another
    assistant took over is known by its first question.
  - `before_edit` takes `assistant` and `session`: the lane the edit is
    in keeps what is said. `kito hook before-edit` says the name given
    with `--assistant` (for the hooks of other assistants, which
    plugins write), and `--model`, `--session`, or what the assistant
    wrote on the standard input (`model`, `session_id`).
  - `assistant_session` (new, needs `read`, records nothing): a session
    says what it runs on when it starts, before it is in any lane.
    `kito hook session-start` asks it. The daemon remembers the last
    256 sessions while it runs; an edit that names its session adds
    what the session said to what it says itself.
- **A name is said, or worked out, and the two are kept apart.** In a
  hook Claude Code is known only by the mark it leaves in the
  environment, and a program made from it may leave the same. So the
  hook gives `claude-code` as a `guess`, not as a name. A guess names a
  lane nobody named, and completes a lane of the same program. Where a
  program named itself otherwise, the guess is left out with the model
  that came with it: one program's name with another's model is worse
  than no model.
- **What is said later completes or replaces**. The same program (the
  same name, or one that continues the other: `codex`,
  `codex-mcp-client`) adds what it says to what is known. Another
  program replaces all of it: the model of the one before is not its
  own. A version or a model alone says nothing of a lane no program was
  named for.
- **It is kept as it is said, within limits**: lower case for the name,
  64 characters, and only what names, versions and models are written
  with. Anything else is dropped. No list of known assistants is in
  Kito: whoever shows a lane decides how to draw `claude-code`.
- **It is a label.** It is what the assistant's side said of itself:
  nothing checks it, and nothing is allowed, refused or counted by it.
  The model an agent gave by itself is the least sure of all, and what a
  hook says later replaces it.

The API stays version 2: an optional field in answers, optional
parameters, one method added.

## Consequences

- Stores written before read as they did: the field is absent. A lane
  written by this version is read by an older Kito, which ignores the
  field and drops it the next time it writes the lane.
- A lane opened by hand and edited by hand says nothing, and neither
  does an assistant with no MCP client and no hook.
- Claude Code's model is known from the first edit after a session
  started with the daemon running. A daemon that restarted forgets the
  sessions: lanes keep what they were told, and a lane opened after says
  the model at the next session start. A model changed in the middle of
  a session (`/model`) is not seen until then.
- Through a server it is the same: the directory's daemon passes the
  three on. A server older than this one does not know
  `assistant_session`: the hook goes on without it.
- A program made from Claude Code, with its hooks and its mark, is named
  by what it says to the MCP server, and its lanes then say no model
  until its plugin gives its hooks `--assistant`. A lane it works in
  that nobody named (opened by hand) is taken for Claude Code's.
- A lane named for one assistant and then edited by Claude Code with no
  question through the MCP server keeps the first name, and says no
  model of the second.
- Not done: a mark chosen by a plugin (its own icon), what a lane ran on
  before (only the last is kept), and counting by assistant or model.
