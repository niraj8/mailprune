# Sweep mechanics: sequence-range fetch, no mid-sweep render, sort at completion

ADR 0001 chose the sweep; ADR 0002 bounded it to the newest 5,000 UIDs. This one decides how the sweep actually runs.

## No `SEARCH ALL`

The load today opens with `uid_search("ALL")` — the whole UID list, newest-first, under a 60-second timeout. At 137k messages that search is the first wait, and it is the last operation whose cost still grows with the mailbox. A bounded window does not need it: `EXISTS` gives the message count `N`, and the newest 5,000 are the sequence range `N-4999:N`.

**The sweep fetches by sequence range, re-anchored off a fresh `EXISTS` each time, and tracks the window as a distance back from the top.** Sequence numbers move — arrivals push everything down, and the user's own trashing expunges and shifts everything below it up — so the window can slip by however many messages arrived while the user was triaging. Messages are deduped by UID when the stacks are rebuilt, which the store gets for free, so a repeat is invisible and only a miss is possible, bounded by session-length arrivals.

Rejected: remembering the lowest UID swept and re-anchoring to it. It is stable, but locating that UID's sequence number costs a `UID SEARCH` — reinstating the operation this decision exists to delete. Also rejected: snapshotting `N` once at launch, which the first trash invalidates.

## Nothing renders until the window completes

The TUI is inert during a sweep (ADR 0001) and a centered alert holds the screen (ADR 0002). **The list does not merge chunk by chunk behind it.** A list re-sorting and reflowing under a modal the user cannot dismiss is motion they can neither act on nor stop.

So the alert is the feedback, and it shows `3,000 of 5,000 · 41 stacks`:

- The denominator is **UIDs**, because the bound is in UIDs. Counting headers returned would never reach 5,000 in a mailbox full of dead UIDs, so the last frame would lie.
- The stack count rides alongside, because it is the number the user is actually waiting for.

This supersedes ADR 0001's claim that the first stacks land after one round trip. Chunking is retained for two other reasons: each command keeps its own bounded timeout, and a sweep that dies part-way keeps the chunks that landed — the short window of ADR 0002.

**`FETCH_CHUNK` stays at 1,000**, five commands per window. One 5,000-UID FETCH would make every failure total, which would leave the short window a concept nothing can produce. `uid_fetch` streams, so progress ticks per message regardless of chunk size — granularity is not an argument for smaller chunks.

## Sorting and rebuilding

- **Stacks are rebuilt from the accumulated messages when the sweep completes**, through the same `build_stacks` path `g` already uses. Nothing is on screen to sort before then, so chunk arrival order cannot affect the final list. An incremental merge would be a second grouping implementation to keep in sync with the first, for microseconds on a 10,000-message store.
- **Selection follows the stack it was on, by key**, across the completion sort — it may land anywhere on screen. The user pressed `m` mid-triage with a stack in mind; holding the row index instead retargets the cursor silently, which matters the moment the next key is `d`.
- **An action removes its messages from the store and rebuilds.** The stack disappears. Leaving the row as a receipt was rejected: the receipt already exists in the action log, which was kept deliberately (#10 proposed deleting it and was closed not-planned), and a row whose mail is already gone from the server invites a second `d`.

## Failure

- **`m` after a short window retries the unfetched remainder of that window** before advancing. Skipping it would leave a permanent hole in the middle of the window that nothing in the UI could explain.
- **The alert becomes the error and waits for a keypress**: `sweep failed at 2,400 of 5,000 · press m to retry, any key to continue`. It is the one channel for "wait or answer" (ADR 0002), and an incomplete list is the thing the user most needs told — the status row is where that goes unread.

## Consequences

- `SEARCH ALL` disappears from the load path. `SEARCH` survives only as fan-out, one per action (ADR 0002).
- The store is keyed by UID and holds every message from every window in the session. At 5,000 messages a window and ~200 bytes a header this stays trivial.
- A message that arrives mid-session can be missed by the window boundary. The mailbox total in the title comes from `EXISTS`, so it stays truthful even when the window has slipped past it.
