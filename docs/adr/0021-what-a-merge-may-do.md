# 0021 · What a merge may do: nothing twice, never less than a line merge, and what a symbol is

Status: accepted
Date: 2026-11-23

## Context

The merge is described in pieces: how a file is read (D-043), what a
conflict is (D-044), how names are settled before the texts (D-060). No
document says what a merge must never do. Real merges of open-source
projects, replayed through Kito 0.39.2, found three things it did:

- **It merged the same thing in twice, and said nothing.** One side
  changed a line and added a line after it, the other added that same
  line at that place: the line was in the result twice (requests
  `3347146a441d`). Both sides added a method `mget` to a class: the class
  had two (redis `0c9ca0e11ca2`). The first came from the line merge,
  which applies both of two changes that touch; the second from the
  merge by symbols, where each side's new symbol has an ID of its own and
  nothing compared the two.
- **It stopped where a plain line merge does not.** One side added a
  function after the statements that close a file, the other changed one
  of those statements (express `f9256ef36fa9`). For the merge by symbols
  the statements were the end of the file on one side and what leads to
  the new function on the other, two different pieces, and the file was a
  conflict of kind `text`. Its lines compose without a doubt.
- **It took two symbols for one.** A property's getter and its setter are
  defined under one name, and the history told symbols of one name apart
  by the order they were created in, while a file is read in the order
  they are written in. When the two orders part, the base's setter is
  compared with the other side's getter (flask `864875099762`). The same
  happens to overloads: a new one written above an old one takes its
  place, and a change the other side made to the old one lands in the
  new one, in silence.

The first and the third change what `Symbol.name` means and what the
merge answers, which the rules of this directory ask to write down.

## Decision

Three rules, which the merge holds whatever else it does.

### 1. Nothing is merged in twice

What both sides added the same, at the same place, is in the result
once.

- **Lines.** In the line merge (`merge3`), two changes that touch are
  still both applied (that choice is not reopened here), but the lines
  one ends with and the other starts with were added by both at the same
  place, and are written once: "two" changed to "TWO" with "added" after
  it, and "added" put after "two", give "TWO", "added". The same lines
  added or removed by both sides are one change wherever each diff
  happened to place them along lines that repeat (a blank line, a
  closing brace): they are compared once moved down as far as they go.
- **Symbols.** A symbol the main lane added to a scope and one the lane
  added to the same scope with the same kind and name are one thing
  added twice:
  - with the same text they are written once, as the main lane has the
    symbol (its ID, its place); the integration says so
    (`auto_resolutions`, kind `same_addition`);
  - with two texts the sides do not go together: a conflict of kind
    `body_body` on the main lane's symbol, with no text at the base, the
    main lane's and the lane's as the two sides. `main` keeps the main
    lane's, `lane` takes the lane's symbol in its place, a text replaces
    both. Never two definitions side by side;
  - unless the language lets the two live side by side
    (`kito_parse::namesakes`): overloads with other parameters in Java,
    C#, C++ and TypeScript; a second `impl` block in Rust; a namespace
    opened again in C++, C# and PHP; declarations TypeScript merges;
    Go's `init`; the name `_`. Those are kept, both, as before.
  The text that leads to the two (comments, blank lines) is merged as
  text between them; if it differs by more than space the file is a
  conflict of kind `text`, as for any text the two sides changed.
- **Only what is new on both sides is one thing added twice.** A symbol
  the base has, which a side moved into the scope, is itself wherever
  it is, and what the other side did to it where it stood must reach
  it. Where one side moved a function into a class or a file and the
  other wrote there one of its name (a copy of it, say), the two are
  not twins and cannot both stay under one name: the file is a
  conflict of kind `text`.
- **The same text is not always the same thing.** A class or a file
  that is byte for byte the same on both sides is taken as it is only
  if it is made of the same symbols on each, or if none of its symbols
  is one the other side changed elsewhere. Otherwise it is merged
  member by member. Until 0.39.4 it was taken whole: a function one
  side moved in, which the other had changed where it stood and copied
  there, lost that change without a word.

### 2. Never less than a plain line merge

