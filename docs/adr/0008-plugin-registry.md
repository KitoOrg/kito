# 0008 · A registry of plugins

Status: accepted
Date: 2026-11-21

## Context

A plugin is installed from a directory somebody has on their machine
(`kito plugin add --path <dir>`): they cloned a repository and built it,
or were handed the files. ADR 0005 left "by name, from a release" for
when a plugin had one. There are now eight (`web`, `github`, `oidc`,
`codex`, `claude`, `zcode`, `kitchen`, and what others will write), a
team's server tells people "install `kitchen`" (ADR 0007), and there is
nowhere to get it from.

Decided with the person on 2026-11-21: the first registry is a static
index on `kito.sh`, served by GitHub Pages with the website; nothing is
published yet (Kito and its plugins are private); release binaries are
built on one machine for Linux until the project's CI runs again.

This is a format others will write to and a new way for programs to
reach a machine, which the ADR rules ask to write down first.

## Decision

### 1. The registry is files

No service, no accounts. Under a base address (`https://kito.sh/
registry`), read with plain HTTPS:

```text
index.json              every plugin: its name, one line, its latest version
plugins/<name>.json     one plugin: its versions, and the files of each
```

```json
{ "registry": 1,
  "plugins": { "kitchen": { "about": "A team's kitchen", "latest": "0.2.0" } } }
```

```json
{ "name": "kitchen", "about": "A team's kitchen",
  "homepage": "https://github.com/KitoOrg/kito-kitchen",
  "versions": { "0.2.0": {
      "api": 2,
      "files": { "linux-x86_64": {
          "url": "https://kito.sh/downloads/kito-kitchen-0.2.0-linux-x86_64.tar.gz",
          "sha256": "…", "size": 1843200 } } } } }
```

- An archive holds one directory with what `plugin add --path` takes:
  the program `kito-<name>` and `kito-plugin.toml`.
- Platforms are named as Kito's own releases are: `linux-x86_64`,
  `linux-aarch64`, `macos-arm64`, `macos-x86_64`, `windows-x86_64`.
- A file's `url` may be anywhere that speaks HTTPS (a GitHub release):
  the registry lists, it need not host.
- `"registry": 1` is the version of this format. A field a program does
  not know is ignored.

### 2. Installing by name

- `kito plugin add <name>` (and `<name>@<version>`), and the same for
  `kito-server plugin add`: reads the plugin's file, takes the latest
  version that fits this Kito's API and this platform, downloads it,
  checks its SHA-256 against the registry's, unpacks it apart, checks
  that the manifest inside is the plugin and the version that were asked
  for, and installs it as `--path` does. Anything that does not match is
  refused, and nothing is installed.
- `kito plugin search [words]` lists what the registry has;
  `kito plugin outdated` says which installed plugins have a newer
  version that fits; `kito plugin update [<name>]` installs it.
- A plugin a repository enabled and a person lacks is said with how to
  get it by name.
- **Which registry**: `https://kito.sh/registry` unless `KITO_REGISTRY`
  or `registry = "…"` in a person's configuration says another: a
  team's own, on its network. `file://` is taken too, for a machine with
  no network and for the tests. Plain `http://` is refused but for this
  machine.
- Installing a plugin still enables nothing: a repository, or a server,
  does that (ADR 0005, 0006).

### 3. What is trusted

- The registry's files are the root: who can write to `kito.sh` decides
  what a name installs. The checksum ties a download to what the
  registry listed, wherever it is hosted; it does not say the registry
  was right. Signatures of the registry's files (a key in Kito, the
  index signed off-line) are the next step and are not in this ADR.
- A plugin is a program run as the person (ADR 0005 already says so).
  Installing one by name is deciding to trust its author and the
  registry; `plugin add` says where the file came from and its checksum.
- Nothing is run while installing: an archive is unpacked and read.
  Paths in an archive that leave its directory, links, and files that
  are not plain files refuse the archive.

### 4. Being listed

- A plugin is listed by a change to the registry's files in the
  website's repository, reviewed like any other.
- `scripts/registry-add.sh` (in the website's repository) writes a
  plugin's file from an archive: it reads the manifest inside, computes
  the checksum, and adds the version.
- An author's own tooling is theirs: the registry asks for an archive
  per platform and its address.

### 5. kito.sh

- The website (`KitoOrg/kito-website`, GitHub Pages) is served at
  `kito.sh`: the documents, `install.sh`, `/registry/`, and
  `/downloads/` while files are small enough to live there (a GitHub
  release, public, when they are not).
- `curl -fsSL https://kito.sh/install.sh | sh` installs Kito itself from
  the same downloads, checking the checksum.

### 6. Taking a plugin out

Asked for with the registry (2026-11-21): what goes in by name must come
out as cleanly. Three places hold something of a plugin, and each has
its own way out:

- **A repository or a server that enabled it**: `kito plugin disable
  <name>`, `kito-server plugin disable <name>`. The plugin is run one
  last time there (`kito-<name> cleanup repository`, or `cleanup
  server`) if its manifest says it has something to take out
  (`[cleanup]`): the files an assistant's plugin wrote in the project,
  for one. Then its key is removed. What it kept of its own
  (`.kito/plugins/<name>/`, `plugins/<name>/` on a server) stays, so
  that enabling it again finds it, unless `--purge` is said.
- **A person's machine**: `kito plugin remove <name>` (for a server,
  `kito-server plugin remove <name>`, which is new). The plugin is run
  one last time (`cleanup person`): what it wrote outside any
  repository, a person's own configuration of an assistant. Then its
  directory is removed.
