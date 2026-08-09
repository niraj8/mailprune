use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use crate::config::AccountConfig;
use crate::stacks::MsgMeta;

type Session = async_imap::Session<TlsStream<TcpStream>>;

const OP_TIMEOUT_SECS: u64 = 30;
/// a search over a six-figure mailbox is server-side but not instant
const SEARCH_TIMEOUT_SECS: u64 = 60;
/// one chunk of headers, never the whole mailbox — see `FETCH_CHUNK`
const FETCH_TIMEOUT_SECS: u64 = 60;

/// Messages one sweep reads before it stops — five FETCHes at `FETCH_CHUNK`.
/// It bounds the wait the TUI cannot escape, not the data: 5,000 headers is
/// about a megabyte. `m` sweeps the next 5,000 (ADR 0002).
pub const WINDOW: usize = 5000;
/// messages per FETCH. Splitting keeps each command's timeout bounded and lets
/// a sweep that dies part-way keep the chunks that landed.
const FETCH_CHUNK: usize = 1000;

/// only the fields `MsgMeta` actually parses — ~200 bytes a message instead of
/// the 2–6 KB a full `RFC822.HEADER` costs. `.PEEK` leaves `\Seen` alone.
const HEADER_QUERY: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST)])";

/// A timeout, as distinct from a server `NO`/`BAD`. The two mean different
/// things to a caller: a refusal arrives on a live connection and only affects
/// the one command, while after a timeout the session state is unknown.
#[derive(Debug)]
pub struct TimedOut {
    what: String,
    secs: u64,
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} timed out after {}s — connection presumed dead, press R to reconnect",
            self.what, self.secs
        )
    }
}

impl std::error::Error for TimedOut {}

/// did this error come from `timed()` giving up, rather than the server
/// answering? Callers must drop the client when it did.
pub fn is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<TimedOut>())
}

/// Gmail drops idle IMAP connections without closing the TCP socket; an
/// un-timeouted await on such a session blocks forever (frozen TUI). After a
/// timeout the session state is unknown — callers must drop the client.
async fn timed<T>(
    secs: u64,
    what: &str,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
        Ok(r) => r,
        Err(_) => {
            crate::debuglog::write(format!("{what} TIMED OUT after {secs}s"));
            Err(anyhow!(TimedOut {
                what: what.to_string(),
                secs,
            }))
        }
    }
}

pub struct ImapClient {
    session: Session,
    pub trash_folder: String,
    pub archive_folder: String,
}

async fn tls_connect(host: &str) -> Result<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect((host, 993))
        .await
        .with_context(|| format!("connecting to {host}:993"))?;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string())?;
    Ok(connector.connect(server_name, tcp).await?)
}

impl ImapClient {
    pub async fn connect(account: &AccountConfig, password: &str) -> Result<Self> {
        crate::debuglog::write(format!(
            "imap connect start {} ({})",
            account.email, account.imap_host
        ));
        let client = timed(
            OP_TIMEOUT_SECS,
            "imap connect",
            Self::connect_inner(account, password),
        )
        .await?;
        crate::debuglog::write(format!("imap connect done {}", account.email));
        Ok(client)
    }

    async fn connect_inner(account: &AccountConfig, password: &str) -> Result<Self> {
        let tls = tls_connect(&account.imap_host).await?;
        let client = async_imap::Client::new(tls);
        let mut session = client
            .login(&account.email, password)
            .await
            .map_err(|(e, _)| anyhow!("IMAP login failed for {}: {e}", account.email))?;

        // Resolve special-use folders (RFC 6154) so localized Gmail names work.
        let mut trash_folder = String::from("[Gmail]/Trash");
        let mut archive_folder = String::from("[Gmail]/All Mail");
        {
            let mut names = session.list(Some(""), Some("*")).await?;
            while let Some(name) = names.next().await {
                let name = name?;
                let attrs = format!("{:?}", name.attributes());
                if attrs.contains("Trash") {
                    trash_folder = name.name().to_string();
                } else if attrs.contains("All") {
                    archive_folder = name.name().to_string();
                }
            }
        }

        // Every other command here is UID-scoped and assumes INBOX is already
        // selected. Only a reset load re-selects, so a session that reconnects
        // mid-session — after a timeout, say — would otherwise issue its first
        // fetch against no mailbox at all.
        session.select("INBOX").await?;

        Ok(Self {
            session,
            trash_folder,
            archive_folder,
        })
    }

