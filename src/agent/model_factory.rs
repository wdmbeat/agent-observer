//! Placeholder port of `agent/model_factory.py` — environment-driven LLM
//! chat-model construction.
//!
//! NOT IMPLEMENTED: the deterministic pipeline never calls into this module.
//! It exists to keep the Python↔Rust file mapping complete and to mark where
//! LLM support would plug in. In the Python agent, `decision_graph.py`'s
//! `_model_node` consults the chat model built here between `_prepare` and
//! `_finalize`; porting that path means: implement `build_chat_model` (e.g.
//! over an OpenAI-compatible HTTP API), add an `Llm` variant to the
//! `Selector` seam in `decision_graph.rs`, and wire `ModelSettings` into
//! agent startup. The Python version also needs `agent/requirements.txt`
//! (langchain-* packages); a Rust port would use `reqwest` + `serde_json`
//! instead.

use std::collections::HashMap;
use std::fmt;

/// Raised when an optional model provider is only partially configured.
#[derive(Debug)]
pub struct ModelConfigurationError(pub String);

impl fmt::Display for ModelConfigurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "model configuration error: {}", self.0)
    }
}

impl std::error::Error for ModelConfigurationError {}

/// Provider aliases from the Python kit (`MODEL_PROVIDER` env var).
pub fn provider_aliases() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("chatgpt", "openai"),
        ("claude", "anthropic"),
        ("grok", "xai"),
        ("glm", "zai"),
        ("kimi", "moonshot"),
        ("qwen", "dashscope"),
    ])
}

/// Which env var holds each provider's API key.
pub fn provider_key_env() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("openai", "OPENAI_API_KEY"),
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("xai", "XAI_API_KEY"),
        ("zai", "ZAI_API_KEY"),
        ("deepseek", "DEEPSEEK_API_KEY"),
        ("moonshot", "MOONSHOT_API_KEY"),
        ("dashscope", "DASHSCOPE_API_KEY"),
        ("minimax", "MINIMAX_API_KEY"),
    ])
}

/// Secret-safe provider settings, mirroring the Python dataclass.
/// The manual `Debug` impl omits `api_key`, like Python's `field(repr=False)`.
#[derive(Clone)]
pub struct ModelSettings {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_mode: String,
    pub timeout_seconds: f64,
    pub max_retries: u32,
    pub top_k_candidates: u32,
}

impl fmt::Debug for ModelSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelSettings")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_mode", &self.api_mode)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("max_retries", &self.max_retries)
            .field("top_k_candidates", &self.top_k_candidates)
            .finish_non_exhaustive()
    }
}

impl ModelSettings {
    /// Load settings from process environment variables
    /// (`MODEL_PROVIDER`, `MODEL_NAME`, `MODEL_BASE_URL`, `MODEL_API_KEY_ENV`,
    /// `MODEL_API_MODE`, `LLM_TIMEOUT_SECONDS`, `LLM_MAX_RETRIES`,
    /// `LLM_TOP_K_CANDIDATES`), matching the Python defaults.
    ///
    /// # Panics
    /// Always — this is a placeholder. See the module docs for the port plan.
    pub fn from_environment() -> Result<Self, ModelConfigurationError> {
        todo!("port ModelSettings.from_environment from agent/model_factory.py")
    }

    /// True when no LLM is configured (the default): `"" | "none" | "deterministic"`.
    pub fn is_deterministic(&self) -> bool {
        matches!(self.provider.as_str(), "" | "none" | "deterministic")
    }
}

/// Placeholder for the chat-model handle a future LLM selector would use.
/// The Python version returns a LangChain chat model; a Rust port would
/// return an HTTP client wrapper around an OpenAI-compatible endpoint.
pub struct ChatModel;

/// Build the chat model for `settings`.
///
/// # Panics
/// Always — this is a placeholder. See the module docs for the port plan.
pub fn build_chat_model(_settings: &ModelSettings) -> Result<ChatModel, ModelConfigurationError> {
    todo!("port build_chat_model from agent/model_factory.py (LangChain -> reqwest)")
}
