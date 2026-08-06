//! Timestamped debug log for postmortems — a TUI can't print, so network
//! operations log start/done lines here. When the UI hangs, the last line
//! names the operation that never came back.
//!
//! Always on, best-effort (failures ignored), at
//! ~/.local/state/mailprune/debug.log. Opt out: MAILPRUNE_NO_DEBUG_LOG=1.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

const ROTATE_BYTES: u64 = 2 * 1024 * 1024;

static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

fn path() -> Option<&'static PathBuf> {
    PATH.get_or_init(|| {
        if std::env::var_os("MAILPRUNE_NO_DEBUG_LOG").is_some() {
            return None;
        }
        let path = crate::action_log::state_dir().join("debug.log");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        // keep one previous generation instead of growing forever
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        Some(path)
    })
    .as_ref()
}

pub fn write(msg: impl AsRef<str>) {
    let Some(path) = path() else { return };
    let line = format!(
        "{} {}\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
        msg.as_ref()
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}
