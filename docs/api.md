# The daemon's API · version 2

What a program asks the daemon of a repository, and what plugins
([ADR 0005](adr/0005-plugins.md)) are answered. `kito`, `kito-mcp` and
`kito-server` use nothing else.

This is version **2**: version 1 (Kito 0.19 to 0.21) and `plugin_call`
(Kito 0.22). A plugin written for version 1 runs as it did. `ping`
answers the version a daemon speaks (`api`).

- Adding a method, or an optional field to an answer, does not change the
  number. A program ignores the fields it does not know.
- Removing a method, or changing what one means, does; the daemon then
  answers the version before as well, for one more release.

## How to ask

JSON-RPC 2.0, one message a line, on the repository's socket: a Unix
socket at `.kito/kitod.sock` (when that path is too long for one,
`kito-<hash of the directory>.sock` in the temporary directory); on
Windows a named pipe, `\\.\pipe\kito-<hash of the directory>`, which the
file `.kito/kitod.pipe` stands for. The daemon is started by `kito daemon start`, or by
any `kito` command that needs it.

The first line says a key, alone:

```json
{"key": "…64 hexadecimal characters…"}
```

A plugin says the one Kito put in its environment (`KITO_PLUGIN_KEY`,
with `KITO_REPO`, the repository's directory, `KITO_API`, and
`KITO_SOCKET`: the path of the socket, or on Windows the name of the
pipe, so that a plugin need not work it out). The key of
a plugin opens what the repository gave that plugin (the permissions
below) and nothing else. Then requests and answers, a line each:

```json
{"jsonrpc": "2.0", "id": 1, "method": "lanes", "params": {}}
{"jsonrpc": "2.0", "id": 1, "result": [ … ]}
{"jsonrpc": "2.0", "id": 2, "error": {"code": -32020, "message": "…"}}
```

Errors that mean something of their own:

| Code | Meaning |
| --- | --- |
| `-32020` | Refused: it is not done, and asking again will not help. The message says why, and what to do instead. A permission that is missing is this. |
| `-32021` | The lane is written from another machine now. |
| `-32022` | No such lane, or it is closed. |
| `-32601`, `-32602`, `-32603`, `-32700` | As JSON-RPC says: unknown method, wrong parameters, an error inside, a line that is not a request. |

A lane is named `lane/3`, `3`, or by the name of a permanent lane
(`staging`); `main` is the main lane. Parameters in brackets may be left
out. Paths are relative to the repository, with `/`.

The answers are the types the daemon serializes, in `crates/kitod/src`:
the file named in the last column defines each, field by field, with what
it means.

A lane says the assistant its agent runs in when that side said it
(`assistant`: `name`, and `version` and `model` when known;
[ADR 0010](adr/0010-assistant-in-a-lane.md)), in `lanes`, `lane_status`,
`board` and the answer of `open_lane`. It is what the assistant said of
itself: a label, by which nothing is decided. Where `assistant` is a
parameter it may carry `guess` in place of `name`: a name that was
worked out and not said, which names a lane nobody named and is left out,
with what comes with it, where a program named itself otherwise.

## Anybody who reached the daemon

| Method | Parameters | Answers |
| --- | --- | --- |
| `ping` | | `version`, `api`, `pid` |

## `read`

