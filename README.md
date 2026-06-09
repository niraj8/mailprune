# mailprune

Chuck-style email triage TUI. Stacks your inbox by sender so you can trash, archive,
mark-read, or **unsubscribe** from hundreds of emails in a few keystrokes. Built for
fast inbox-zero over Gmail IMAP, multi-account.

```
 mailprune  personal  work
┌ stacks (42) · 873 msgs · by sender · sort read rate · 2 marked ─┐┌ DoorDash <no-reply@doordash.com> · unsub: one-click POST ┐
│▌ 214   0% U DoorDash (12 new)                                   ││ 2026-06-09 ● Your order is on the way                     │
│▌ 120   2% U Medium Daily Digest                                 ││ 2026-06-08   Craving something new?                       │
│   76  31% U LinkedIn                                            ││ 2026-06-07   Weekend deals near you                       │
│   31  94%   GitHub                                              ││ ...                                                       │
└─────────────────────────────────────────────────────────────────┘└───────────────────────────────────────────────────────────┘
 j/k · Enter expand · Space mark · a mark all · d trash · e archive · r read · u unsub · s group · o sort · / filter · Tab acct · q quit
```

## Install

```sh
make install   # cargo build --release && cp to ~/bin
```

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
| `Enter` / `Esc` | expand / collapse stack |
| `d` | trash entire stack (moves to Gmail Trash — recoverable 30 days) |
| `e` | archive stack (moves to All Mail) |
| `r` | mark stack read |
| `u` | unsubscribe — RFC 8058 one-click POST → mailto via SMTP → browser fallback; then offers to trash the stack |
| `Space` | mark stack for bulk action (auto-advances; `d`/`e`/`r`/`u` then apply to all marked) |
| `a` | mark all visible stacks (again to clear) |
| `s` | toggle grouping: sender ↔ sender+subject |
| `o` | toggle sort: count ↔ read rate (least-read first — your dead newsletters) |
| `/` | filter stacks by sender |
| `Tab` | next account |
| `R` | refresh |
| `g` / `G` | top / bottom |
| `q` | quit |

## The kill-loop

The fastest way to inbox zero:

1. `o` — sort by read rate. Stacks you never open float to the top.
2. `Space` down the list to mark the dead newsletters (auto-advances).
3. `u` — bulk unsubscribe everything marked, one confirm.
4. `y` again at the "also trash?" prompt.
5. `s` to regroup by sender+subject and repeat for noisy notification types
   from senders you otherwise keep.

## Notes

- Each stack shows a read-rate % (share of its messages you've opened), red when ≈0 — a 0% stack with 100 messages is a newsletter you should unsubscribe from. Based on messages currently in INBOX only.
- Delete is always move-to-Trash, never permanent — Gmail keeps trash 30 days. That's the undo story.
- Unsubscribe priority: `List-Unsubscribe-Post` one-click (silent HTTP POST) → `mailto:` (sends an email via SMTP with your app password) → opening the `https` link in your browser.
- Passwords live in the Keychain under service `mailprune`. Env override: `MAILPRUNE_PASSWORD_<EMAIL_WITH_UNDERSCORES>`.
