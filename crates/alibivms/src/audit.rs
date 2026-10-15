//! Append-only JSON-lines audit log of downloads and site changes, in the user's data folder.
//! Rotated once to `audit.log.1` when it passes [`MAX_BYTES`].

use std::{
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use serde::Serialize;
use tokio::{io::AsyncWriteExt, sync::Mutex, task::JoinHandle};

use crate::sites::now;

/// Size at which the log is moved to `.1` and a fresh one started.
const MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone)]
pub struct Audit {
    path: Arc<PathBuf>,
    user: Arc<String>,
    lock: Arc<Mutex<()>>,
    /// Writes not yet known to be finished, so the close path can wait for them.
    pending: Arc<StdMutex<Vec<JoinHandle<()>>>>,
}

#[derive(Serialize, Default)]
pub struct Entry {
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub begin: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Audit {
    pub fn new(path: PathBuf) -> Self {
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "unknown".into());
        Self { path: Arc::new(path), user: Arc::new(user), lock: Arc::default(), pending: Arc::default() }
    }

    /// Records an entry in the background; failures are logged, never shown.
    pub fn record(&self, entry: Entry) {
        let this = self.clone();
        let task = crate::app::rt().spawn(async move {
            let mut v = serde_json::to_value(&entry).unwrap_or_default();
            v["ts"] = now().into();
            v["user"] = this.user.as_str().into();
            let line = format!("{v}\n");
            tracing::info!(target: "audit", "{}", line.trim_end());
            let _g = this.lock.lock().await;
            if let Err(e) = this.append(&line).await {
                tracing::error!(path = %this.path.display(), error = %e, "audit log write failed");
            }
        });
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|t| !t.is_finished());
        pending.push(task);
    }

    async fn append(&self, line: &str) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        if tokio::fs::metadata(&*self.path).await.is_ok_and(|m| m.len() >= MAX_BYTES) {
            tokio::fs::rename(&*self.path, self.path.with_extension("log.1")).await?;
        }
        let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&*self.path).await?;
        f.write_all(line.as_bytes()).await
    }

    /// Waits for every write recorded so far. Call before quitting, under a timeout.
    pub async fn flush(&self) {
        let tasks = std::mem::take(&mut *self.pending.lock().unwrap());
        for t in tasks {
            let _ = t.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_rotates() {
        let dir = std::env::temp_dir().join(format!("alibivms-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");
        std::fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();
        let audit = Audit::new(path.clone());
        audit.record(Entry { action: "test", site: Some("hq".into()), ..Default::default() });
        crate::app::rt().block_on(audit.flush());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""action":"test""#) && text.contains(r#""site":"hq""#) && text.ends_with('\n'));
        assert_eq!(std::fs::metadata(dir.join("audit.log.1")).unwrap().len(), MAX_BYTES);
        assert!(audit.pending.lock().unwrap().is_empty() || audit.pending.lock().unwrap().iter().all(|t| t.is_finished()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
