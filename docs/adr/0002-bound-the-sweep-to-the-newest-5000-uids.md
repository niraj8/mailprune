# Bound the sweep to the newest 5,000 UIDs

ADR 0001 makes a sweep uninterruptible and the TUI inert for its length, so the sweep's length is the wait the user cannot escape. This ADR bounds it: a sweep reads the newest 5,000 UIDs and stops. `m` — load (m)ore — sweeps the next 5,000.

The bound exists to bound **the wait**, not the data (headers cost ~200 bytes each, so 5,000 of them is a megabyte) and not the relevance of old mail (which is the user's judgement, not ours).

## The window

- **Unit: UIDs, a fixed count.** Not wall-clock. A time-bounded sweep gives a different window on every launch, so counts change with the network rather than with the mail, and "why is that stack gone" has no answer. A UID count is reproducible, printable in the title, and testable.
- **5,000, as a `const`.** Five FETCHes at today's `FETCH_CHUNK` of 1,000. Not config, not a flag: `m` is already the escape hatch, and a config key is a promise nobody has asked for. The number is one line — revisit it against a measurement on a real account.
- **UIDs, not returned headers.** Trashed mail leaves dead UIDs that return nothing, exactly as `SCAN_BUDGET` already reasons. Counting headers would let a heavily-trashed mailbox sweep unboundedly to fill its quota — the unbounded wait this bound exists to stop. The cost is that a window after heavy trashing holds fewer than 5,000 live messages.
- **The same bound every sweep.** The first sweep and every `m` are the same size, so "how long will this take" has one answer the user can learn.
- **Per account, applied identically.** A budget shared across accounts would make one account's window depend on another's mail.
- **Swept at first use, not at launch.** With N accounts, sweeping all of them at startup multiplies the wait the bound is protecting. The first account sweeps at launch; the others sweep on the first `Tab` to them.
- **No persistence across runs.** Every launch starts a fresh window. Restoring one needs the on-disk cache, and its counts would have been computed against mail that may no longer exist.
- **No backfill.** Trashing most of a window does not trigger another sweep. An unrequested sweep is an unrequested inert TUI, at the worst possible moment.

## What the title says

The pane title already carries `loaded of total msgs`. It becomes:

- `newest 5,000 of 137,482 msgs` while the window is bounded. "newest" is the word that stops the count reading as a mailbox total, and it is short enough to survive the truncation ladder.
- `all 3,120 msgs` when the sweep reached the end of the mailbox. The user has to be able to tell "this is a window" from "this is everything" — it decides whether they trust the list. `m` then reports that there is nothing behind it, rather than today's `all clear — press m to load 40 more senders`.

A sweep that fails part-way — a `NO` on chunk 3 of 5, or a timeout — keeps the stacks that landed, shows the error, and lets `m` retry. The title states the count actually swept. That failure is about the window's edge, not about a stack, so it must not be called **partial**.

## Fan-out survives, smaller

Fan-out today is `SEARCH FROM addr` **and** a FETCH of every matching header, because it is building stacks. After the sweep, stacks come from the sweep, so only the UID set is still needed:

- **Fan-out becomes the SEARCH alone**, run when the user acts. The FETCH half is deleted with the sender-at-a-time load path.
- Dropping fan-out entirely was rejected: it would reduce "this sender is junk" to "this sender is junk since roughly March", which is the promise the tool is built on.
- **The confirm prompt runs that SEARCH first** and states the real number: `trash 400 messages from Foo (12 in view)`. The round trip is not added — it is the one the action needs anyway, moved ahead of the prompt. Otherwise a stack showing 12 deletes 400 messages the user never saw a number for.
- **Partial stops describing a stack.** Nothing is fanned out until the user acts, so a stack in the list can no longer be partial. It becomes an action-time error: the SEARCH was refused, so the action reached only part of that sender's mail. CONTEXT.md and README both need this change.

## One prominent channel for "wait" and "answer"

The busy spinner and the confirm prompt share the status row today (`draw_status`). Under ADR 0001 the whole TUI goes inert during a sweep, which the status row is too quiet to convey. Both move to a **centered alert** in the middle of the screen: the sweep's inert state and the action confirm use the same slot, so the user learns one place to look for "the app is doing something" and "the app wants an answer". The status-row spinner retires.

What that alert looks like — its frame, its progress representation, how it clears — is not decided here.

## Consequences

- `R` reconnects **and re-sweeps the window from scratch**, discarding stacks. It is the recovery path after a dead socket, and a socket that died mid-sweep leaves a window whose size nobody can know. This makes `R` an inert-TUI event like any other sweep.
- Counts on screen are window counts; the number the user confirms is mailbox-wide. Those two numbers differ by design, and the confirm prompt is the only place they are reconciled.
- `SENDERS_PER_BATCH`, `SCAN_BUDGET` and `DISCOVERY_CHUNK` all die with the discovery path. `FETCH_CHUNK` stays.
- README's "the stacks on screen are the 40 most recent senders" paragraph, and its `~214` partial-count explanation, both become wrong. They ship with the implementation, not before it.
