//! SQLx-backed SQLite layer.

mod memory;
pub mod messages;
mod migrate;

use std::path::Path;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

pub use memory::MemoryNote;
pub use messages::{Attachment, Message, ThreadMeta, HOST_PEER};

#[derive(Clone)]
pub struct Db {
    pub(crate) pool: SqlitePool,
}

impl Db {
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let opts = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;

        migrate::run(&pool).await?;
        Ok(Self { pool })
    }

    // Threads
    pub async fn list_threads(&self, peer_id: &str) -> Result<Vec<ThreadMeta>> {
        messages::list_threads(&self.pool, peer_id).await
    }
    pub async fn create_thread(
        &self,
        peer_id: &str,
        title: Option<&str>,
    ) -> Result<ThreadMeta> {
        messages::create_thread(&self.pool, peer_id, title).await
    }
    /// Idempotent thread insert by caller-supplied ID. Returns `Ok(())`
    /// whether or not it already existed.
    pub async fn upsert_thread(&self, peer_id: &str, id: &str, title: &str) -> Result<()> {
        messages::upsert_thread(&self.pool, peer_id, id, title).await
    }
    pub async fn rename_thread(&self, peer_id: &str, id: &str, title: &str) -> Result<()> {
        messages::rename_thread(&self.pool, peer_id, id, title).await
    }
    pub async fn delete_thread(&self, peer_id: &str, id: &str) -> Result<()> {
        messages::delete_thread(&self.pool, peer_id, id).await
    }

    // Messages
    pub async fn load_messages(
        &self,
        peer_id: &str,
        thread_id: &str,
        limit: i64,
    ) -> Result<Vec<Message>> {
        messages::load(&self.pool, peer_id, thread_id, limit).await
    }
    pub async fn append_message(
        &self,
        thread_id: &str,
        role: &str,
        sender: &str,
        content: &str,
        attachments: &[Attachment],
    ) -> Result<Message> {
        messages::append(&self.pool, thread_id, role, sender, content, attachments).await
    }
    pub async fn update_message(&self, id: &str, new_content: &str) -> Result<()> {
        messages::update_content(&self.pool, id, new_content).await
    }
    pub async fn set_message_metrics(
        &self,
        id: &str,
        metrics: &serde_json::Value,
    ) -> Result<()> {
        messages::set_metrics(&self.pool, id, metrics).await
    }
    pub async fn count_since_summary(&self, peer_id: &str, thread_id: &str) -> Result<i64> {
        messages::count_since_summary(&self.pool, peer_id, thread_id).await
    }
    pub async fn oldest_unsummarized(
        &self,
        peer_id: &str,
        thread_id: &str,
        keep_recent: i64,
    ) -> Result<Vec<Message>> {
        messages::oldest_unsummarized(&self.pool, peer_id, thread_id, keep_recent).await
    }
    pub async fn mark_summarized(&self, ids: &[String], summary_id: &str) -> Result<()> {
        messages::mark_summarized(&self.pool, ids, summary_id).await
    }

    // Long-term memory
    pub async fn save_memory(
        &self,
        peer_id: &str,
        thread_id: &str,
        summary: &str,
        keywords: &str,
    ) -> Result<MemoryNote> {
        memory::save(&self.pool, peer_id, thread_id, summary, keywords).await
    }
    pub async fn relevant_memories(
        &self,
        peer_id: &str,
        thread_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<MemoryNote>> {
        memory::search(&self.pool, peer_id, thread_id, query, limit).await
    }
}