    pub async fn trash(&mut self, uids: &[u32]) -> Result<()> {
        crate::debuglog::write(format!("imap trash start {} uids", uids.len()));
        let folder = self.trash_folder.clone();
        let session = &mut self.session;
        timed(OP_TIMEOUT_SECS, "imap trash", async move {
            session.uid_mv(uid_set(uids), &folder).await?;
            Ok(())
        })
        .await?;
        crate::debuglog::write("imap trash done");
        Ok(())
    }

    pub async fn archive(&mut self, uids: &[u32]) -> Result<()> {
        crate::debuglog::write(format!("imap archive start {} uids", uids.len()));
        let folder = self.archive_folder.clone();
        let session = &mut self.session;
        timed(OP_TIMEOUT_SECS, "imap archive", async move {
            session.uid_mv(uid_set(uids), &folder).await?;
            Ok(())
        })
        .await?;
        crate::debuglog::write("imap archive done");
        Ok(())
    }

    pub async fn mark_read(&mut self, uids: &[u32]) -> Result<()> {
        crate::debuglog::write(format!("imap mark_read start {} uids", uids.len()));
        let session = &mut self.session;
        timed(OP_TIMEOUT_SECS, "imap mark_read", async move {
            let mut stream = session.uid_store(uid_set(uids), "+FLAGS (\\Seen)").await?;
            while let Some(item) = stream.next().await {
                item?;
            }
            Ok(())
        })
        .await?;
        crate::debuglog::write("imap mark_read done");
        Ok(())
    }

    pub async fn logout(mut self) {
        let _ = self.session.logout().await;
    }
}

/// The operations the sweep and the fan-out need from a mailbox. Everything
/// above this line talks to a socket; everything below it is decisions about
/// what to ask for next, and this is the line between them.
///
/// `exists` is the mailbox size, which is also the newest message's sequence
/// number. `fetch_seq` takes an inclusive sequence range — one command's worth,
/// the caller chunks — and `fetch` the same by UID. Either can come back with
/// fewer messages than were asked for, because mail expunged since the count
/// was taken simply returns nothing. `search` returns UIDs newest-first.
pub trait Mailbox {
    fn exists(&mut self) -> impl Future<Output = Result<u32>>;
    fn fetch_seq(&mut self, lo: u32, hi: u32) -> impl Future<Output = Result<Vec<MsgMeta>>>;
    fn search(&mut self, query: &str) -> impl Future<Output = Result<Vec<u32>>>;
    fn fetch(&mut self, uids: &[u32]) -> impl Future<Output = Result<Vec<MsgMeta>>>;
}

impl Mailbox for ImapClient {
    /// Re-`SELECT`s rather than trusting a count from earlier in the session:
    /// the sweep anchors its window to the top of the mailbox, and every
    /// arrival and every `UID MOVE` this session has already moved it.
    async fn exists(&mut self) -> Result<u32> {
        let session = &mut self.session;
        let exists = timed(OP_TIMEOUT_SECS, "imap select", async move {
            Ok(session.select("INBOX").await?.exists)
        })
        .await?;
        crate::debuglog::write(format!("imap exists {exists}"));
        Ok(exists)
    }

    async fn fetch_seq(&mut self, lo: u32, hi: u32) -> Result<Vec<MsgMeta>> {
        let session = &mut self.session;
        timed(FETCH_TIMEOUT_SECS, "imap header fetch", async move {
            let stream = session.fetch(format!("{lo}:{hi}"), HEADER_QUERY).await?;
            collect_metas(stream, (hi - lo + 1) as usize).await
        })
        .await
    }

    async fn search(&mut self, query: &str) -> Result<Vec<u32>> {
        let session = &mut self.session;
        let query = query.to_string();
        timed(SEARCH_TIMEOUT_SECS, "imap search", async move {
            Ok(newest_first(session.uid_search(query).await?))
        })
        .await
    }

    async fn fetch(&mut self, uids: &[u32]) -> Result<Vec<MsgMeta>> {
        let session = &mut self.session;
        let set = uid_set(uids);
        timed(FETCH_TIMEOUT_SECS, "imap header fetch", async move {
            let stream = session.uid_fetch(set, HEADER_QUERY).await?;
            collect_metas(stream, uids.len()).await
        })
        .await
    }
}

