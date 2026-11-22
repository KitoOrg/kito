# 0019 · A review sent whole, suggestions, and what changed since

Status: accepted
Date: 2026-11-22

## Context

A lane was reviewed one comment at a time: each went to its agent as it
was written, and the verdict was given apart. The person asked on
2026-11-22 for review in the diff as people know it: comments gathered
and sent together with the verdict, suggested lines that are applied
with one click or by the agent, files marked as viewed, a bar with the
verdict, the approvals, the checks and integrating always at hand, and
what changed since one's own review. A request for changes tells the
agent at once; an approval counts for the lane as it was, and is given
by somebody who may write who is not its owner (as before).

## Decision

- **`submit_review`** takes a lane, a verdict (`approve`,
  `request_changes`, `comment`), words for the lane as a whole and the
  comments. Every comment's target is checked before any is written;
  then the comments are written, then the verdict given: the agent is
  told them together, before its next edit. A request for changes with
  comments and no words says "see the comments".
- **A suggestion** is lines a comment on lines puts in their place
  (`suggestion` of a comment, kept with it). The agent is told it with
  the comment. **`apply_suggestion`** writes it into the lane's working
  copy where the lines are now, and resolves the comment; refused when
  the lines are gone. A copy written on another machine is brought the
  change as any change of the lane on the server.
- **A request for changes that stands is told to the agent** as a
  comment is, once.
- **A verdict keeps what it saw**: for each file the lane changed, its
  content then (`seen`). **`changes_since_review`** answers, for the one
  who asks, each file whose content is not what they saw at their last
  verdict, with its hunks against it.
- Comments being written wait in the browser until the review is sent;
  what was viewed is kept there too, with the file's content then.
- Both new fields are optional in the records: a store written before
  reads as it was, and an older Kito reads a newer store, ignoring them.

## Consequences

- A draft is in one browser: written on a phone, it is not on the
  laptop.
- "Since my review" counts from a verdict, not from a comment alone.
