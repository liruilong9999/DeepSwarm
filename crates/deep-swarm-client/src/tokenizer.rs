use tokenizers::Tokenizer;

use crate::models::Usage;

const TOKENIZER_JSON: &[u8] =
    include_bytes!("../../../docs/other/deepseek_v3_tokenizer/tokenizer.json");

#[derive(Clone)]
pub struct DeepSeekTokenizer {
    inner: Tokenizer,
}

impl DeepSeekTokenizer {
    pub fn from_embedded() -> Result<Self, TokenizerError> {
        Tokenizer::from_bytes(TOKENIZER_JSON)
            .map(|inner| Self { inner })
            .map_err(|error| TokenizerError::Load(error.to_string()))
    }

    pub fn count(&self, text: &str) -> Result<u64, TokenizerError> {
        self.inner
            .encode(text, false)
            .map(|encoding| encoding.len() as u64)
            .map_err(|error| TokenizerError::Encode(error.to_string()))
    }

    pub fn count_usage(
        &self,
        serialized_prompt: &str,
        completion: &str,
    ) -> Result<TokenCount, TokenizerError> {
        let prompt_tokens = self.count(serialized_prompt)?;
        let completion_tokens = self.count(completion)?;
        Ok(TokenCount {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        })
    }

    pub fn validate_usage(
        &self,
        serialized_prompt: &str,
        completion: &str,
        reported: &Usage,
    ) -> Result<TokenCount, UsageMismatch> {
        let expected = self
            .count_usage(serialized_prompt, completion)
            .map_err(UsageMismatch::Tokenizer)?;
        let reported_count = TokenCount {
            prompt_tokens: reported.prompt_tokens,
            completion_tokens: reported.completion_tokens,
            total_tokens: reported.total_tokens,
        };
        if reported.total_tokens != reported.prompt_tokens + reported.completion_tokens
            || reported_count != expected
        {
            return Err(UsageMismatch::Counts {
                expected,
                reported: reported_count,
            });
        }
        Ok(expected)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenCount {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenizerError {
    #[error("embedded DeepSeek tokenizer could not be loaded: {0}")]
    Load(String),
    #[error("DeepSeek tokenization failed: {0}")]
    Encode(String),
}

#[derive(Debug, thiserror::Error)]
pub enum UsageMismatch {
    #[error(transparent)]
    Tokenizer(TokenizerError),
    #[error("DeepSeek usage mismatch: expected {expected:?}, reported {reported:?}")]
    Counts {
        expected: TokenCount,
        reported: TokenCount,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_v3_tokenizer_has_stable_counts() {
        let tokenizer = DeepSeekTokenizer::from_embedded().unwrap();
        assert_eq!(tokenizer.count("Hello!").unwrap(), 2);
        assert_eq!(tokenizer.count("").unwrap(), 0);
    }

    #[test]
    fn validates_all_usage_fields() {
        let tokenizer = DeepSeekTokenizer::from_embedded().unwrap();
        let expected = tokenizer.count_usage("Hello!", "Hi!").unwrap();
        let usage = Usage {
            prompt_tokens: expected.prompt_tokens,
            completion_tokens: expected.completion_tokens,
            total_tokens: expected.total_tokens,
            ..Usage::default()
        };
        assert_eq!(
            tokenizer.validate_usage("Hello!", "Hi!", &usage).unwrap(),
            expected
        );

        let invalid = Usage {
            total_tokens: usage.total_tokens + 1,
            ..usage
        };
        assert!(matches!(
            tokenizer.validate_usage("Hello!", "Hi!", &invalid),
            Err(UsageMismatch::Counts { .. })
        ));
    }
}
