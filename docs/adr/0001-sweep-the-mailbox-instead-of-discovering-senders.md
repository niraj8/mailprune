# Sweep the mailbox instead of discovering senders and fanning each one out

Loading the mailbox used to sample recent headers until 40 new senders turned up, then run a search and a fetch per sender to enumerate all their mail — about 80 serial round trips with the keyboard locked before the first decision. We now sweep the mailbox newest-first in header chunks and build the stacks from what the sweep read. A sender is fanned out when the user acts on them, not for all 40 up front. What makes the wait tolerable is that the sweep is bounded (ADR 0002), not that it renders as it goes — nothing renders until the window completes (ADR 0003).

A sweep runs to the end of a bounded window and cannot be interrupted. The TUI is inert while it runs: `q` and ctrl-c respond, nothing else does. The bound, not the mailbox size, is what makes that wait finite.

## Considered options

**A connection pool running the fan-outs concurrently.** Rejected: it buys parallelism for work the sweep does not do at all, and pays for it in per-account connection limits plus timeout and reconnect semantics across N sockets.

**A second session so actions stay live during a sweep.** A pool opens N sockets to make one slow operation finish sooner; a second session would open one socket so that two things the user sees as separate — the list filling, and acting on it — do not serialise against each other. Rejected: it makes a sweep and an action interleave. The action has to fan its sender out mid-sweep, the sweep's completion sort and uid pruning have to defer so the stack indices the action captured stay valid, and two simultaneous connections per account is a limit no provider has been checked against. Bounding the window answers the same complaint — an unbounded wait — for none of that.

**Stopping the sweep when a triage key is pressed**, then acting on the frozen window. Rejected: it leaves the session mid-command for the action that follows to reason about, and the bounded window already caps the wait it was meant to escape.

**Branching on mailbox size**, sweeping small mailboxes and keeping the batch path for large ones. Rejected: the sweep serves both sizes, so the threshold was only a hedge against not having one design that works at either.

## Consequences

- A stack's count is now the sender's messages **inside the swept window**, not their mailbox-wide total. The pane title must state the window.
- **Partial** now means only that the server refused a fan-out. It never describes a stack the sweep has not yet widened past.
- Acting on a stack still reaches all of that sender's mail. That cost moves from 40 fan-outs per load to one per decision.
- The TUI is inert for the length of a sweep — every key except `q` and ctrl-c is refused, mutating or not. The UI has to say so plainly rather than swallow keys in silence.
- The window is bounded, so that inert stretch is finite and roughly constant whatever the mailbox size. `m` — load (m)ore — widens the window with another sweep, inert on the same terms. The bound's unit and default are not decided here.
- Chunking stays, but it buys neither early action nor a filling list. It bounds each command's timeout and lets a sweep that fails part-way keep the chunks that landed. Feedback is the alert's counter (ADR 0003).
- One session throughout. The sweep and the actions never overlap, so nothing concurrent is introduced.

## Not a revert of #15

#15 stopped the code fetching every header at once, because a full-header fetch costs 2–6 KB a message — 300–600 MB at 100k messages, which always tripped its timeout. The narrow header query from that same change costs roughly 200 bytes a message, about 30× cheaper, and that is what makes a chunked sweep viable. The narrow query and the chunking stay. Only the sender-at-a-time enumeration built on top of them is dropped.

## Where the specs went

#19 (the sweep) and #18 (keys live during a load) were closed as too dense to build from. This ADR carries the decision they held; #18's premise — actions running while the mailbox loads — is rejected above rather than deferred.
