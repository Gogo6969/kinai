//! SQLx-backed SQLite layer.

mod memory;
pub mod messages;
pub mod reports;
pub(crate) mod migrate;
pub mod telegram;
pub mod user_facts;

use std::path::Path;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

pub use memory::MemoryNote;
pub use reports::Report;
pub use messages::{Attachment, Message, SearchHit, ThreadMeta, HOST_PEER};
pub use user_facts::UserFact;

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
    /// Per-thread sticky LLM-slot ("fast" / "deep" / None for default).
    /// See `slash::route_for` for how this gets applied per turn.
    pub async fn thread_active_slot(
        &self,
        peer_id: &str,
        id: &str,
    ) -> Result<Option<String>> {
        messages::thread_active_slot(&self.pool, peer_id, id).await
    }
    pub async fn set_thread_active_slot(
        &self,
        peer_id: &str,
        id: &str,
        slot: Option<&str>,
    ) -> Result<()> {
        messages::set_thread_active_slot(&self.pool, peer_id, id, slot).await
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
    /// `load_messages` for the LLM context builder: image payloads are
    /// stripped at the SQL layer (PDFs kept). See messages::load_for_context.
    pub async fn load_messages_for_context(
        &self,
        peer_id: &str,
        thread_id: &str,
        limit: i64,
    ) -> Result<Vec<Message>> {
        messages::load_for_context(&self.pool, peer_id, thread_id, limit).await
    }
    pub async fn search_messages(
        &self,
        peer_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        messages::search(&self.pool, peer_id, query, limit).await
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
    // ---- Reports (answers a family member flagged) --------------------
    #[allow(clippy::too_many_arguments)]
    pub async fn add_report(
        &self,
        peer_id: &str,
        reporter: &str,
        message_id: &str,
        question: &str,
        answer: &str,
        model: &str,
        slot: &str,
    ) -> Result<Report> {
        reports::add(&self.pool, peer_id, reporter, message_id, question, answer, model, slot).await
    }
    pub async fn list_reports(&self, limit: i64) -> Result<Vec<Report>> {
        reports::list(&self.pool, limit).await
    }
    pub async fn open_report_count(&self) -> Result<i64> {
        reports::open_count(&self.pool).await
    }
    pub async fn set_report_reviewed(&self, id: &str, reviewed: bool) -> Result<()> {
        reports::set_reviewed(&self.pool, id, reviewed).await
    }
    pub async fn delete_report(&self, id: &str) -> Result<()> {
        reports::delete(&self.pool, id).await
    }

    /// (question, answer) for the fact-check button — peer-scoped.
    pub async fn question_answer_for_fact_check(
        &self,
        peer_id: &str,
        message_id: &str,
    ) -> Result<Option<(String, String)>> {
        messages::question_answer_for_fact_check(&self.pool, peer_id, message_id).await
    }
    pub async fn update_message(&self, id: &str, new_content: &str) -> Result<()> {
        messages::update_content(&self.pool, id, new_content).await
    }
    /// Truncate a thread at `from_created_at` (inclusive), peer-scoped.
    /// Returns rows removed. Backs regenerate/edit-and-resend.
    pub async fn delete_messages_from(
        &self,
        peer_id: &str,
        thread_id: &str,
        from_created_at: &str,
    ) -> Result<u64> {
        messages::delete_from(&self.pool, peer_id, thread_id, from_created_at).await
    }
    /// Fetch one message by id, scoped to a peer's threads (`None` if not
    /// owned / not found).
    pub async fn get_message(&self, peer_id: &str, id: &str) -> Result<Option<Message>> {
        messages::get_by_id(&self.pool, peer_id, id).await
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

    // ---- User facts (persistent per-peer memory) ----

    /// Insert-or-update a fact. `source` is one of "tool" / "extractor"
    /// / "manual" — the UI uses it to mark how each fact was learned.
    pub async fn save_user_fact(
        &self,
        peer_id: &str,
        key: &str,
        value: &str,
        source: &str,
        source_msg_id: Option<&str>,
    ) -> Result<UserFact> {
        user_facts::upsert(&self.pool, peer_id, key, value, source, source_msg_id).await
    }

    /// All facts for a peer, newest-first. Used by the Settings page
    /// and by `for_prompt` for context-injection.
    pub async fn list_user_facts(&self, peer_id: &str) -> Result<Vec<UserFact>> {
        user_facts::list(&self.pool, peer_id).await
    }

    /// Facts to inject into the system prompt. Currently identical to
    /// list_user_facts; broken out so we can add caching / limits later
    /// without touching the Settings UI's "show me everything" call.
    pub async fn user_facts_for_prompt(&self, peer_id: &str) -> Result<Vec<UserFact>> {
        user_facts::for_prompt(&self.pool, peer_id).await
    }

    pub async fn delete_user_fact(&self, peer_id: &str, id: &str) -> Result<()> {
        user_facts::delete(&self.pool, peer_id, id).await
    }

    pub async fn delete_user_fact_by_key(&self, peer_id: &str, key: &str) -> Result<u64> {
        user_facts::delete_by_key(&self.pool, peer_id, key).await
    }

    pub async fn clear_user_facts(&self, peer_id: &str) -> Result<u64> {
        user_facts::clear_all(&self.pool, peer_id).await
    }
}
