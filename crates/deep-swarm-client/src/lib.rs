pub mod error;
pub mod models;
pub mod streaming;

mod client;

pub use client::{ChatCompletionStream, DeepSeekClient, RetryPolicy};
pub use error::{Error, Result};

pub const CONTRACT_ID: &str = "deepseek-1.0.0+2026-07-13";
