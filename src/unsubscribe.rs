use anyhow::{bail, Context, Result};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::AccountConfig;
use crate::stacks::MsgMeta;

#[derive(Debug, Clone)]
pub enum Method {
    /// RFC 8058 one-click: POST to https URL, no browser needed
    OneClick(String),
    /// send an email to the unsubscribe address
    Mailto { to: String, subject: String },
    /// plain https link — open in browser
    Browser(String),
}

impl Method {
    pub fn describe(&self) -> &'static str {
        match self {
            Method::OneClick(_) => "one-click POST",
            Method::Mailto { .. } => "unsubscribe email",
            Method::Browser(_) => "open in browser",
        }
    }
}

/// Parse a List-Unsubscribe header value: comma-separated <uri> entries.
pub fn pick_method(msg: &MsgMeta) -> Option<Method> {
    let header = msg.list_unsubscribe.as_deref()?;
    let mut https_url: Option<String> = None;
    let mut mailto: Option<Method> = None;
    for part in header.split(',') {
        let uri = part.trim().trim_start_matches('<').trim_end_matches('>').trim();
        if uri.starts_with("https://") || uri.starts_with("http://") {
            https_url.get_or_insert_with(|| uri.to_string());
        } else if let Some(rest) = uri.strip_prefix("mailto:") {
            let (to, query) = match rest.split_once('?') {
                Some((a, q)) => (a.to_string(), q),
                None => (rest.to_string(), ""),
            };
            let mut subject = String::from("unsubscribe");
            for kv in query.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    if k.eq_ignore_ascii_case("subject") {
                        subject = urldecode(v);
                    }
                }
            }
            mailto.get_or_insert(Method::Mailto { to, subject });
        }
    }
    if msg.one_click {
        if let Some(url) = &https_url {
            return Some(Method::OneClick(url.clone()));
        }
    }
    // prefer mailto over browser: fully automatic
    mailto.or(https_url.map(Method::Browser))
}

pub async fn execute(
    method: &Method,
    account: &AccountConfig,
    password: &str,
) -> Result<String> {
    match method {
        Method::OneClick(url) => {
            let client = reqwest::Client::builder()
                .user_agent("mailstack/0.1")
                .timeout(std::time::Duration::from_secs(15))
                .build()?;
            let resp = client
                .post(url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body("List-Unsubscribe=One-Click")
                .send()
                .await
                .with_context(|| format!("POST {url}"))?;
            let status = resp.status();
            if status.is_success() {
                Ok(format!("unsubscribed (one-click, HTTP {status})"))
            } else {
                bail!("one-click POST returned HTTP {status}")
            }
        }
        Method::Mailto { to, subject } => {
            let email = Message::builder()
                .from(account.email.parse()?)
                .to(to.parse().with_context(|| format!("bad mailto address {to}"))?)
                .subject(subject.clone())
                .body(String::from("unsubscribe"))?;
            let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay(&account.smtp_host)?
                .credentials(lettre::transport::smtp::authentication::Credentials::new(
                    account.email.clone(),
                    password.to_string(),
                ))
                .build();
            mailer.send(email).await.context("sending unsubscribe email")?;
            Ok(format!("unsubscribe email sent to {to}"))
        }
        Method::Browser(url) => {
            open::that_detached(url).with_context(|| format!("opening {url}"))?;
            Ok("opened unsubscribe page in browser".into())
        }
    }
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