- A cleanup that fails is said, and the plugin is taken out all the
  same: a broken plugin must not be the one thing that cannot be
  removed. `--no-cleanup` does not run it at all.
- `remove` says which repositories it knows still enable the plugin
  (the one it is run in; on a server, the server itself), and refuses
  on a server while the server has it enabled: disable first.

### 7. A server's plugins, from a browser

Asked for with the registry (2026-11-21): installing and taking out a
server's plugins from an interface, not only on the server's machine.
Kito keeps having no interface of its own: the server answers, and the
web client (`kito-web`, behind the server) shows.

- **Administrators.** A person of the server may be one: `kito-server
  user admin <name>` (`--off`). Roles in repositories say what somebody
  may do to code; this says they may change the server itself. Nobody
  is one unless said, and it is said on the server's machine only.
- **`/-/admin/plugins`** on the server's HTTP address, for
  administrators (anybody else: 403, the same whoever they are):
  `GET` lists what is installed, what is enabled and with what, and
  what the registry has; `POST` with `install` (a name, a version),
  `enable` (the permissions given, `for_clients`), `disable`, `remove`.
  A request with a cookie must carry `X-Kito`, as for the API.
- **Only by name, only from the server's registry.** No address, no
  path, no upload: what a browser can install is what the registry the
  server was told lists, checked as section 2 says. An administrator who
  wants something else is on the server's machine.
- **Permissions are shown before they are given**: `enable` says what
  the plugin asks for, and takes what is given; nothing is enabled by
  installing.
- **Pages and sign-in plugins take effect when the server starts
  again**: said in the answer. The plugin that serves the interface
  cannot be disabled or removed from the interface.
- Every change is told as a server event (`plugin_installed`,
  `plugin_enabled`, `plugin_disabled`, `plugin_removed`, with who did
  it), and written in the server's log.

This is the first thing a browser can do that runs new code on the
server. It is as safe as an administrator's session: sessions end after
thirty days and with the person, and a server that wants none of it
gives nobody the mark.

## Consequences

- `kito` and `kito-server` learn to fetch over HTTPS and to unpack an
  archive: three libraries more (an HTTP client on the TLS they already
  have, gzip, tar; zip on Windows).
- A machine that installs by name reaches `kito.sh`, or the registry it
  was told; `--path` stays for the ones that reach nothing.
- Nothing is published by this ADR. The registry starts empty; listing
  Kito's own plugins waits for their repositories or their binaries to
  be public.
- Not in this ADR: signatures, a service with accounts and `publish`,
  private registries with sign-in, yanking a version, statistics.

## Order of work

One phase (22), released as 0.23.0, in four parts:

1. the format, `plugin add <name>`, `search`, with a registry of files
   (`file://`) in the tests; `kito-server plugin add <name>`;
2. taking a plugin out: `[cleanup]`, `disable --purge`, `remove` on a
   server, and the plugins there are brought to it;
3. administrators and `/-/admin/plugins`; the page in `kito-web`;
4. `outdated` and `update`, the website's side (`/registry/`,
   `registry-add.sh`, `install.sh`, the domain), documents and the demo.

## Amendment, 2026-11-22: where a plugin runs

The person asked why plugins that set up a coding assistant were offered
for install on a server's page. They are for the machines people work
on. Each listing now says where its plugin runs, in `runs` of
`index.json` and of each version: `server`, `repository`, or both. It is
read from the manifest by one rule (`Manifest::runs`, and the same rule
in the site's `registry-add.mjs`): a plugin with `[assistant]` never runs
on a server; one with commands, tools, events, gates, a session or a
repository's permissions runs in a repository. `kito-server plugin add`
and `enable`, and the web client's Plugins page, leave out what is not
for a server; `kito plugin add` and `enable` what is not for a
repository. A listing without `runs`, from before, is taken to run
anywhere.

## Amendment, 2026-11-23: the address of a directory, on Windows

A registry given as `file://` and a path was read on Windows only when
the path was one Windows works out itself. The one test that still
failed there showed it: a server whose directory was named in full
(`\\?\C:\…`, which is what asking Windows for a directory's own name
answers, and so what a program that was told a directory passes on)
listed nothing of its registry. Windows reads such a path as it is
written, and the `/index.json` that is added to a registry's address
was the name of nothing. `file:///C:/…`, the form such an address has
everywhere else, did not work either: what is left of it, `/C:/…`, is
no path on Windows.

On Windows, in the path of a `file://` address, every `/` is now a `\`,
and a `/` before a drive's letter is left out: `file://C:\registry`,
`file://\\?\C:\registry` and `file:///C:/registry` are one directory.
No name on Windows can have a `/` in it, so nothing that was read
before is read differently. Elsewhere the path is taken as it is
written, as before. Nothing of an address is decoded on any system: a
path with a space in it is written with the space.
