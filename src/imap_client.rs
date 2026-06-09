use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures::StreamExt;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use crate::config::AccountConfig;
use crate::stacks::MsgMeta;

type Session = async_imap::Session<TlsStream<TcpStream>>;

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

        Ok(Self {
            session,
            trash_folder,
            archive_folder,
        })
    }

    pub async fn fetch_inbox(&mut self) -> Result<Vec<MsgMeta>> {
        let mailbox = self.session.select("INBOX").await?;
        if mailbox.exists == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(mailbox.exists as usize);
        {
            let mut stream = self
                .session
                .uid_fetch("1:*", "(UID FLAGS INTERNALDATE RFC822.HEADER)")
                .await?;
            while let Some(fetch) = stream.next().await {
                let fetch = fetch?;
                let Some(uid) = fetch.uid else { continue };
                let unread = !fetch
                    .flags()
                    .any(|f| matches!(f, async_imap::types::Flag::Seen));
                let date = fetch.internal_date().map(|d| d.with_timezone(&Utc));
                let Some(header_bytes) = fetch.header() else {
                    continue;
                };
                let (headers, _) = match mailparse::parse_headers(header_bytes) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                use mailparse::MailHeaderMap;
                let from_raw = headers.get_first_value("From").unwrap_or_default();
                let (sender_name, sender_email) = parse_from(&from_raw);
                let subject = headers.get_first_value("Subject").unwrap_or_default();
                let list_unsubscribe = headers.get_first_value("List-Unsubscribe");
                let one_click = headers
                    .get_first_value("List-Unsubscribe-Post")
                    .map(|v| v.to_lowercase().contains("one-click"))
                    .unwrap_or(false);
                out.push(MsgMeta {
                    uid,
                    sender_email,
                    sender_name,
                    subject,
                    date,
                    unread,
                    list_unsubscribe,
                    one_click,
                });
            }
        }
        Ok(out)
    }

    pub async fn trash(&mut self, uids: &[u32]) -> Result<()> {
        let folder = self.trash_folder.clone();
        self.session.uid_mv(uid_set(uids), &folder).await?;
        Ok(())
    }

    pub async fn archive(&mut self, uids: &[u32]) -> Result<()> {
        let folder = self.archive_folder.clone();
        self.session.uid_mv(uid_set(uids), &folder).await?;
        Ok(())
    }

    pub async fn mark_read(&mut self, uids: &[u32]) -> Result<()> {
        let mut stream = self
            .session
            .uid_store(uid_set(uids), "+FLAGS (\\Seen)")
            .await?;
        while let Some(item) = stream.next().await {
            item?;
        }
        Ok(())
    }

    pub async fn logout(mut self) {
        let _ = self.session.logout().await;
    }
}

/// "John Doe <a@b.com>" -> ("John Doe", "a@b.com"); RFC 2047 already decoded by mailparse
fn parse_from(raw: &str) -> (String, String) {
    if let Ok(list) = mailparse::addrparse(raw) {
        for addr in list.iter() {
            match addr {
                mailparse::MailAddr::Single(s) => {
                    return (
                        s.display_name.clone().unwrap_or_default(),
                        s.addr.clone(),
                    )
                }
                mailparse::MailAddr::Group(g) => {
                    if let Some(s) = g.addrs.first() {
                        return (
                            s.display_name.clone().unwrap_or_default(),
                            s.addr.clone(),
                        );
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
    use super::uid_set;

    #[test]
    fn uid_set_compresses_ranges() {
        assert_eq!(uid_set(&[3, 1, 2, 7, 9, 8]), "1:3,7:9");
        assert_eq!(uid_set(&[5]), "5");
        assert_eq!(uid_set(&[2, 2, 4]), "2,4");
    }
}
