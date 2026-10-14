//! Append-only JSON-lines audit log of downloads and site changes, in the user's data folder.

use std::{path::PathBuf, sync::Arc};

use serde::Serialize;
use tokio::{io::AsyncWriteExt, sync::Mutex};

use crate::sites::now;

#[derive(Clone)]
pub struct Audit {
    path: Arc<PathBuf>,
    user: Arc<String>,
    lock: Arc<Mutex<()>>,
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
        Self { path: Arc::new(path), user: Arc::new(user), lock: Arc::default() }
    }

    /// Records an entry in the background; failures are logged, never shown.
    pub fn record(&self, entry: Entry) {
        let this = self.clone();
        crate::app::rt().spawn(async move {
            let mut v = serde_json::to_value(&entry).unwrap_or_default();
            v["ts"] = now().into();
            v["user"] = this.user.as_str().into();
            let line = format!("{v}\n");
            tracing::info!(target: "audit", "{}", line.trim_end());
            let _g = this.lock.lock().await;
            let r = async {
                if let Some(dir) = this.path.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&*this.path).await?;
                f.write_all(line.as_bytes()).await
            }
            .await;
            if let Err(e) = r {
                tracing::error!(path = %this.path.display(), error = %e, "audit log write failed");
            }
        });
    }
}
