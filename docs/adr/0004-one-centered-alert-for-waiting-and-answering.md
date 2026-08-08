# One centered alert for waiting and for answering

ADR 0001 makes the TUI inert during a sweep; ADR 0002 sends both the sweep's progress and the action confirm to a centered alert; ADR 0003 makes that alert the only thing on screen that moves. This decides what it is.

Settled against a throwaway prototype of four variants — branch `prototype/sweep-alert`, `cargo run --example alert_prototype`. Variant 4 won.

## The box

A **60-column, 6-row bordered box**, centered, drawn over a `Clear` so nothing shows through. Contents, top to bottom:

1. A headline: a braille spinner, then the message.
2. A progress bar, when there is progress to show.
3. A hint line, in muted text, naming the keys that work.

The border and title take the state's colour: **cyan** sweeping, **red** failed, **yellow** confirm — yellow being the colour the confirm already uses in the status row.

Behind it, every cell's foreground is overwritten with a muted colour. Overwriting rather than the `DIM` modifier, because terminals are free to ignore `DIM`. This flattens the list's own colours while the alert is up, which is acceptable for something the user cannot interact with, and it wants the semantic colour from #5 (theme module, NO_COLOR and light-terminal contrast) rather than a literal `DarkGray`. Under `NO_COLOR` there is no dimming at all — the box's border and its `Clear` carry the whole job, which is the reason the box is large.

## The three states share the frame

Same size, same position, every time. Only the colour, the text and the hint line change:

| State | Headline | Bar | Hint |
| --- | --- | --- | --- |
| Sweeping | `3,000 of 5,000 · 41 stacks` | yes | `q quit` |
| Failed | `stopped at 2,400 of 5,000` | no | `m retry · any key to continue` |
| Confirm | `trash 400 messages from DoorDash (12 in view)?` | no | `y yes · n no` |

A distinct shape for the confirm was rejected: it is two layouts to keep aligned for a distinction the colour and the hint line already make, and one slot in one place is the thing the user learns.

**The spinner stays even though the bar is present.** The bar carries progress; the spinner carries liveness on its own terms, and a bar that has not moved for two seconds is ambiguous in a way a stopped spinner is not.

## Consequences

- `draw_status`'s spinner branch and its `Mode::Confirm` branch both move out of the status row. The status row keeps the idle status text and the view-state keys.
- The alert is the only place a sweep can be observed, so anything the sweep needs to say has to fit three lines at 60 columns — including the failure message.
- The first sweep renders the alert over an empty stack pane. That is why the box is 60 columns rather than the 46 the smaller variants used: on an otherwise blank screen, a small box reads as a crash rather than as progress.
- Depends on #5 for the muted colour and for behaving under `NO_COLOR`.
