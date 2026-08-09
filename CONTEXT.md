# mailprune

A terminal tool for emptying an overfull inbox. It groups mail by who sent it, so a decision made once — this sender is junk — applies to everything they ever sent.

## Language

**Stack**:
A group of messages presented as one row and decided on as a unit. Grouped by sender, or by sender and subject.
_Avoid_: Bundle, cluster, thread, conversation

**Sweep**:
The walk through a mailbox, newest message first, that turns mail into stacks. It reads message headers in chunks of 1,000 and rebuilds the stacks once, when the window is complete — nothing appears while it runs. It holds the keyboard for its whole length; the alert — its spinner, its counter and its bar — is the only thing that moves.
_Avoid_: Load, fetch, scan, batch, discovery

**Window**:
How far back a sweep has reached: the newest 5,000 UIDs, and another 5,000 for every `m`. A stack's count is the sender's messages inside the window, not their whole history, so the count grows as the window widens. The pane title always states it — `newest 5,000 of 137,482 msgs`, or `all 3,120 msgs` once the sweep has reached the end of the mailbox.
_Avoid_: Budget, page, limit

**Fan-out**:
The search that finds every message one sender has in the mailbox, ignoring the window. It returns UIDs and nothing else, and runs once when the user acts on a stack: its result is the number the confirm prompt states, and then the set the action works on. What makes trashing a stack remove all of that sender's mail rather than the part on screen. Grouping by sender **and subject** has no fan-out: a stack is then one thread of a sender, and `SEARCH` has no key for the normalized subject it was grouped on — so those stacks act on the mail inside the window.
_Avoid_: Expand, resolve, full search

**Keep**:
A stack the user saw and chose not to act on. Recorded at exit as a decision in its own right, not as an absence of one.
_Avoid_: Skip, ignore, pass

**Partial**:
An action the server refused to fan out fully, so it reached only part of the sender's mail. Reported when the action runs — never a state a stack sits in, because nothing is fanned out until the user acts.
_Avoid_: Incomplete, provisional, approximate

**Short window**:
A window smaller than the bound because the sweep that built it failed part-way. The stacks that landed are real; `m` retries. Distinct from **partial**, which is about one sender's mail, not the window's edge.
_Avoid_: Partial, truncated, aborted

**Alert**:
The centered 60×6 box that holds anything the user must wait for or answer — a sweep in progress, a failed sweep, an action's confirm prompt. Same frame every time; the border colour and the hint line say which. One place to look, because during a sweep there is nothing else to look at.
_Avoid_: Modal, dialog, popup, toast
