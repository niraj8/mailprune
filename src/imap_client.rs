use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
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

/// senders discovered per batch — one `m` press
pub const SENDERS_PER_BATCH: usize = 20;
/// UIDs discovery may scan before giving up on finding more senders. Counts
/// UIDs rather than returned headers: after trashing, dead UIDs stay in the
/// list and return nothing, and the budget must bound those too.
const SCAN_BUDGET: usize = 500;
/// UIDs per discovery FETCH
const DISCOVERY_CHUNK: usize = 100;
/// UIDs per fan-out FETCH. A single sender can own tens of thousands of
/// messages; splitting keeps the command line and each timeout bounded.
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

    /// Every UID in INBOX, newest first. ~7 bytes a UID, so even a six-figure
    /// mailbox is under a megabyte in one round trip — three orders of
    /// magnitude below fetching its headers. UIDs are immutable, so the cursor
    /// walked over this list survives trashing; sequence numbers would not.
    pub async fn uid_list(&mut self) -> Result<Vec<u32>> {
        crate::debuglog::write("imap uid list start");
        let session = &mut self.session;
        let uids = timed(SEARCH_TIMEOUT_SECS, "imap uid list", async move {
            let mailbox = session.select("INBOX").await?;
            if mailbox.exists == 0 {
                return Ok(Vec::new());
            }
            Ok(newest_first(session.uid_search("ALL").await?))
        })
        .await?;
        crate::debuglog::write(format!("imap uid list done {} uids", uids.len()));
        Ok(uids)
    }

    /// Headers for exactly these UIDs in one FETCH, in whatever order the
    /// server returns them. Callers pass no more than `FETCH_CHUNK` at a time,
    /// so no single command and no single timeout is unbounded.
    async fn fetch_chunk(&mut self, uids: &[u32]) -> Result<Vec<MsgMeta>> {
        let session = &mut self.session;
        let set = uid_set(uids);
        timed(FETCH_TIMEOUT_SECS, "imap header fetch", async move {
            let mut out = Vec::with_capacity(uids.len());
            let mut stream = session.uid_fetch(set, HEADER_QUERY).await?;
            while let Some(fetch) = stream.next().await {
                let fetch = fetch?;
                if let Some(msg) = to_meta(&fetch) {
                    out.push(msg);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Every message from `addr` still in INBOX, and whether that is all of
    /// them. `FROM` is a substring match server-side, so `a@b.com` also matches
    /// `xa@b.com` — the results are filtered back to the exact address.
    ///
    /// A refused chunk stops the walk but keeps the chunks that did arrive:
    /// they are real messages, and falling all the way back to the discovery
    /// sample would under-report further than the failure requires.
    async fn fan_out(&mut self, addr: &str) -> Result<(Vec<MsgMeta>, bool)> {
        let session = &mut self.session;
        let query = format!("FROM {}", quoted(addr));
        let uids = timed(SEARCH_TIMEOUT_SECS, "imap sender search", async move {
            Ok(newest_first(session.uid_search(query).await?))
        })
        .await?;
        let mut msgs = Vec::with_capacity(uids.len());
        let mut complete = true;
        for chunk in uids.chunks(FETCH_CHUNK) {
            match self.fetch_chunk(chunk).await {
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

    /// Read headers newest-first until `SENDERS_PER_BATCH` senders not already
    /// in `known` turn up, or the scan budget runs out. The messages read here
    /// exist only to learn addresses — fan-out re-reads them properly.
    /// `found` is written in place rather than returned, so a chunk that fails
    /// halfway through does not throw away the cursor the earlier chunks earned.
    async fn discover(
        &mut self,
        uids: &[u32],
        known: &HashSet<String>,
        found: &mut Discovery,
    ) -> Result<()> {
        let mut scanned = 0;
        while found.cursor < uids.len()
            && found.order.len() < SENDERS_PER_BATCH
            && scanned < SCAN_BUDGET
        {
            let take = DISCOVERY_CHUNK
                .min(uids.len() - found.cursor)
                .min(SCAN_BUDGET - scanned);
            let chunk = &uids[found.cursor..found.cursor + take];
            let fetched = self.fetch_chunk(chunk).await?;
            scanned += consume_chunk(chunk, fetched, known, found);
        }
        crate::debuglog::write(format!(
            "imap discovery found {} senders in {scanned} uids",
            found.order.len()
        ));
        Ok(())
    }

    /// One batch: discover up to `SENDERS_PER_BATCH` new senders, then fan each
    /// one out, handing it to `on_sender` as it resolves so stacks appear while
    /// the rest of the batch is still running.
    ///
    /// Returns the cursor *and* the outcome, rather than a `Result` that
    /// carries one or the other: a fan-out failure does not un-scan the UIDs
    /// discovery already read, and losing the cursor would make the next `m`
    /// spend its whole budget re-reading them.
    ///
    /// A server `NO`/`BAD` for one sender yields a partial batch — never a
    /// silent drop, because a sender missing from a triage list is
    /// undetectable. A timeout aborts: the session state is unknown afterwards,
    /// so the remaining senders would be 19 more commands into a dead socket,
    /// each waiting out its own timeout.
    pub async fn load_batch(
        &mut self,
        uids: &[u32],
        cursor: usize,
        known: &HashSet<String>,
        mut on_sender: impl FnMut(SenderBatch),
    ) -> (usize, Result<()>) {
        let mut found = Discovery {
            cursor,
            order: Vec::new(),
            samples: HashMap::new(),
        };
        if let Err(e) = self.discover(uids, known, &mut found).await {
            return (found.cursor, Err(e));
        }
        for addr in std::mem::take(&mut found.order) {
            let sample = found.samples.remove(&addr).unwrap_or_default();
            match self.fan_out(&addr).await {
                Ok((msgs, complete)) => {
                    // a refused chunk can still leave nothing behind
                    let msgs = if msgs.is_empty() && !complete {
                        sample
                    } else {
                        msgs
                    };
                    on_sender(SenderBatch {
                        addr,
                        msgs,
                        partial: !complete,
                    })
                }
                Err(e) if is_timeout(&e) => return (found.cursor, Err(e)),
                Err(e) => {
                    crate::debuglog::write(format!("imap search refused for {addr}: {e:#}"));
                    on_sender(SenderBatch {
                        addr,
                        msgs: sample,
                        partial: true,
                    });
                }
            }
        }
        (found.cursor, Ok(()))
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

/// one sender's mail, handed back mid-batch as soon as it resolves
pub struct SenderBatch {
    pub addr: String,
    pub msgs: Vec<MsgMeta>,
    /// the fan-out was refused; `msgs` is the discovery sample, not the
    /// sender's whole mail, so the count under-reports
    pub partial: bool,
}

/// what one discovery pass learned
struct Discovery {
    cursor: usize,
    /// new senders, lowercased, in the order they were first seen
    order: Vec<String>,
    /// their discovery-window messages, kept only as a fallback for a sender
    /// whose fan-out is refused
    samples: HashMap<String, Vec<MsgMeta>>,
}

/// Fold one fetched chunk into `found`, walking `chunk` in order rather than
/// the fetch results, and return how many UIDs were used.
///
/// The cursor advances by exactly that count. Stopping at the sender limit
/// therefore leaves the rest of the chunk unread instead of skipping past it —
/// a sender sitting behind the 20th would otherwise never be discovered at
/// all. UIDs that returned no header (trashed since the list was taken) still
/// count as consumed.
fn consume_chunk(
    chunk: &[u32],
    fetched: Vec<MsgMeta>,
    known: &HashSet<String>,
    found: &mut Discovery,
) -> usize {
    let mut by_uid: HashMap<u32, MsgMeta> = fetched.into_iter().map(|m| (m.uid, m)).collect();
    let mut consumed = 0;
    for (i, uid) in chunk.iter().enumerate() {
        if found.order.len() >= SENDERS_PER_BATCH {
            break;
        }
        consumed = i + 1;
        let Some(msg) = by_uid.remove(uid) else {
            continue;
        };
        let addr = msg.sender_email.to_lowercase();
        if addr.is_empty() || known.contains(&addr) {
            continue;
        }
        match found.samples.get_mut(&addr) {
            Some(sample) => sample.push(msg),
            None => {
                found.order.push(addr.clone());
                found.samples.insert(addr, vec![msg]);
            }
        }
    }
    found.cursor += consumed;
    consumed
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

    /// (consumed, senders in discovery order, sample sizes by sender)
    fn consume(
        chunk: &[u32],
        fetched: Vec<MsgMeta>,
        known: &[&str],
    ) -> (usize, Vec<String>, HashMap<String, usize>) {
        let known: HashSet<String> = known.iter().map(|s| s.to_string()).collect();
        let mut found = Discovery {
            cursor: 0,
            order: Vec::new(),
            samples: HashMap::new(),
        };
        let consumed = consume_chunk(chunk, fetched, &known, &mut found);
        assert_eq!(found.cursor, consumed, "the cursor tracks what was read");
        let sizes = found
            .samples
            .into_iter()
            .map(|(k, v)| (k, v.len()))
            .collect();
        (consumed, found.order, sizes)
    }

    #[test]
    fn discovery_orders_senders_by_uid_not_by_fetch_order() {
        // the server may answer a fetch in any order; the walk is over the
        // chunk, which is newest-first
        let fetched = vec![meta(1, "old@x.com"), meta(3, "new@x.com")];
        let (consumed, order, sizes) = consume(&[3, 2, 1], fetched, &[]);
        assert_eq!(consumed, 3);
        assert_eq!(order, ["new@x.com", "old@x.com"]);
        assert_eq!(sizes["new@x.com"], 1);
    }

    #[test]
    fn known_senders_and_dead_uids_are_skipped_but_still_counted() {
        // uid 2 returned nothing (trashed since the list was taken)
        let fetched = vec![meta(3, "Known@x.com"), meta(1, "fresh@x.com")];
        let (consumed, order, _) = consume(&[3, 2, 1], fetched, &["known@x.com"]);
        assert_eq!(consumed, 3, "a dead uid still burns scan budget");
        assert_eq!(
            order,
            ["fresh@x.com"],
            "sender matching is case-insensitive"
        );
    }

    /// a sender sitting behind the 20th must still be discoverable later, so
    /// the cursor may not advance past UIDs the stop condition never looked at
    #[test]
    fn hitting_the_sender_limit_leaves_the_rest_of_the_chunk_unread() {
        let n = SENDERS_PER_BATCH as u32;
        let chunk: Vec<u32> = (0..=n).rev().collect();
        let fetched: Vec<MsgMeta> = (0..=n).map(|i| meta(i, &format!("s{i}@x.com"))).collect();

        let (consumed, order, _) = consume(&chunk, fetched, &[]);
        assert_eq!(order.len(), SENDERS_PER_BATCH);
        assert_eq!(
            consumed, SENDERS_PER_BATCH,
            "the 21st uid is left for later"
        );
        assert!(
            !order.contains(&"s0@x.com".to_string()),
            "the oldest is unread"
        );
    }

    #[test]
    fn repeat_messages_from_one_sender_grow_its_sample_not_the_sender_list() {
        let fetched = vec![meta(3, "a@x.com"), meta(2, "a@x.com"), meta(1, "b@x.com")];
        let (_, order, sizes) = consume(&[3, 2, 1], fetched, &[]);
        assert_eq!(order, ["a@x.com", "b@x.com"]);
        assert_eq!(sizes["a@x.com"], 2);
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