| Method | Parameters | Answers | Defined in |
| --- | --- | --- | --- |
| `status` | | The daemon (`pid`, what it watches) and, under `repo`, the repository: files, ops, checkpoints, protection, checks, what was last pushed | `server.rs`, `repo.rs` (`Status`) |
| `lanes` | `[all]` | The open lanes (with `all`, the closed ones too) | `lanes.rs` (`LaneInfo`) |
| `lane_status` | `lane` | One lane, with its conflicts and the comments that wait | `lanes.rs` (`LaneInfo`) |
| `lane_diff` | `lane`, `[paths]` | What the lane changes: files, hunks, symbols | `review.rs` (`LaneDiff`) |
| `lane_files` | `[lane]`, `[paths]` | The files of a lane, each with the ID of its content | `files.rs` (`FileEntry`) |
| `get_blobs` | `blobs` | Contents by their IDs, in base64 | `server.rs` |
| `board` | | Who is on what: every open lane with its scope and where it stands | `scope.rs` |
| `who_touches` | `target` | The lanes that declared or changed a function, a file or a directory | `scope.rs` |
| `log` | `[lane]`, `[limit]`, `[path]` | The last changes, newest first | `repo.rs` |
| `why` | `file`, `symbol`, `[lane]` | The history of a function | `repo.rs` |
| `why_graph` | `target`, `[lane]`, `[uses]` | Why a function, a file or lines are the way they are | `why.rs` (`WhyGraph`) |
| `reasons` | `[about]`, `[limit]` | Earlier work, found by the words of its reason | `why.rs` (`ReasonHit`) |
| `notes` | `[lane]`, `[on]` | The notes left on code | `notes.rs` (`NoteInfo`) |
| `comments` | `lane`, `[all]` | What was said about a lane, each comment followed by its answers | `comments.rs` (`CommentInfo`) |
| `review` | `lane` | Where the review of a lane stands, and what is missing | `verdicts.rs` (`ReviewInfo`) |
| `changes_since_review` | `lane` | What changed in the lane since the one who asks last approved it or asked for changes: each file, with its hunks against what they saw (ADR 0019) | `review.rs` (`ChangesSince`) |
| `conflicts` | `[all]` | The conflicts that wait for a decision (with `all`, the decided ones too) | `kito-integrate` (`ConflictInfo`) |
| `integration_result` | `job`, `[wait_ms]` | How an integration ended, waiting for it if it has not | `kito-integrate` (`IntegrationResult`) |
| `events` | `[after]`, `[wait_ms]` | What happened since event `after`, waiting up to `wait_ms` for something: `events`, and `next` to ask with | `rpc.rs` (`Event`) |
| `plugins` | | The plugins this repository names | `plugins.rs` (`PluginInfo`) |
| `plugin_call` | `ask`, `[params]` | Asks the plugin of the same name on the server this directory is connected to (`kito-<name> answer <ask>` there), and returns what it answers. Refused in a repository of its own | `connected.rs`, `kito-server` |
| `sync` | `[lane]` | Records what is saved and not recorded yet; changes nothing else | `server.rs` |
| `before_edit` | `path`, `[lane]`, `[lines]`, `[whole]`, `[assistant]`, `[session]` | Whether an edit may be made, and what to read first. What it says of the assistant that asks, and what its session said, is kept by the lane | `guard.rs` (`EditAnswer`) |
| `after_shell` | `[since_ms]`, `[lanes]`, `[main]` | What a command changed where it should not have | `guard.rs` |
| `reasons_behind` | `lane` | The reasons behind what a lane replaced, not told yet | `guard.rs` (`Replaced`) |
| `assistant_session` | `session`, `assistant` (`[name]`, `[version]`, `[model]`) | Nothing. What the session of an assistant runs on is remembered while the daemon runs (the last 256), and said of the lanes that session then edits in: nothing of the repository changes | `server.rs` |
| `instructions` | | What the repository's agents are told (ADR 0012): the kitchen attached, each instruction with where it comes from (`kitchen` or `repository`), and the kitchen's it took away | `instructions.rs` (`Instructions`) |
| `instructions_text` | `[assistant]` (`claude` by default) | What that assistant reads: Kito's rules, then the instructions for every assistant and for it, between Kito's marks | `instructions.rs` (`compose`) |
| `instructions_history` | | Who changed which instruction, when, the text before and after (the last 200), the latest first | `instructions.rs` (`history`) |
| `tasks` | `[state]` (`open` by default, `all`, `in_lane`, `ready`, `done`, `dropped`), `[for]` | The tasks (ADR 0015), each with its state, whom it is for, who took it up and its lanes | `tasks.rs` (`TaskInfo`) |
| `task` | `task` (`task#3`) | One task, with what was said on it | `tasks.rs` (`TaskInfo`) |
| `search` | `[q]` (words, all of them), `[kinds]`, `[since_ms]`, `[until_ms]`, `[who]`, `[where]`, `[outcome]`, `[limit]` | What matches (ADR 0016): reasons, changes, lanes, tasks, comments, notes, the best first | `insights.rs` |
| `stats` | `[since_ms]`, `[until_ms]` | Lane flow by week and its medians, per assistant and model, busy files and functions, review | `insights.rs` |

## `write`