Where the merge of a file ends in conflicts of kind `text` only, and a
plain line merge of the whole file has none, the result is that line
merge. A plain line merge (`merge3_apart`) is what `git merge-file`
does: the changes of the two sides are both applied where they neither
overlap nor touch; where they do, the two sides must have made the same
thing of those lines. The integration says which files were merged so
(`auto_resolutions`, kind `line_merge`, the file as `symbol`), and the
symbols of the result keep their IDs: each definition is the symbol the
main lane has under that name, else the lane's.

It applies only where the text of the file is all there is to merge: a
file one of whose symbols is in another file on a side is left with its
conflict, since a change to a symbol follows it across files and lines
do not know that. Conflicts of the other kinds are not settled this way:
a file that also has a function both sides changed, or one a side
removed and the other changed, keeps all its conflicts.

Rule 1 comes first. Lines do not know that a function both sides added,
or both moved, to two different places of a file is one function: a line
merge keeps it twice. The line merge of a file read as symbols is
therefore taken only if it can be read as symbols too, every definition
in it is one a side has, and no symbol is two definitions (a function
one side renamed and moved, and the other moved elsewhere, is in a line
merge under both its names). A result that fails this is not what the
two sides made, and the file keeps its conflict.

Kito may compose more than a line merge does. It composes less only
there, where the line merge would write a definition twice.

### 3. What a symbol is

A symbol is known by its container, its kind and its name. Where two
things of one container may have one name, what tells them apart is part
of the name, as a word before it:

| Language | Definition | Name |
| --- | --- | --- |
| Python | `@total.setter`, `@total.deleter`, any `@total.<word>` on `def total` | `setter total`, `deleter total`, `<word> total` |
| JavaScript, TypeScript | `set total(v)`, also `static` and abstract | `set total` |

The getter keeps the plain name: it is the property (`total`), and what
most code and most people mean by that name. `ParsedSymbol.formerly`
holds the name without the word. The name as the code writes it is still
the last word of the recorded one, as for `Server.Start` in Go: an
explicit rename changes that word.

A history written before has both halves under the plain name. It is
read as it is: a setter is recognized under the name it had, when no
symbol goes by that name, and the next save of its file records the
rename (`Rename total → set total`) with the symbol's ID unchanged. Its
notes and its reasons stay with it. Where such a history has under the
plain name as many symbols as the file has by it, with a word or
without, they are those in the order of the file, which is the order
they were numbered in: a setter written above its getter is the first
of the two.

Symbols that share a name and are not told apart by a word (overloads, a
`static` method and a method, a name assigned twice, two `impl` blocks)
are still numbered in the order they were created. The merge no longer
trusts that order blindly: a file in which a symbol of a side is not
what the base has under its ID, while a namesake of it is, is not merged
as symbols. It is merged as text, like a file with a syntax error; if
one of its symbols is in another file on a side, it is a conflict of
kind `text`.

## Consequences

- No format changes. A conflict both sides added is an existing kind
  with an absent text, which `Conflict` already allowed. `Symbol.name`
  is a string as before; older binaries read a history with `set total`
  in it, and show it.
- The names of setters change, once, where a file is saved: scopes,
  notes and `why` targets written as `file:Class.total` now mean the
  getter alone, and the setter is `file:Class.set total`.
- Merges that were made and are not made any more: two methods of one
  name added by both sides with different texts (a conflict now); a file
  whose overloads were taken for one another and whose changes still
  happened to compose (text now, or a conflict).
- Merges that are made and were not: the files of rule 2; the same
  lines removed by both sides next to lines that repeat.
- Not decided here, and unchanged: whether changes on lines that touch
  should be merged at all; imports added at the same place by both
  sides (a conflict of kind `text`, as in a line merge); telling
  overloads apart in the history itself, which needs the history to
  know more of a symbol than its name.
- Overloads are told apart by what their parameter lists say, names
  included: `log(String msg)` and `log(String message)`, each added by
  a side, are still both kept, and it is the compiler that refuses
  them. Telling overloads by their types alone needs more of each
  grammar than the merge reads today.
- A line merge, Kito's or git's, still writes a block twice when both
  sides add it and one of them also changes the line right next to it:
  the diff then takes the end of one function for the end of the other,
  and the two blocks are no longer the same lines. In a file Kito reads
  as symbols, rule 1 covers it; in other files nothing does.
