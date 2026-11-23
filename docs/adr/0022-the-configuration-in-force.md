# 0022 · The configuration in force is the one that is written

Status: accepted
Date: 2026-11-23

## Context

The daemon read `.kito/config.toml` when it started, and again only when
a command of Kito's own changed it (`kito protect`, `kito identity`, the
settings a server changes). A person or a script that wrote the file
changed nothing until something restarted the daemon, and nothing said
so. A team that added its checks (`[verify] commands`) to a repository
in use had none: `kito status` went on saying `checks: []`, and a lane
that broke them was integrated with no verification.

The file is the place the documentation sends people to. What it says
has to be what the repository does.

## Decision

**The configuration is read again every time the daemon takes the
repository up** (a request, a capture, a scan, an integration), and by
the repository itself before each integration (`Repo::refresh_config`).
The file is read and compared with what was read last; if it is the
same, nothing else is done.

It is not left to file events: `.kito/` is not captured, events are late
by the time of a debounce, and on some file systems they do not come at
all (the daemon knows, and scans). An integration asked for right after
the file is written must go by that file. Reading it where it is used
has no such window, and costs one small read.

**A file that cannot be read puts nothing in force.** Broken TOML, or a
file that cannot be opened: the configuration read before it changed
stays in force, the checks included, and never the defaults. It is said
where somebody looks:

- `kito status` says it first, and `status` answers
  `repo.config_error`;
- the answer of every integration carries `config_error`, however it
  ended: the checks that ran were those of the configuration read
  before;
- the daemon writes it in its log once, and once more when the file can
  be read again.

A daemon that starts on a file it cannot read does not start, as before:
there is no configuration to keep.

**What follows the file, and what waits for a restart.** Everything the
repository reads when it uses it follows the file at once:

| Section | Follows the file |
| --- | --- |
| `[verify]` | the commands, their time limit, how many lanes are checked together |
| `[main]` | `protected` |
| `[agents]` | `why`, `files_in_history` (what is ignored is worked out again) |
| `[sandbox]` | everything |
| `[git]` | `export_on_checkpoint`, `branch`, `email_domain`, `push` |
| `[lanes]` | `share` (for lanes opened from then on), the permanent lanes' `verify`, `branch`, `integrate`, `required` |
| `[review]`, `[scope]`, `[kitchen]`, `[plugins.*]` | everything |
| `[semantic.servers.*]` | everything: the servers are set up again |
| `[author]` | everything: who signs is worked out again |
| `[capture]` | `ignore`, `max_file_bytes` |

Three settings are read by the daemon when it starts, because they are
how it watches: `[capture] watch`, `debounce_ms` and `rescan_secs`.
When the file gives one of them another value than the daemon runs with,
`status` says so (`restart_needed`, the names of the settings; `kito
status` prints them with what to do), until the daemon has started
again.

Who signs also depends on what is outside the repository (the
environment, the personal configuration, git's): those are read when the
daemon starts, when the repository's own file changes, and when `kito
identity` asks.

## Consequences

- Nothing to migrate. `status` and the integration's answer gain fields
  that are absent when there is nothing to say.
- A change of the file is in force for the request that follows it,
  including one that is half-way through several steps: an integration
  whose checks are running finishes by the rules as they are when it
  records.
- A file caught half-written by an editor that writes in place is, for
  that instant, a file that cannot be read: it is said, nothing changes,
  and the next look reads it whole.
- The commands of Kito that change the configuration write the file and
  the daemon's copy together, as before; the file is then found changed,
  read, and found to say what is in force already.
