# mailprune

Email triage TUI. Stacks your inbox by sender so you can trash, archive,
mark-read, or **unsubscribe** from hundreds of emails in a few keystrokes. Built for
fast inbox-zero over Gmail IMAP, multi-account.

## The kill-loop

The fastest way to inbox zero:

1. `s` — sort by read rate. Stacks you never open float to the top.
2. `Space` down the list to mark the dead newsletters (auto-advances).
3. `u` — bulk unsubscribe everything marked, one confirm.
4. `y` again at the "also trash?" prompt.
5. `g` to regroup by sender+subject and repeat for noisy notification types
   from senders you otherwise keep.
6. `m` when the pane runs dry, and go again.

```
 mailprune  personal  work                          12 trashed · 40 archived · 3 unsubbed
┌ stacks (42) · 873 of 137,482 msgs · by sender · sort read rate · 2 marked ┐┌ DoorDash <no-reply@doordash.com> · unsub: one-click POST ┐
│▌ 214   0% U DoorDash (12 new)                                             ││ 2026-06-09 ● Your order is on the way                     │
│▌ 120   2% U Medium Daily Digest                                           ││ 2026-06-08   Craving something new?                       │
│   76  31% U LinkedIn                                                      ││ 2026-06-07   Weekend deals near you                       │
│   31  94%   GitHub                                                        ││ ...                                                       │
└───────────────────────────────────────────────────────────────────────────┘└───────────────────────────────────────────────────────────┘
 personal: 873 of 137,482 messages in 42 stacks                   m more  g group  s sort  Tab acct
 j/k move  Space mark  d trash  e archive  r read  u unsub  / filter  ? keys
```

The stacks on screen are the 40 most recent senders, not your whole mailbox —
press `m` for the next 40. But each **count is every message that sender has in
your inbox**, not just the ones listed, so `d` trashes all of them. A count
shown as `~214` means the server refused part of the listing: the sender has
more than that, mailprune only has 214, and `d` will clear only those. Press
`R` to reload.

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

## Notes

- **Nothing is ever deleted permanently.** `d` moves mail to Gmail Trash, which
  Gmail keeps for 30 days. That's the undo story.
- The read-rate % is the share of a stack's messages you've opened, red when
  ≈0. A 0% stack with 100 messages is a newsletter you should unsubscribe from.
- `u` tries RFC 8058 one-click POST first, then `mailto:` (sends a real email
  via SMTP from your account), then opens the `https` link in your browser.
- mailprune writes a local log of your triage decisions — header metadata only,
  never message bodies — to `~/.local/state/mailprune/actions.jsonl`, for
  future "suggest stacks to act on" features. Disable with `action_log = false`
  in config.toml. Network activity is logged to `debug.log` alongside it;
  disable with `MAILPRUNE_NO_DEBUG_LOG=1`.