/// drain a fetch response stream into the headers it carried
async fn collect_metas(
    stream: impl futures::Stream<Item = async_imap::error::Result<async_imap::types::Fetch>>,
    cap: usize,
) -> Result<Vec<MsgMeta>> {
    let mut out = Vec::with_capacity(cap);
    let mut stream = std::pin::pin!(stream);
    while let Some(fetch) = stream.next().await {
        let fetch = fetch?;
        if let Some(msg) = to_meta(&fetch) {
            out.push(msg);
        }
    }
    Ok(out)
}

/// What one sweep read.
#[derive(Debug, Default)]
pub struct Sweep {
    /// the headers that landed, newest chunk first. Windows can overlap when
    /// mail arrived between two sweeps, so the store dedupes by UID.
    pub msgs: Vec<MsgMeta>,
    /// mailbox size, from this sweep's own `EXISTS`
    pub total: usize,
    /// the sweep got that `EXISTS`. False only when the mailbox never answered
    /// — a failed connect knows nothing, and must not report an empty mailbox
    /// as if it had counted one.
    pub anchored: bool,
    /// what this sweep set out to read: `WINDOW`, or the rest of the mailbox
    /// when that is nearer
    pub bound: usize,
    /// what it actually read. Below `bound` is a short window
    pub swept: usize,
    /// the window now reaches the oldest message in the mailbox
    pub reached_end: bool,
}

impl Sweep {
    /// the sweep stopped before its bound, so the window has a hole at its
    /// oldest edge. `m` retries the remainder before advancing (ADR 0003).
    pub fn short(&self) -> bool {
        self.swept < self.bound
    }
}

/// one chunk landed, for the alert to render
#[derive(Debug, Clone, Copy)]
pub struct SweepProgress {
    /// messages swept so far, out of `bound`
    pub swept: usize,
    pub bound: usize,
    /// distinct senders seen so far. The sweep does not know how the user is
    /// grouping, and this is the stack count under the default grouping.
    pub stacks: usize,
}

/// Read the newest `WINDOW` messages the window does not already cover.
///
/// `back` is how many of the newest messages earlier sweeps already reached.
/// The window is tracked as that distance back from the top and re-anchored off
/// a fresh `EXISTS` every sweep: `EXISTS` is the newest message's sequence
/// number, so the newest 5,000 are a sequence range and no `SEARCH` is needed
/// at any mailbox size. Remembering the lowest UID swept would be stabler, but
/// finding its sequence number costs the `UID SEARCH` this path exists to
/// delete (ADR 0003). Sequence numbers move under arrivals and expunges, so a
/// window can slip past messages that arrived mid-session; a repeat is free,
/// because the store dedupes by UID.
///
/// Returns what landed *and* the outcome, rather than a `Result` carrying one
/// or the other: the chunks before a refusal are real messages, and the caller
/// needs the swept count to say how far the window actually reaches. A timeout
/// stops the sweep the same way, but leaves the session state unknown — the
/// caller must drop the client, which is what `is_timeout` is for.
pub async fn sweep<M: Mailbox>(
    mailbox: &mut M,
    back: usize,
    mut on_progress: impl FnMut(SweepProgress),
) -> (Sweep, Result<()>) {
    let total = match mailbox.exists().await {
        Ok(n) => n as usize,
        Err(e) => return (Sweep::default(), Err(e)),
    };
    // the newest message no sweep has reached yet
    let hi = total.saturating_sub(back);
    let bound = WINDOW.min(hi);
    let mut window = Sweep {
        total,
        anchored: true,
        bound,
        reached_end: hi == 0,
        ..Sweep::default()
    };
    if bound == 0 {
        return (window, Ok(()));
    }
    let lo = hi - bound + 1;

    let mut senders: HashSet<String> = HashSet::new();
    let mut result = Ok(());
    let mut top = hi;
    loop {
        let bottom = (top + 1).saturating_sub(FETCH_CHUNK).max(lo);
        match mailbox.fetch_seq(bottom as u32, top as u32).await {
            Ok(part) => {
                // The bound counts the sequence numbers asked for, not the
                // headers that came back. A message expunged since the
                // `EXISTS` returns nothing, and letting that extend the window
                // is exactly the unbounded wait the bound exists to stop.
                window.swept += top - bottom + 1;
                for m in &part {
                    senders.insert(m.sender_email.to_lowercase());
                }
                window.msgs.extend(part);
                on_progress(SweepProgress {
                    swept: window.swept,
                    bound,
                    stacks: senders.len(),
                });
            }
            Err(e) => {
                crate::debuglog::write(format!("imap sweep stopped at {bottom}:{top}: {e:#}"));
                result = Err(e);
                break;
            }
        }
        if bottom == lo {
            break;
        }
        top = bottom - 1;
    }
    window.reached_end = window.swept == hi;
    crate::debuglog::write(format!(
        "imap sweep {lo}:{hi} swept {} of {bound}, {} msgs",
        window.swept,
        window.msgs.len()
    ));
    (window, result)
}

