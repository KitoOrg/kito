# 0009 · The registry is signed

Status: accepted
Date: 2026-11-21

## Context

ADR 0008 made a registry of files, and tied a download to what the
registry listed by its SHA-256. It left out who the registry is: "who can
write to `kito.sh` decides what a name installs", and so does whoever
sits between a person and `kito.sh`, or takes the host that serves it.
The checksum is no help there: it is read from the same place as the
lie. Signatures were named as the next step; the person asked for them
on 2026-11-21, before anything is published.

## Decision

- **A registry has a key** (Ed25519). The secret half signs; the public
  half is told to everybody, as 64 hexadecimal digits.
- **Every file of a registry has its signature beside it**
  (`index.json.sig`, `plugins/<name>.json.sig`): 64 bytes, as
  hexadecimal, over the file's bytes as they are served.
- **A registry whose key is known is taken only as that key signed it.**
  A file with no signature, with another's, or changed since it was
  signed, is refused whole, and nothing of that registry is used.
  - The key of the registry at `kito.sh` is written in Kito itself. It
    is not made yet: until it is, that registry is taken as its address
    gives it, which is said.
  - For any other registry, `KITO_REGISTRY_KEY` says its key. Several,
    with commas: one of them is enough, which is how a key is replaced.
  - A key that cannot be read is an error, never "no key".
  - With no key known, a registry is taken as it is, and `plugin add`
    says that nothing but its address says the files are its own.
- **A signed index is numbered and does not go back.** Signing gives the
  index the next `serial`. Kito remembers the highest it saw of each
  registry (`registries.json`, with a person's plugins) and refuses a
  lower one: an old index is still signed, and serving it again is how
  an old version with a known fault would be installed.
- **A plugin's file is held to the index.** It is signed by itself, so
  an old one is still signed too: it must have the version the index
  says is the latest, or it is an old file and is refused.
- **Signing is Kito's own**: `kito registry key <file>` makes a key (the
  secret in a file for its owner only, never written over; the public
  half printed), and `kito registry sign <directory> --key <file>` signs
  a registry kept in a directory. A registry whose files do not agree
  (the index names a version a plugin's file lacks) is not signed at
  all. One implementation signs and checks.

## Consequences

- Who holds the secret decides what a name installs; who serves the
  files, or carries them, no longer does. The secret is kept off the
  machine that serves the registry.
- A person's first look at a registry cannot tell an old index from the
  current one: the number protects from then on.
- A lost secret means a new key, told to everybody again: for `kito.sh`,
  a new release of Kito. Two keys at once make the change gradual.
- The archives are not signed: the signed file says their checksum,
  which is as good.
- Not in this ADR: keys with an end date, several signers required at
  once, a log of everything ever signed.
