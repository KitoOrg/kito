# 0015 · Tasks: what needs doing, and the lanes that do it

Status: accepted
Date: 2026-11-22

## Context

A lane said why it was opened, and could name a task in words, but Kito
had nowhere to say "this needs doing" before somebody opened a lane, nor
to see what was asked for and where it stands. GitHub issues reached Kito
only as lanes (kito-github). The person asked on 2026-11-22 for tasks:
work items for lanes, which agents may take up when told, synchronised
both ways with GitHub issues, managed from the command line, the web
client and its "Needs you".

## Decision

- **A task is `task#<n>`** of a repository: a title, a text, who asked
  for it, whom it is for (a person or an agent), and a state. It is kept
  by Kito as metadata (`.kito/tasks.json`), never in the project's files,
  and shared through the server as the rest is: reading needs `read`,
  changing needs `write`.
- **Lanes do tasks.** A lane opened with `task: "task#3"` is one of the
  task's lanes; a task has as many as it needs, and a lane is for one at
  most. A lane is refused for a task that is not there or not open.
- **States**: `open`; `in_lane` while one of its lanes is open; `ready`
  when it has lanes and all are closed; `done` and `dropped`, said by a
  person. Kito never closes a task: once it is `ready`, the person who
  asked sees it in "Needs you" and closes it, or reopens it.
- **Agents take tasks up when told.** The MCP tools `tasks` and `task`
  read them; `claim_task` takes one up, refused when another agent did;
  `task_done` says the agent thinks it is done, and the task stays open.
  Nothing starts an agent: that is for the person (D-163 stands).
- **Comments on a task** are told to the agents in its lanes, once each,
  with their notices (`task_comment`), as what a person said.
- **Where it comes from**: a task may carry a `source` its plugin
  understands (`{"kind": "github", ...}`): kito-github makes tasks of
  issues and closes an issue when its task is done.
- Commands: `kito task [list] | show | add | comment | assign | done |
  reopen | drop`; `kito lane open --task task#3`.

## Consequences

- What is asked for is visible before anybody works on it, and lanes say
  what they were for in a way Kito can follow.
- Tasks have no history beyond their comments and events: who changed a
  title is an event, not a record.
- Tasks are per repository; nothing spans repositories yet.