/// Every message from `addr` still in the mailbox, and whether that is all of
/// them. `FROM` is a substring match server-side, so `a@b.com` also matches
/// `xa@b.com` — the results are filtered back to the exact address.
///
/// A refused chunk stops the walk but keeps the chunks that did arrive: they
/// are real messages, and falling all the way back to the discovery sample
/// would under-report further than the failure requires.
// nothing calls fan-out between the load path going and #30 wiring it to the
// action path, where it loses its FETCH half and becomes the SEARCH alone
#[allow(dead_code)]
async fn fan_out<M: Mailbox>(mailbox: &mut M, addr: &str) -> Result<(Vec<MsgMeta>, bool)> {
    let uids = mailbox.search(&format!("FROM {}", quoted(addr))).await?;
    let mut msgs = Vec::with_capacity(uids.len());
    let mut complete = true;
    for chunk in uids.chunks(FETCH_CHUNK) {
        match mailbox.fetch(chunk).await {
            Ok(part) => msgs.extend(part),
            Err(e) if is_timeout(&e) => return Err(e),
            Err(e) => {
                crate::debuglog::write(format!("imap fetch refused for {addr}: {e:#}"));
                complete = false;
                break;
            }
        }
    }
    msgs.retain(|m| m.sender_email.eq_ignore_ascii_case(addr));
    Ok((msgs, complete))
}

/// a UID search result as a newest-first list
fn newest_first(found: HashSet<u32>) -> Vec<u32> {
    let mut uids: Vec<u32> = found.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    uids
}

/// one fetch response as a `MsgMeta`, or None if it carried no usable header
fn to_meta(fetch: &async_imap::types::Fetch) -> Option<MsgMeta> {
    use mailparse::MailHeaderMap;
    let uid = fetch.uid?;
    let unread = !fetch
        .flags()
        .any(|f| matches!(f, async_imap::types::Flag::Seen));
    let date = fetch.internal_date().map(|d| d.with_timezone(&Utc));
    let (headers, _) = mailparse::parse_headers(fetch.header()?).ok()?;
    let from_raw = headers.get_first_value("From").unwrap_or_default();
    let (sender_name, sender_email) = parse_from(&from_raw);
    Some(MsgMeta {
        uid,
        sender_email,
        sender_name,
        subject: headers.get_first_value("Subject").unwrap_or_default(),
        date,
        unread,
        list_unsubscribe: headers.get_first_value("List-Unsubscribe"),
        one_click: headers
            .get_first_value("List-Unsubscribe-Post")
            .map(|v| v.to_lowercase().contains("one-click"))
            .unwrap_or(false),
    })
}

