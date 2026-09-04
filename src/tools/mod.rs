//! Built-in tools invoked via OpenAI-style function calling.
//!
//! Each tool declares a JSON schema (consumed by the LLM) and an async
//! executor (run when the model emits a tool call). The pipeline lives in
//! `loop_pipeline.rs` — it streams the model's first response, runs any
//! tool calls, and either streams a follow-up turn or returns the result.

pub mod calculator;
pub mod datetime;
pub mod fetch_page;
pub mod image_recover;
pub mod force_search;
pub mod image_search;
pub mod loop_pipeline;
pub mod registry;
pub mod status_phrases;
pub mod video_transcript;
pub mod web_search;
pub mod x_search;
