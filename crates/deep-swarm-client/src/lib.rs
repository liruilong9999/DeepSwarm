pub mod error;
pub mod models;
pub mod streaming;
pub mod tokenizer;

mod client;

pub use client::{ChatCompletionStream, DeepSeekClient, RetryPolicy};
pub use error::{Error, Result};
pub use tokenizer::{DeepSeekTokenizer, TokenCount, TokenizerError, UsageMismatch};

pub const CONTRACT_ID: &str = "deepseek-1.0.0+2026-07-13";