| Method | Parameters | Answers | Defined in |
| --- | --- | --- | --- |
| `open_lane` | `agent`, `intent`, `scope`, `[task]`, `[base]`, `[on]`, `[target]`, `[assistant]` (`name`, `[version]`, `[model]`: what the agent runs in, ADR 0010) | The lane, its working copy, and why what it declared is the way it is | `lanes.rs`, `guard.rs` |
| `close_lane` | `lane` | The lane as it was closed | `lanes.rs` (`LaneInfo`) |
| `extend_scope` | `lane`, `scope` | The scope now, and the why of what was added | `guard.rs` (`ScopeWhy`) |
| `set_intent` | `summary`, `[lane]`, `[task]`, `[prompt]` | The intent the next changes are recorded with | `lanes.rs` |
| `rename_symbol` | `symbol`, `new_name`, `[lane]` | What was renamed, and the uses that followed | `rename.rs` |
| `put_file` | `lane`, `path`, `content`, `[executable]`, `[base]`, `[writer]` | What became of a file saved from elsewhere: recorded, merged, or a conflict | `files.rs` (`Put`) |
| `remove_file` | `lane`, `path`, `[base]`, `[writer]` | The same, for a removal | `files.rs` (`Put`) |
| `claim_writer` | `lane`, `writer` | Makes this machine the one that writes the lane | `server.rs` |
| `notices` | `lane`, `[assistant]` | What the lane's agent must be told, taken once. Whoever asks may say what it is: the lane keeps it | `repo.rs` |
| `add_note` | `on`, `text`, `[lane]`, `[agent]` | The note | `notes.rs` (`NoteInfo`) |
| `remove_note` | `note` | The note, removed | `notes.rs` (`NoteInfo`) |
| `comment` | `lane`, `text`, `[on]`, `[reply_to]`, `[agent]` | The comment | `comments.rs` (`CommentInfo`) |
| `resolve_comment` | `comment` | The comment, resolved | `comments.rs` (`CommentInfo`) |
| `instructions_set` | `name` (`all/<file>` or `<assistant>/<file>`), `text` | `name`, `changed`. An empty text takes the kitchen's of that name away | `instructions.rs` |
| `instructions_remove` | `name` | `name`, `changed` | `instructions.rs` |
| `task_add` | `title`, `[text]`, `[for]`, `[source]` | The task asked for | `tasks.rs` (`TaskInfo`) |
| `task_update` | `task`, `[title]`, `[text]`, `[for]` (empty: nobody), `[source]` | The task | `tasks.rs` (`TaskInfo`) |
| `task_claim` | `task`, `agent` | The task, taken up by that agent; refused when another took it up | `tasks.rs` (`TaskInfo`) |
| `task_comment` | `task`, `text`, `[agent]`, `[source]` | The task; the agents in its lanes are told (`notices`, `task_comment`) | `tasks.rs` (`TaskInfo`) |
| `task_done` | `task`, `[agent]` | Done; with `agent`, that agent thinks it is, and it stays open | `tasks.rs` (`TaskInfo`) |
| `task_reopen` | `task` | Open again | `tasks.rs` (`TaskInfo`) |
| `task_drop` | `task` | Nobody will do it | `tasks.rs` (`TaskInfo`) |
| `verdict` | `lane`, `verdict` (`approve`, `request_changes`, `withdraw`), `[text]`, `[agent]` | Where the review stands now | `verdicts.rs` (`ReviewInfo`) |
| `submit_review` | `lane`, `verdict` (`approve`, `request_changes`, `comment`), `[text]`, `[comments]` (each `on`, `text`, `[suggestion]`) | A review sent whole (ADR 0019): every comment is checked before any is written, then the verdict is given. `comments`: those written; `review`: where it stands | `server.rs`, `comments.rs` |
| `apply_suggestion` | `comment` | Puts the lines a comment suggests in the place of those it is on, in the lane's working copy, and resolves it. Refused when the lines are not there any more | `comments.rs` (`CommentInfo`) |

## `integrate`

| Method | Parameters | Answers | Defined in |
| --- | --- | --- | --- |
| `request_integration` | `lane` | A job (`job#3`): `integration_result` says how it ended | `server.rs` |
| `resolve_conflict` | `conflict`, `[choice]` (`main`, `lane`), `[text]` | The conflict, decided | `kito-integrate` (`ConflictInfo`) |
| `catch_up` | `lane` | Brings into a permanent lane what its target got since | `kito-integrate` (`IntegrationResult`) |
| `pull` | | Brings in the commits of the git remote, through a lane | `pull.rs` (`Pull`) |
| `git_sync` | | Brings in what the git remote has and pushes, now (ADR 0020): `remote`, what was last `pushed`, `push_error`, `pull_waiting` as they were when asked | `server.rs` |

## `admin`

| Method | Parameters | Answers |
| --- | --- | --- |
| `checkpoint` | `label` | The checkpoint made of the main lane as it is |
| `rollback` | `to`, `[paths]`, `[dry_run]` | What going back to a checkpoint changes, or changed |
| `protect` | `[on]` | Whether the main lane is protected now |
| `configure` | `[checks]`, `[push]`, `[approvals]`, `[kitchen]` (a directory, `null` for the standard kitchen) | The rules of the repository now |
| `create_lane` | `name`, `[from]` | A permanent lane |
| `export_git` | | Exports the checkpoints to git |
| `import_scip` | `file` | Reads who uses what from a SCIP index |
| `reload` | | Reads the configuration again |
| `plugin_enable` | `name`, `[permissions]` | The plugin, as the repository has it now |
| `plugin_disable` | `name`, `[purge]` | Whether something changed. The plugin's key is removed; with `purge`, what it kept in the repository too |

A method this page does not list needs `admin`.

## Never for a plugin

`plugin_key` (the key of a plugin, given to whoever holds the socket's)
and `shutdown`.

## In a directory connected to a server

The daemon of a connected directory (`kito connect`) answers the same
methods by asking the server, as the person who connected, and adds
`attach`, `detach`, `take` (`write`) and `mirror_status` (`read`) for the
copies of lanes it keeps.

It answers plugins too ([ADR 0007](adr/0007-kitchens.md)): not those a
person enabled, which is not done there (`plugin_enable` and
`plugin_disable` are refused), but those the server enabled for its
clients (`kito-server plugin enable <name> --for-clients`), with what of
a repository the server gave them, if the person has them installed.
`plugin_call` is how such a plugin asks its counterpart on the server:
the server runs `kito-<name> answer <ask>` with the parameters on its
input and `KITO_SERVER_DIR`, `KITO_ASKED_BY`, `KITO_ASKED_REPO` and
`KITO_ASKED_ROLE` in its environment, and the last line it prints is the
answer. A plugin reaches the plugin of its own name and no other.
