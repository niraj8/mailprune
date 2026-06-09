# mailstack

Chuck-style email triage TUI. Stacks your inbox by sender so you can trash, archive,
mark-read, or **unsubscribe** from hundreds of emails in a few keystrokes. Built for
fast inbox-zero over Gmail IMAP, multi-account.

```
 mailstack  personal  work
┌ stacks (42) · 873 msgs ──────────┐┌ DoorDash <no-reply@doordash.com> · unsub: one-click POST ┐
│  214 U DoorDash (12 new)         ││ 2026-06-09 ● Your order is on the way                     │
│  120 U Medium Daily Digest       ││ 2026-06-08   Craving something new?                       │
│   76 U LinkedIn                  ││ 2026-06-07   Weekend deals near you                       │
│   31   GitHub                    ││ ...                                                       │
└──────────────────────────────────┘└───────────────────────────────────────────────────────────┘
 j/k move · Enter expand · d trash · e archive · r read · u unsub · / filter · Tab account · R refresh · q quit
```

## Setup

1. **Enable 2FA** on each Google account, then generate an app password at
   <https://myaccount.google.com/apppasswords> (also requires IMAP enabled in
   Gmail Settings → Forwarding and POP/IMAP).

2. **Config** — create `~/.config/mailstack/config.toml`:

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
   mailstack auth you@gmail.com
   ```

4. **Run**: `mailstack` (TUI) or `mailstack stacks` (headless dump of all accounts).

## Keys

| key | action |
| --- | --- |
| `j` / `k` | move selection |
| `Enter` / `Esc` | expand / collapse stack |
| `d` | trash entire stack (moves to Gmail Trash — recoverable 30 days) |
| `e` | archive stack (moves to All Mail) |
| `r` | mark stack read |
| `u` | unsubscribe — RFC 8058 one-click POST → mailto via SMTP → browser fallback; then offers to trash the stack |
| `/` | filter stacks by sender |
| `Tab` | next account |
| `R` | refresh |
| `g` / `G` | top / bottom |
| `q` | quit |

## Notes

- Delete is always move-to-Trash, never permanent — Gmail keeps trash 30 days. That's the undo story.
- Unsubscribe priority: `List-Unsubscribe-Post` one-click (silent HTTP POST) → `mailto:` (sends an email via SMTP with your app password) → opening the `https` link in your browser.
- Passwords live in the Keychain under service `mailstack`. Env override: `MAILSTACK_PASSWORD_<EMAIL_WITH_UNDERSCORES>`.