/// an IMAP quoted string, so an address can never end the argument early
fn quoted(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// "John Doe <a@b.com>" -> ("John Doe", "a@b.com"); RFC 2047 already decoded by mailparse
fn parse_from(raw: &str) -> (String, String) {
    if let Ok(list) = mailparse::addrparse(raw) {
        for addr in list.iter() {
            match addr {
                mailparse::MailAddr::Single(s) => {
                    return (s.display_name.clone().unwrap_or_default(), s.addr.clone());
                }
                mailparse::MailAddr::Group(g) => {
                    if let Some(s) = g.addrs.first() {
                        return (s.display_name.clone().unwrap_or_default(), s.addr.clone());
                    }
                }
            }
        }
    }
    (String::new(), raw.trim().to_string())
}

/// compress sorted uids into IMAP set syntax: 1,2,3,7 -> "1:3,7"
pub fn uid_set(uids: &[u32]) -> String {
    let mut uids = uids.to_vec();
    uids.sort_unstable();
    uids.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < uids.len() {
        let start = uids[i];
        let mut end = start;
        while i + 1 < uids.len() && uids[i + 1] == end + 1 {
            i += 1;
            end = uids[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        i += 1;
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn uid_set_compresses_ranges() {
        assert_eq!(uid_set(&[3, 1, 2, 7, 9, 8]), "1:3,7:9");
        assert_eq!(uid_set(&[5]), "5");
        assert_eq!(uid_set(&[2, 2, 4]), "2,4");
    }

    #[test]
    fn addresses_are_quoted_so_they_cannot_escape_the_search_argument() {
        assert_eq!(quoted("a@b.com"), "\"a@b.com\"");
        assert_eq!(quoted("we\"ird\\@b.com"), "\"we\\\"ird\\\\@b.com\"");
    }

    fn meta(uid: u32, sender: &str) -> MsgMeta {
        MsgMeta {
            uid,
            sender_email: sender.into(),
            sender_name: String::new(),
            subject: "hi".into(),
            date: None,
            unread: true,
            list_unsubscribe: None,
            one_click: false,
        }
    }

    /// what the scripted mailbox does when an operation is reached
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Answer {
        Ok,
        /// a server `NO`/`BAD`: a clean answer on a live connection
        Refuse,
        Timeout,
    }

    /// A mailbox with no socket. Models the three behaviours the sweep and the
    /// fan-out actually depend on: sequence numbers are 1-based and oldest
    /// first, `FROM` matches as a substring, and a message that has left the
    /// mailbox simply returns nothing.
    struct FakeMailbox {
        /// oldest first, so `msgs[i]` is sequence number `i + 1`
        msgs: Vec<MsgMeta>,
        /// how each fetch is answered, in order; missing entries are `Ok`
        fetches: Vec<Answer>,
        fetch_calls: usize,
        /// the sequence ranges asked for, in order
        ranges: Vec<(u32, u32)>,
        /// keyed by the address inside `FROM "…"`
        searches: HashMap<String, Answer>,
        searched: bool,
        /// UIDs the server answers nothing for: expunged since the `EXISTS`,
        /// or a header the parser could not use. Their sequence numbers are
        /// still asked for, which is what the bound counts.
        silent: HashSet<u32>,
    }

    impl FakeMailbox {
        fn new(msgs: Vec<MsgMeta>) -> Self {
            Self {
                msgs,
                fetches: Vec::new(),
                fetch_calls: 0,
                ranges: Vec::new(),
                searches: HashMap::new(),
                searched: false,
                silent: HashSet::new(),
            }
        }

        /// `n` messages, one per sender, oldest first — uid `i` at sequence `i`
        fn of_size(n: u32) -> Self {
            Self::new(
                (1..=n)
                    .map(|uid| meta(uid, &format!("s{uid}@x.com")))
                    .collect(),
            )
        }

        /// how each fetch is answered, in order
        fn fetches_answer(mut self, answers: &[Answer]) -> Self {
            self.fetches = answers.to_vec();
            self
        }

        fn search_answers(mut self, addr: &str, answer: Answer) -> Self {
            self.searches.insert(addr.into(), answer);
            self
        }

        /// the fetch for these uids comes back with nothing
        fn silent_uids(mut self, uids: impl IntoIterator<Item = u32>) -> Self {
            self.silent = uids.into_iter().collect();
            self
        }

        fn answer(a: Answer, what: &str) -> Result<()> {
            match a {
                Answer::Ok => Ok(()),
                Answer::Refuse => Err(anyhow!("NO [CANNOT] {what} not supported")),
                Answer::Timeout => Err(anyhow!(TimedOut {
                    what: what.into(),
                    secs: 60,
                })),
            }
        }

        fn next_fetch_answer(&mut self) -> Result<()> {
            let answer = self
                .fetches
                .get(self.fetch_calls)
                .copied()
                .unwrap_or(Answer::Ok);
            self.fetch_calls += 1;
            Self::answer(answer, "fetch")
        }
    }

    impl Mailbox for FakeMailbox {
        async fn exists(&mut self) -> Result<u32> {
            Ok(self.msgs.len() as u32)
        }

        async fn fetch_seq(&mut self, lo: u32, hi: u32) -> Result<Vec<MsgMeta>> {
            self.ranges.push((lo, hi));
            self.next_fetch_answer()?;
            Ok(self.msgs[(lo - 1) as usize..hi as usize]
                .iter()
                .filter(|m| !self.silent.contains(&m.uid))
                .cloned()
                .collect())
        }

        async fn search(&mut self, query: &str) -> Result<Vec<u32>> {
            let addr = query
                .trim_start_matches("FROM ")
                .trim_matches('"')
                .to_string();
            self.searched = true;
            Self::answer(
                self.searches.get(&addr).copied().unwrap_or(Answer::Ok),
                "search",
            )?;
            Ok(newest_first(
                self.msgs
                    .iter()
                    // IMAP FROM is a substring match, not an equality test
                    .filter(|m| m.sender_email.contains(&addr))
                    .map(|m| m.uid)
                    .collect(),
            ))
        }

        async fn fetch(&mut self, uids: &[u32]) -> Result<Vec<MsgMeta>> {
            self.next_fetch_answer()?;
            Ok(self
                .msgs
                .iter()
                .filter(|m| uids.contains(&m.uid) && !self.silent.contains(&m.uid))
                .cloned()
                .collect())
        }
    }

    /// one sweep, with the progress it reported along the way
    async fn swept(
        mailbox: &mut FakeMailbox,
        back: usize,
    ) -> (Sweep, Result<()>, Vec<SweepProgress>) {
        let mut progress = Vec::new();
        let (sweep, result) = sweep(mailbox, back, |p| progress.push(p)).await;
        (sweep, result, progress)
    }

    /// Spec: "no `uid_search("ALL")` — that search is the last operation whose
    /// cost grows with the mailbox", and "a 5,000-UID window is 5 FETCHes".
    #[tokio::test]
    async fn a_full_window_is_five_fetches_and_no_search() {
        let mut mailbox = FakeMailbox::of_size(137_482);
        let (sweep, result, _) = swept(&mut mailbox, 0).await;

        assert!(result.is_ok());
        assert!(!mailbox.searched, "a sweep never searches, at any size");
        assert_eq!(
            mailbox.ranges,
            [
                (136_483, 137_482),
                (135_483, 136_482),
                (134_483, 135_482),
                (133_483, 134_482),
                (132_483, 133_482),
            ],
            "five contiguous chunks, newest first"
        );
        assert_eq!(sweep.swept, WINDOW);
        assert_eq!(sweep.msgs.len(), WINDOW);
        assert_eq!(sweep.total, 137_482, "the title's denominator is EXISTS");
        assert!(!sweep.short());
        assert!(!sweep.reached_end);
    }

    /// The window is a distance back from the top, re-anchored off a fresh
    /// `EXISTS`: `m` reads the 5,000 behind the first window, and mail that
    /// arrived in between shifts what that means rather than being skipped.
    #[tokio::test]
    async fn m_reads_the_window_behind_the_one_already_swept() {
        let mut mailbox = FakeMailbox::of_size(20_000);
        let (_, result, _) = swept(&mut mailbox, WINDOW).await;
        assert!(result.is_ok());
        assert_eq!(mailbox.ranges[0], (14_001, 15_000));
        assert_eq!(mailbox.ranges.last(), Some(&(10_001, 11_000)));
    }

    /// A refusal on chunk 3 of 5 leaves the window 2,600 short at its oldest
    /// edge. `m` carries the count the sweep actually reached, so the next
    /// sweep starts at that edge and fills the hole before it advances —
    /// skipping it would leave a gap mid-window that nothing in the UI could
    /// explain (ADR 0003).
    #[tokio::test]
    async fn m_after_a_short_window_reads_the_remainder_before_it_advances() {
        let mut mailbox = FakeMailbox::of_size(20_000)
            .fetches_answer(&[Answer::Ok, Answer::Ok, Answer::Refuse]);
        let (short, result, _) = swept(&mut mailbox, 0).await;
        assert!(result.is_err());
        assert_eq!(short.swept, 2_000, "the window stopped two chunks in");

        let mut mailbox = FakeMailbox::of_size(20_000);
        let (_, result, _) = swept(&mut mailbox, short.swept).await;

        assert!(result.is_ok());
        assert_eq!(
            mailbox.ranges[0],
            (17_001, 18_000),
            "the retry starts where the refusal stopped, not past the hole"
        );
    }

    #[tokio::test]
    async fn a_sweep_re_anchors_to_the_top_so_arrivals_do_not_shift_it_off() {
        let mut mailbox = FakeMailbox::of_size(20_000);
        // 10 messages arrive between the two sweeps
        mailbox
            .msgs
            .extend((20_001..=20_010).map(|uid| meta(uid, "new@x.com")));
        let (sweep, result, _) = swept(&mut mailbox, WINDOW).await;

        assert!(result.is_ok());
        assert_eq!(sweep.total, 20_010);
        assert_eq!(
            mailbox.ranges[0],
            (14_011, 15_010),
            "the window is measured from the new top, not the old one"
        );
    }

    /// The bound is a count of UIDs, not of headers returned: a mailbox full of
    /// dead UIDs must not make a sweep run long to fill its quota.
    #[tokio::test]
    async fn headers_that_never_arrive_end_the_sweep_at_the_bound_anyway() {
        // four of the window's five chunks answer with nothing at all
        let mut mailbox = FakeMailbox::of_size(20_000).silent_uids(15_001..=19_000);
        let (sweep, result, _) = swept(&mut mailbox, 0).await;

        assert!(result.is_ok());
        assert_eq!(mailbox.ranges.len(), 5, "still five fetches, not more");
        assert_eq!(sweep.swept, WINDOW, "uids swept, not headers returned");
        assert_eq!(sweep.msgs.len(), 1_000, "only the live ones came back");
        assert!(!sweep.short(), "a dead uid is swept, not missed");
    }

    #[tokio::test]
    async fn a_mailbox_smaller_than_the_window_is_swept_to_its_oldest_message() {
        let mut mailbox = FakeMailbox::of_size(2_500);
        let (sweep, result, _) = swept(&mut mailbox, 0).await;

        assert!(result.is_ok());
        assert_eq!(mailbox.ranges, [(1_501, 2_500), (501, 1_500), (1, 500)]);
        assert_eq!(sweep.bound, 2_500, "the bound is the rest of the mailbox");
        assert_eq!(sweep.swept, 2_500);
        assert!(sweep.reached_end, "the title says `all`, not `newest`");
        assert!(!sweep.short());
    }

    #[tokio::test]
    async fn m_past_the_oldest_message_sweeps_nothing_and_asks_for_nothing() {
        let mut mailbox = FakeMailbox::of_size(2_500);
        let (sweep, result, progress) = swept(&mut mailbox, 2_500).await;

        assert!(result.is_ok());
        assert!(mailbox.ranges.is_empty(), "no fetch for an empty window");
        assert!(progress.is_empty());
        assert_eq!(sweep.swept, 0);
        assert!(sweep.reached_end);
        assert!(!sweep.short(), "reaching the end is not a short window");
    }

    #[tokio::test]
    async fn an_empty_mailbox_sweeps_nothing() {
        let mut mailbox = FakeMailbox::of_size(0);
        let (sweep, result, _) = swept(&mut mailbox, 0).await;

        assert!(result.is_ok());
        assert!(mailbox.ranges.is_empty());
        assert_eq!(sweep.total, 0);
        assert!(sweep.reached_end);
    }

    /// Spec: "on a server `NO`/`BAD` mid-window: stops, keeps the chunks that
    /// landed, and reports a short window with the count actually swept."
    #[tokio::test]
    async fn a_refused_chunk_keeps_the_earlier_ones_and_reports_a_short_window() {
        let mut mailbox =
            FakeMailbox::of_size(20_000).fetches_answer(&[Answer::Ok, Answer::Ok, Answer::Refuse]);
        let (sweep, result, _) = swept(&mut mailbox, 0).await;

        let err = result.expect_err("the refusal is reported");
        assert!(!is_timeout(&err), "the connection is still usable");
        assert_eq!(mailbox.ranges.len(), 3, "the sweep stops at the refusal");
        assert_eq!(sweep.swept, 2_000);
        assert_eq!(sweep.msgs.len(), 2_000, "two chunks of real messages");
        assert!(sweep.short(), "the window has a hole at its oldest edge");
        assert!(!sweep.reached_end);
    }

    /// A timeout stops the sweep the same way, but the session state is unknown
    /// afterwards — the caller has to be able to tell, so it drops the client.
    #[tokio::test]
    async fn a_timeout_stops_the_sweep_and_the_caller_can_tell() {
        let mut mailbox =
            FakeMailbox::of_size(20_000).fetches_answer(&[Answer::Ok, Answer::Timeout]);
        let (sweep, result, _) = swept(&mut mailbox, 0).await;

        assert!(is_timeout(&result.expect_err("the timeout is reported")));
        assert_eq!(sweep.swept, 1_000, "the chunk that landed still counts");
        assert!(sweep.short());
    }

    /// The alert renders UIDs swept out of the bound, plus the stacks found so
    /// far — the number the user is actually waiting for (ADR 0003).
    #[tokio::test]
    async fn progress_reports_uids_out_of_the_bound_and_stacks_so_far() {
        // 1,500 messages from two senders: the newest 1,000 from a@, the rest
        // from b@, so the stack count moves between the two chunks
        let mut msgs: Vec<MsgMeta> = (1..=500).map(|uid| meta(uid, "b@x.com")).collect();
        msgs.extend((501..=1_500).map(|uid| meta(uid, "a@x.com")));
        let mut mailbox = FakeMailbox::new(msgs);

        let (_, result, progress) = swept(&mut mailbox, 0).await;

        assert!(result.is_ok());
        let seen: Vec<(usize, usize, usize)> = progress
            .iter()
            .map(|p| (p.swept, p.bound, p.stacks))
            .collect();
        assert_eq!(seen, [(1_000, 1_500, 1), (1_500, 1_500, 2)]);
    }

    #[tokio::test]
    async fn one_sender_counts_once_however_many_messages_it_sent() {
        let mut mailbox = FakeMailbox::new((1..=10).map(|uid| meta(uid, "a@x.com")).collect());
        let (_, _, progress) = swept(&mut mailbox, 0).await;
        assert_eq!(progress.last().map(|p| p.stacks), Some(1));
    }

    /// Spec: "`FROM` is a substring match, so `a@b.com` also matches
    /// `xa@b.com` — results are filtered to the exact address." Without the
    /// filter, trashing one newsletter would take another sender's mail.
    #[tokio::test]
    async fn a_substring_match_never_lands_in_another_senders_fan_out() {
        let mut mailbox = FakeMailbox::new(vec![
            meta(1, "news@x.com"),
            meta(2, "xnews@x.com"),
            meta(3, "news@x.com"),
        ]);
        let (msgs, complete) = fan_out(&mut mailbox, "news@x.com").await.unwrap();

        assert!(complete);
        assert_eq!(msgs.len(), 2);
        assert!(
            msgs.iter().all(|m| m.sender_email == "news@x.com"),
            "xnews@ leaked into news@'s fan-out"
        );
    }

    /// A sender with more mail than one FETCH can carry is split across
    /// commands. If a later chunk is refused, the chunks that did arrive are
    /// real messages and are worth keeping.
    #[tokio::test]
    async fn a_refused_fan_out_chunk_keeps_the_chunks_that_arrived() {
        let msgs: Vec<MsgMeta> = (1..=(FETCH_CHUNK as u32 + 500))
            .map(|uid| meta(uid, "big@x.com"))
            .collect();
        // 1,500 messages is two fan-out chunks; the second is refused
        let mut mailbox = FakeMailbox::new(msgs).fetches_answer(&[Answer::Ok, Answer::Refuse]);

        let (msgs, complete) = fan_out(&mut mailbox, "big@x.com").await.unwrap();

        assert!(!complete, "the count is short and must say so");
        assert_eq!(msgs.len(), FETCH_CHUNK, "the first chunk survives");
    }

    #[tokio::test]
    async fn a_refused_fan_out_search_is_an_error_not_an_empty_result() {
        let mut mailbox = FakeMailbox::new(vec![meta(1, "bad@x.com")])
            .search_answers("bad@x.com", Answer::Refuse);
        let err = fan_out(&mut mailbox, "bad@x.com")
            .await
            .expect_err("refused");
        assert!(!is_timeout(&err));
    }

    #[test]
    fn a_timeout_is_distinguishable_from_a_server_refusal() {
        let refused = anyhow!("NO [CANNOT] search failed");
        assert!(!is_timeout(&refused));
        let out = anyhow!(TimedOut {
            what: "imap sender search".into(),
            secs: 60,
        });
        assert!(is_timeout(&out));
        assert!(format!("{out}").contains("press R to reconnect"));
        // still a timeout once it has been given context by a caller
        assert!(is_timeout(&out.context("loading batch")));
    }
}
