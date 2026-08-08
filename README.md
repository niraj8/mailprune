# mailprune

Email triage TUI. Stacks your inbox by sender so you can trash, archive,
mark-read, or **unsubscribe** from hundreds of emails in a few keystrokes. Built for
fast inbox-zero over Gmail IMAP, multi-account.

```
 mailprune  personal  work                          12 trashed · 40 archived · 3 unsubbed
┌ stacks (42) · 873 of 137,482 msgs · by sender · sort read rate · 2 marked ┐┌ DoorDash <no-reply@doordash.com> · unsub: one-click POST ┐
│▌ 214   0% U DoorDash (12 new)                                             ││ 2026-06-09 ● Your order is on the way                     │
│▌ 120   2% U Medium Daily Digest                                           ││ 2026-06-08   Craving something new?                       │
│   76  31% U LinkedIn                                                      ││ 2026-06-07   Weekend deals near you                       │
│   31  94%   GitHub                                                        ││ ...                                                       │
└───────────────────────────────────────────────────────────────────────────┘└───────────────────────────────────────────────────────────┘
 personal: 873 of 137,482 messages in 42 stacks                   m more  s group  o sort  Tab acct
 j/k move  ↵ open  Space mark  d trash  e archive  u unsub  / filter  ? keys
```

Needs a terminal of at least 80x24; below that mailprune asks you to resize rather than drawing a squeezed layout.

## How loading works

mailprune never downloads your whole mailbox. On start it takes one cheap list
of message IDs (a few hundred KB even at 100k messages), reads the newest
headers until it has found **40 senders**, then asks the server for every
message each of those senders has in your inbox. Stacks appear as each sender
resolves, usually within a second.

So the *set* of stacks on screen is a recent-senders window — but every **count
is the sender's true inbox-wide total**, and `d` really does trash all of it,
including messages that were never on screen. The pane title shows both numbers
(`873 of 137,482`), and both fall as you triage.

Press `m` for the next 40 senders. Loading is always explicit: the pane drains
as you clear it and refills only when you ask. A count prefixed with `~` means
the server refused part of that sender's listing, so the count is short and
trashing the stack will under-clear — press `R` to reload.

## Install

macOS and Linux (prebuilt binaries via Homebrew):

```sh
brew install niraj8/tap/mailprune
```

Or grab a tarball from [releases](https://github.com/niraj8/mailprune/releases) (macOS arm64/x86_64, Linux x86_64), or build from source:

```sh
make install   # cargo build --release && cp to ~/bin
```

Passwords are stored in the platform keychain: macOS Keychain, Secret Service
(GNOME Keyring/KWallet) on Linux, Credential Manager on Windows (compiles, untested).
No keyring daemon? Use the `MAILPRUNE_PASSWORD_<EMAIL_WITH_UNDERSCORES>` env var instead.

## Setup

1. **Enable 2FA** on each Google account, then generate an app password at
   <https://myaccount.google.com/apppasswords> (also requires IMAP enabled in
   Gmail Settings → Forwarding and POP/IMAP).

2. **Config** — create `~/.config/mailprune/config.toml`:

   ```toml
   [[accounts]]
   name = "personal"
   email = "you@gmail.com"

   [[accounts]]
   name = "work"
   email = "you@other.com"
   ```

3. **Store app passwords** (saved in the macOS Keychain):

   ```sh
   mailprune auth you@gmail.com
   ```

4. **Run**: `mailprune` (TUI) or `mailprune stacks` (headless dump of all accounts).

## Keys

| key | action |
| --- | --- |
| `j` / `k` | move selection |
| `Enter` | expand / collapse stack |
| `Esc` | collapse the stack, else clear marks, else clear the filter |
| `d` | trash entire stack (moves to Gmail Trash — recoverable 30 days) |
| `e` | archive stack (moves to All Mail) |
| `r` | mark stack read |
| `u` | unsubscribe — RFC 8058 one-click POST → mailto via SMTP → browser fallback; then offers to trash the stack |
| `Space` | mark stack for bulk action (auto-advances; `d`/`e`/`r`/`u` then apply to all marked) |
| `a` | mark all visible stacks (again to clear) |
| `m` | load 40 more senders, appended to the end |
| `s` | toggle grouping: sender (default) ↔ sender+subject |
| `o` | toggle sort: count (default) ↔ read rate (least-read first — your dead newsletters), re-sorting everything loaded |
| `/` | filter stacks by sender |
| `Tab` | next account |
| `R` | reload from scratch — new message list, stacks cleared |
| `g` / `G` | top / bottom |
| `?` | full key overlay (any key closes) |
| `q` | quit |

## The kill-loop

The fastest way to inbox zero:

1. `o` — sort by read rate. Stacks you never open float to the top.
2. `Space` down the list to mark the dead newsletters (auto-advances).
3. `u` — bulk unsubscribe everything marked, one confirm.
4. `y` again at the "also trash?" prompt.
5. `s` to regroup by sender+subject and repeat for noisy notification types
   from senders you otherwise keep.
6. `m` when the pane runs dry, and go again.

## Notes

- A stack is a set of messages, not a Gmail conversation. Under `s`
  (sender+subject) the key normalizes `Re:`/`Fwd:` and digit runs, so
  `Order #123 shipped` and `Order #456 shipped` land together — that is the
  point for newsletters. Either way `d` trashes only the *inbox* copies: if you
  ever replied, your Sent copy keeps the Gmail conversation alive. Newsletters
  have no replies, so this never bites in the case mailprune is built for.
- Inside an expanded stack, dates within the last 30 days are bold — recent mail separates from the backlog at a glance.
- Each stack shows a read-rate % (share of its messages you've opened), red when ≈0 — a 0% stack with 100 messages is a newsletter you should unsubscribe from. Based on messages currently in INBOX only.
- Delete is always move-to-Trash, never permanent — Gmail keeps trash 30 days. That's the undo story.
- Unsubscribe priority: `List-Unsubscribe-Post` one-click (silent HTTP POST) → `mailto:` (sends an email via SMTP with your app password) → opening the `https` link in your browser.
- Passwords live in the Keychain under service `mailprune`. Env override: `MAILPRUNE_PASSWORD_<EMAIL_WITH_UNDERSCORES>`.
- mailprune logs your triage decisions (header metadata only — sender, subject, counts; never message bodies) to `~/.local/state/mailprune/actions.jsonl`. The log stays on your machine and will power future "suggest stacks to act on" features. Disable with `action_log = false` in config.toml; delete the file anytime.
- Network operations also leave start/done lines in `~/.local/state/mailprune/debug.log` (rotated at 2 MB) — if the TUI ever hangs, the last line names the operation that never returned. Disable with `MAILPRUNE_NO_DEBUG_LOG=1`.
