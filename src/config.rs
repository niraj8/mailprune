use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub email: String,
    #[serde(default = "default_imap_host")]
    pub imap_host: String,
    #[serde(default = "default_smtp_host")]
    pub smtp_host: String,
}

fn default_imap_host() -> String {
    "imap.gmail.com".into()
}

fn default_smtp_host() -> String {
    "smtp.gmail.com".into()
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(rename = "accounts")]
    pub accounts: Vec<AccountConfig>,
}

pub fn config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".config/mailprune/config.toml"))
}

pub const SAMPLE_CONFIG: &str = r#"# mailprune config
[[accounts]]
name = "personal"
email = "you@gmail.com"
# imap_host = "imap.gmail.com"   # default
# smtp_host = "smtp.gmail.com"   # default

# [[accounts]]
# name = "work"
# email = "you@yourdomain.com"
"#;

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        bail!(
            "no config found at {}\n\ncreate it with:\n\n{}",
            path.display(),
            SAMPLE_CONFIG
        );
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if cfg.accounts.is_empty() {
        bail!("config has no [[accounts]] entries");
    }
    Ok(cfg)
}

const KEYRING_SERVICE: &str = "mailprune";

/// Gmail app passwords are 16 letters; Google displays them with spaces.
/// Strip all whitespace so pasted "xxxx xxxx xxxx xxxx" still works.
fn clean(password: &str) -> String {
    password.split_whitespace().collect()
}

pub fn get_password(email: &str) -> Result<String> {
    if let Ok(v) = std::env::var(format!(
        "MAILPRUNE_PASSWORD_{}",
        email.replace(['@', '.', '-', '+'], "_").to_uppercase()
    )) {
        return Ok(clean(&v));
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, email)?;
    if let Ok(stored) = entry.get_password() {
        return Ok(clean(&stored));
    }
    // migrate entries stored under the pre-rename service name
    let old = keyring::Entry::new("mailstack", email)?;
    let stored = old.get_password().with_context(|| {
        format!(
            "no app password stored for {email}.\nrun: mailprune auth {email}\n(generate one at https://myaccount.google.com/apppasswords)"
        )
    })?;
    let stored = clean(&stored);
    if entry.set_password(&stored).is_ok() {
        let _ = old.delete_credential();
    }
    Ok(stored)
}

pub fn store_password(email: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, email)?;
    entry.set_password(&clean(password))?;
    Ok(())
}
