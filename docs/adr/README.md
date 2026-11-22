# ADR (Architecture Decision Record)

An ADR, written **before** the code, is required to:

- change the semantics of a data model type (`kito-core`): adding optional
  fields at the end does not require an ADR, everything else does;
- change one of the choices in the "Decisions made" section of the project
  plan (language, relationship with git, hashes, interfaces…);
- change the on-disk format in an incompatible way (the
  `golden_encoding_is_stable` test of `kito-core` fails).

Choices on points the plan does not cover are not ADRs: they go in
[`../decisions.md`](../decisions.md).

## Format

Files named `NNNN-short-title.md`, numbered in order:

```markdown
# NNNN · Title

Status: proposed | accepted | superseded by NNNN
Date: YYYY-MM-DD

## Context
What does not work with the current choice.

## Decision
What changes, with the types or formats affected.

## Consequences
Compatibility with existing data, migration, what becomes harder.
```

- [0001 · Lanes on lanes: stacked lanes, targets, permanent lanes](0001-lanes-on-lanes.md)
  (accepted)
- [0002 · Kito on Windows](0002-windows.md) (accepted)
- [0003 · Notes on code](0003-notes-on-code.md) (accepted)
- [0004 · Review of a lane](0004-review-of-a-lane.md) (accepted)
- [0005 · Plugins](0005-plugins.md) (accepted)
- [0006 · Plugins for the server](0006-server-plugins.md) (accepted)
- [0010 · A lane says which assistant works in it](0010-assistant-in-a-lane.md)
  (accepted)
- [0011 · One way to add an assistant, and the ones a machine has](0011-one-way-to-add-an-assistant.md)
- [0012 · Instructions: what agents are told, kept by Kito and out of the project](0012-instructions.md)
- [0013 · Connecting by one address, signed in in the browser](0013-connect-by-address.md)
- [0014 · Roles in each repository, and roles of the server](0014-roles.md)
- [0015 · Tasks: what needs doing, and the lanes that do it](0015-tasks.md)
- [0016 · Search and numbers over what a repository did](0016-search-and-numbers.md)
- [0017 · The server's audit log, and its backups](0017-audit-and-backups.md)
  (accepted)
- [0018 · Repositories added, archived, renamed and deleted from afar](0018-repositories-from-afar.md)
  (accepted)
