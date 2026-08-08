# mailprune

A terminal tool for emptying an overfull inbox. It groups mail by who sent it, so a decision made once — this sender is junk — applies to everything they ever sent.

## Language

**Stack**:
A group of messages presented as one row and decided on as a unit. Grouped by sender, or by sender and subject.
_Avoid_: Bundle, cluster, thread, conversation

**Sweep**:
The walk through a mailbox, newest message first, that turns mail into stacks. It reads message headers in chunks and merges each chunk into the stacks already on screen. It runs to the end of the window and holds the keyboard for its whole length.
_Avoid_: Load, fetch, scan, batch, discovery

**Window**:
How far back a sweep has reached. A stack's count is the sender's messages inside the window, not their whole history, so the count grows as the window widens.
_Avoid_: Budget, page, limit

**Fan-out**:
Enumerating everything one sender has in the mailbox, ignoring the window. What makes trashing a stack remove all of that sender's mail rather than the part on screen.
_Avoid_: Expand, resolve, full search

**Keep**:
A stack the user saw and chose not to act on. Recorded at exit as a decision in its own right, not as an absence of one.
_Avoid_: Skip, ignore, pass

**Partial**:
A stack the server refused to fan out, so acting on it reached only part of the sender's mail. A failure, never the normal state of a stack inside the window.
_Avoid_: Incomplete, provisional, approximate
