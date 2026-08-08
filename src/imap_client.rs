use anyhow::{Context, Result, anyhow};
use async_imap::imap_proto::BodyStructure;
use chrono::Utc;
use futures::StreamExt;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use crate::config::AccountConfig;
use crate::stacks::MsgMeta;

type Session = async_imap::Session<TlsStream<TcpStream>>;

const OP_TIMEOUT_SECS: u64 = 30;
/// header fetch of a large inbox legitimately takes a while
const FETCH_TIMEOUT_SECS: u64 = 180;

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
            Err(anyhow!(
                "{what} timed out after {secs}s — connection presumed dead, press R to reconnect"
            ))
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

        Ok(Self {
            session,
            trash_folder,
            archive_folder,
        })
    }

    pub async fn fetch_inbox(&mut self) -> Result<Vec<MsgMeta>> {
        crate::debuglog::write("imap fetch start");
        let session = &mut self.session;
        let out = timed(FETCH_TIMEOUT_SECS, "imap fetch", async move {
            let mailbox = session.select("INBOX").await?;
            if mailbox.exists == 0 {
                return Ok(Vec::new());
            }
            let mut out = Vec::with_capacity(mailbox.exists as usize);
            let mut stream = session
                .uid_fetch(
                    "1:*",
                    "(UID FLAGS INTERNALDATE BODYSTRUCTURE RFC822.HEADER)",
                )
                .await?;
            while let Some(fetch) = stream.next().await {
                let fetch = fetch?;
                let Some(uid) = fetch.uid else { continue };
                let unread = !fetch
                    .flags()
                    .any(|f| matches!(f, async_imap::types::Flag::Seen));
                let date = fetch.internal_date().map(|d| d.with_timezone(&Utc));
                let has_attachment = fetch.bodystructure().is_some_and(has_attachment);
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
                    has_attachment,
                    list_unsubscribe,
                    one_click,
                });
            }
            Ok(out)
        })
        .await?;
        crate::debuglog::write(format!("imap fetch done {} msgs", out.len()));
        Ok(out)
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

/// Does any part of the message carry `Content-Disposition: attachment`?
///
/// Only that disposition counts. Marketing mail is almost always
/// multipart/related with inline images, so treating "has a non-text part" as
/// an attachment would put a paperclip on nearly every stack.
fn has_attachment(body: &BodyStructure) -> bool {
    let (common, children) = match body {
        BodyStructure::Basic { common, .. }
        | BodyStructure::Text { common, .. }
        | BodyStructure::Message { common, .. } => (common, None),
        BodyStructure::Multipart { common, bodies, .. } => (common, Some(bodies)),
    };
    if let Some(d) = &common.disposition
        && d.ty.eq_ignore_ascii_case("attachment")
    {
        return true;
    }
    children.is_some_and(|bodies| bodies.iter().any(has_attachment))
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
    use super::{has_attachment, uid_set};
    use async_imap::imap_proto::{
        BodyContentCommon, BodyContentSinglePart, BodyStructure, ContentDisposition,
        ContentEncoding, ContentType,
    };

    /// a leaf part of `ty/subtype`, optionally carrying a disposition
    fn part<'a>(ty: &'a str, subtype: &'a str, disposition: Option<&'a str>) -> BodyStructure<'a> {
        BodyStructure::Basic {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: ty.into(),
                    subtype: subtype.into(),
                    params: None,
                },
                disposition: disposition.map(|ty| ContentDisposition {
                    ty: ty.into(),
                    params: None,
                }),
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: ContentEncoding::Base64,
                octets: 100,
            },
            extension: None,
        }
    }

    fn multipart<'a>(subtype: &'a str, bodies: Vec<BodyStructure<'a>>) -> BodyStructure<'a> {
        BodyStructure::Multipart {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: "multipart".into(),
                    subtype: subtype.into(),
                    params: None,
                },
                disposition: None,
                language: None,
                location: None,
            },
            bodies,
            extension: None,
        }
    }

    #[test]
    fn plain_and_inline_only_mail_has_no_attachment() {
        assert!(!has_attachment(&part("text", "plain", None)));
        // the newsletter shape: inline images must not trip the indicator
        assert!(!has_attachment(&multipart(
            "related",
            vec![
                part("text", "html", None),
                part("image", "png", Some("inline")),
            ]
        )));
    }

    #[test]
    fn attachment_disposition_is_found_at_any_depth() {
        assert!(has_attachment(&multipart(
            "mixed",
            vec![
                part("text", "plain", None),
                part("application", "pdf", Some("attachment")),
            ]
        )));
        // nested multipart/alternative inside multipart/mixed
        assert!(has_attachment(&multipart(
            "mixed",
            vec![
                multipart(
                    "alternative",
                    vec![part("text", "plain", None), part("text", "html", None)]
                ),
                multipart("mixed", vec![part("image", "jpeg", Some("ATTACHMENT"))]),
            ]
        )));
    }

    #[test]
    fn uid_set_compresses_ranges() {
        assert_eq!(uid_set(&[3, 1, 2, 7, 9, 8]), "1:3,7:9");
        assert_eq!(uid_set(&[5]), "5");
        assert_eq!(uid_set(&[2, 2, 4]), "2,4");
    }
}
