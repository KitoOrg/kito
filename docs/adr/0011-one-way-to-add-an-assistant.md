# 0011 · One way to add an assistant, and the ones a machine has

Status: accepted
Date: 2026-11-21

## Context

Setting a coding assistant up for Kito took different commands for each:
Claude Code was set up by `kito init` or `kito claude`; Codex by `kito
plugin add codex`, `kito plugin enable codex`, then `kito assistant add
codex`; ZCode by the same three and `kito zcode hooks install`. A person
who uses two assistants had to learn three ways. The person asked on
2026-11-21 that the command be the same for every assistant, and that
the assistants already on a machine be found when Kito is installed, with
a question whether to set them up.

## Decision

- **`kito assistant add <name>…` sets any assistant up, whole.**
  - `claude` is Kito's own: it writes what `kito claude` writes.
  - Any other name is a plugin's. When no plugin enabled here sets it up,
    the one installed that does is enabled here; when none is installed,
    the plugin of that name is installed from the registry first, then
    enabled. Each step is said. Enabling gives the plugin what its
    manifest asks for, as `kito plugin enable` does.
  - Then the plugin's own `assistant setup` runs. For ZCode that now
    includes its hooks, which live in the person's own ZCode settings:
    without them nothing holds ZCode to the rules.
  - `kito assistant remove <name>` takes out what `add` wrote in the
    project, Claude Code's included. What is the person's own (ZCode's
    hooks) stays: other projects use it.
  - `kito claude` stays, as `kito assistant add claude` said shorter.
- **Kito knows which assistants a machine has**: Claude Code (`claude`),
  Codex (`codex`), ZCode (`zcode`), each by its program on the `PATH` or
  its settings directory in the home directory (`~/.claude`, `~/.codex`,
  `~/.zcode`). `kito assistant list` says, for each, whether it is on
  this machine and how it is set up. The list is short and in Kito: an
  assistant a plugin knows that is not in it is still added by name.
- **Which assistants a new project gets is the person's to say, once**:
  `kito assistant default <name>…` (or `--none`) writes it in the
  personal configuration (`[assistants] default`). `kito init`, `kito
  import` and `kito connect` set those up.
  - With nothing said, and somebody at the terminal, they find the
    assistants on the machine and ask, one question for each.
  - With nobody at the terminal (scripts, an agent running `kito init`),
    they do as before: Claude Code only, and they name the other
    assistants found, with the command that adds them.
  - `--assistant <name>` (again for more) says it for one project;
    `--no-assistants` sets none up; `--no-claude` leaves Claude Code
    out. `KITO_NO_CLAUDE` still means: nothing by itself.
- **The install script asks once.** After installing, `install.sh` looks
  for the same assistants, asks for each whether new projects should get
  it, and writes the answer with `kito assistant default`. With nobody to
  ask, it says what it found and the command.
- A plugin that cannot be installed or enabled during `init` does not
  stop `init`: the repository is made, and what failed is said with the
  command to try again.

## Consequences

- A person types the same command for every assistant, and is asked at
  most twice in their life (when installing, and in a project where they
  never said).
- `kito init` may now reach the registry, for an assistant whose plugin
  is not installed: only when that assistant was chosen.
- Scripts and agents that run `kito init` see no change: nobody is asked
  where nobody can answer.
- Detection reads only the `PATH` and three directories: an assistant
  installed somewhere else is not found, and is added by name.
- Writing ZCode's hooks on `add` changes a file outside the project; it
  is said when it is done, and `kito zcode hooks remove` undoes it.
