//! Port of `agent/model_factory.py` — environment-driven LLM chat-model
//! construction over an OpenAI-compatible chat-completions endpoint.
//!
//! The Python version returns a LangChain chat model; this port speaks HTTP
//! directly (`ureq`, blocking). Scope cuts versus Python: Anthropic's native
//! API and the OpenAI Responses API are rejected with a
//! `ModelConfigurationError` (chat completions only here).

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

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

/// Sent when the platform proxy is used without a model name; the proxy
/// replaces it with the model configured for the team on the Participate page.
pub const PLATFORM_MODEL_PLACEHOLDER: &str = "team-model";

/// `os.environ.get(key, "").strip()`.
fn env_trim(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
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
    pub max_retries: i64,
    pub top_k_candidates: i64,
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
    /// (`MODEL_PROVIDER`, `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_MODEL`,
    /// `MODEL_NAME`, `MODEL_BASE_URL`, `MODEL_API_KEY_ENV`, `MODEL_API_MODE`,
    /// `LLM_TIMEOUT_SECONDS`, `LLM_MAX_RETRIES`, `LLM_TOP_K_CANDIDATES`),
    /// matching the Python precedence exactly. Unlike Python, dotenv
    /// application happens in the caller (`minimal_agent::run`), so this
    /// function reads the live environment only.
    pub fn from_environment() -> Result<Self, ModelConfigurationError> {
        let aliases = provider_aliases();
        let key_env = provider_key_env();
        let provider =
            std::env::var("MODEL_PROVIDER").unwrap_or_else(|_| "deterministic".to_string());
        let provider = provider.trim().to_lowercase();
        let provider = match aliases.get(provider.as_str()) {
            Some(canonical) => canonical.to_string(),
            None => provider,
        };
        let key_name = {
            let custom = env_trim("MODEL_API_KEY_ENV");
            if custom.is_empty() {
                key_env
                    .get(provider.as_str())
                    .copied()
                    .unwrap_or("MODEL_API_KEY")
                    .to_string()
            } else {
                custom
            }
        };
        // OPENAI_BASE_URL/OPENAI_API_KEY first (what the platform injects), then the
        // provider-specific settings. Anthropic's native API is not OpenAI-compatible.
        let compatible_base = if provider != "anthropic" {
            env_trim("OPENAI_BASE_URL")
        } else {
            String::new()
        };
        let base_url = if compatible_base.is_empty() {
            env_trim("MODEL_BASE_URL")
        } else {
            compatible_base.clone()
        };
        let mut api_key = if compatible_base.is_empty() {
            String::new()
        } else {
            env_trim("OPENAI_API_KEY")
        };
        if api_key.is_empty() {
            api_key = env_trim(&key_name);
        }
        let mut model = env_trim("OPENAI_MODEL");
        if model.is_empty() {
            model = env_trim("MODEL_NAME");
        }
        if model.is_empty() && !compatible_base.is_empty() {
            model = PLATFORM_MODEL_PLACEHOLDER.to_string();
        }
        // The platform proxy speaks chat completions only.
        let default_mode = if provider == "openai" && compatible_base.is_empty() {
            "responses"
        } else {
            "chat"
        };
        let api_mode = std::env::var("MODEL_API_MODE")
            .unwrap_or_else(|_| default_mode.to_string())
            .trim()
            .to_lowercase();
        let parse_error = |name: &str, raw: &str| {
            ModelConfigurationError(format!("{name} must be a number, got {raw:?}"))
        };
        let raw = std::env::var("LLM_TIMEOUT_SECONDS").unwrap_or_else(|_| "30".to_string());
        let timeout_seconds: f64 = raw
            .trim()
            .parse()
            .map_err(|_| parse_error("LLM_TIMEOUT_SECONDS", &raw))?;
        let raw = std::env::var("LLM_MAX_RETRIES").unwrap_or_else(|_| "1".to_string());
        let max_retries: i64 = raw
            .trim()
            .parse()
            .map_err(|_| parse_error("LLM_MAX_RETRIES", &raw))?;
        let raw = std::env::var("LLM_TOP_K_CANDIDATES").unwrap_or_else(|_| "12".to_string());
        let top_k_candidates: i64 = raw
            .trim()
            .parse()
            .map_err(|_| parse_error("LLM_TOP_K_CANDIDATES", &raw))?;
        Ok(Self {
            provider,
            model,
            base_url,
            api_key,
            api_mode,
            timeout_seconds,
            max_retries,
            top_k_candidates,
        })
    }

    /// True when no LLM is configured (the default): `"" | "none" | "deterministic"`.
    pub fn is_deterministic(&self) -> bool {
        matches!(self.provider.as_str(), "" | "none" | "deterministic")
    }
}

/// A blocking chat-completions client for one configured endpoint — the
/// counterpart of the LangChain chat model `_model_node` invokes.
#[derive(Clone)]
pub struct ChatModel {
    settings: ModelSettings,
    agent: ureq::Agent,
}

impl ChatModel {
    fn new(settings: &ModelSettings) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs_f64(settings.timeout_seconds))
            .build();
        Self { settings: settings.clone(), agent }
    }

    /// One system+user exchange; returns the message text (`_extract_text`
    /// accepts both string and parts-array content). Retries transport and
    /// 5xx failures up to `max_retries` times.
    pub fn invoke(&self, system: &str, user: &str) -> Result<String> {
        let base_url = if self.settings.base_url.is_empty() {
            "https://api.openai.com/v1"
        } else {
            self.settings.base_url.trim_end_matches('/')
        };
        let url = format!("{base_url}/chat/completions");
        let body = json!({
            "model": self.settings.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let attempts = 1 + self.settings.max_retries.max(0) as u32;
        let mut last_error: Option<anyhow::Error> = None;
        for _ in 0..attempts {
            let result = self
                .agent
                .post(&url)
                .set(
                    "Authorization",
                    &format!("Bearer {}", self.settings.api_key),
                )
                .send_json(&body);
            match result {
                Ok(response) => {
                    let payload: Value =
                        response.into_json().context("model response is not valid JSON")?;
                    return message_text(&payload);
                }
                Err(ureq::Error::Status(code, response)) => {
                    let detail = response.into_string().unwrap_or_default();
                    let error = anyhow::anyhow!("model request failed with HTTP {code}: {detail}");
                    if code >= 500 {
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
                Err(ureq::Error::Transport(transport)) => {
                    last_error = Some(anyhow::anyhow!("model request failed: {transport}"));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("model request failed")))
    }
}

/// `decision_graph.py::_extract_text` over `choices[0].message.content`.
fn message_text(payload: &Value) -> Result<String> {
    let Some(content) = payload.pointer("/choices/0/message/content") else {
        bail!("model response has no choices[0].message.content: {payload}");
    };
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(items) => {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|item| item.as_str().or_else(|| item.get("text").and_then(Value::as_str)))
                .collect();
            Ok(parts.join("\n"))
        }
        _ => bail!("model response content is not text: {content}"),
    }
}

/// Build the chat model for `settings`; `Ok(None)` when deterministic.
pub fn build_chat_model(
    settings: &ModelSettings,
) -> Result<Option<ChatModel>, ModelConfigurationError> {
    if settings.is_deterministic() {
        return Ok(None);
    }
    if settings.model.is_empty() || settings.api_key.is_empty() {
        return Err(ModelConfigurationError(format!(
            "{} requires a model name (OPENAI_MODEL or MODEL_NAME) and an API key (OPENAI_API_KEY or its provider key)",
            settings.provider
        )));
    }
    if settings.timeout_seconds <= 0.0 || settings.max_retries < 0 {
        return Err(ModelConfigurationError(
            "timeout must be positive and retries non-negative".to_string(),
        ));
    }
    if settings.top_k_candidates < 1 {
        return Err(ModelConfigurationError(
            "LLM_TOP_K_CANDIDATES must be positive".to_string(),
        ));
    }
    if settings.provider == "anthropic" {
        return Err(ModelConfigurationError(
            "Anthropic's native API is not OpenAI-compatible; use an OpenAI-compatible endpoint"
                .to_string(),
        ));
    }
    if settings.provider != "openai" && settings.base_url.is_empty() {
        return Err(ModelConfigurationError(format!(
            "{} requires OPENAI_BASE_URL or MODEL_BASE_URL for its compatible endpoint",
            settings.provider
        )));
    }
    if settings.api_mode != "chat" {
        return Err(ModelConfigurationError(
            "this port speaks chat completions only; set MODEL_API_MODE=chat".to_string(),
        ));
    }
    Ok(Some(ChatModel::new(settings)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::Arc;

    /// Every env var `from_environment` may read, plus this test's own key.
    const ENV_KEYS: [&str; 22] = [
        "MODEL_PROVIDER",
        "MODEL_NAME",
        "MODEL_BASE_URL",
        "MODEL_API_KEY_ENV",
        "MODEL_API_MODE",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_MODEL",
        "ANTHROPIC_API_KEY",
        "XAI_API_KEY",
        "ZAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "MOONSHOT_API_KEY",
        "DASHSCOPE_API_KEY",
        "MINIMAX_API_KEY",
        "MODEL_API_KEY",
        "LLM_TIMEOUT_SECONDS",
        "LLM_MAX_RETRIES",
        "LLM_TOP_K_CANDIDATES",
        "SAC_AGENT_TEST_CUSTOM_KEY",
        "SAC_AGENT_TEST_SPARE_A",
        "SAC_AGENT_TEST_SPARE_B",
    ];

    /// Restores the process environment on drop (tests run multi-threaded, so
    /// all env-touching assertions stay in this one test function).
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn capture() -> Self {
            Self(ENV_KEYS.iter().map(|key| (*key, std::env::var_os(key))).collect())
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn from_environment_mirrors_python_precedence() {
        let _guard = EnvGuard::capture();
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }

        // Defaults: deterministic, chat mode, Python's numeric defaults.
        let settings = ModelSettings::from_environment().unwrap();
        assert!(settings.is_deterministic());
        assert_eq!(settings.provider, "deterministic");
        assert_eq!(settings.model, "");
        assert_eq!(settings.base_url, "");
        assert_eq!(settings.api_key, "");
        assert_eq!(settings.api_mode, "chat");
        assert_eq!(settings.timeout_seconds, 30.0);
        assert_eq!(settings.max_retries, 1);
        assert_eq!(settings.top_k_candidates, 12);

        // Provider alias; native OpenAI defaults to the Responses API.
        std::env::set_var("MODEL_PROVIDER", " ChatGPT ");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.provider, "openai");
        assert_eq!(settings.api_mode, "responses");
        assert_eq!(settings.api_key, "");
        // The provider key env supplies the key without a platform proxy.
        std::env::set_var("OPENAI_API_KEY", "sac-agent-test-direct-key");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.api_key, "sac-agent-test-direct-key");

        // The platform proxy takes precedence over MODEL_* settings, and the
        // proxy speaks chat completions only.
        std::env::set_var("OPENAI_BASE_URL", "http://sac-agent-test-proxy.invalid/v1");
        std::env::set_var("MODEL_NAME", "sac-agent-test-model-name");
        std::env::set_var("MODEL_BASE_URL", "http://sac-agent-test-fallback.invalid/v1");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.base_url, "http://sac-agent-test-proxy.invalid/v1");
        assert_eq!(settings.api_key, "sac-agent-test-direct-key");
        assert_eq!(settings.model, "sac-agent-test-model-name");
        assert_eq!(settings.api_mode, "chat");

        // OPENAI_MODEL beats MODEL_NAME; with neither, the placeholder rides
        // the platform proxy.
        std::env::set_var("OPENAI_MODEL", "sac-agent-test-openai-model");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.model, "sac-agent-test-openai-model");
        std::env::remove_var("OPENAI_MODEL");
        std::env::remove_var("MODEL_NAME");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.model, PLATFORM_MODEL_PLACEHOLDER);

        // Anthropic ignores the platform proxy: MODEL_BASE_URL and its own
        // key env apply instead.
        std::env::set_var("MODEL_PROVIDER", "claude");
        std::env::set_var("ANTHROPIC_API_KEY", "sac-agent-test-anthropic-key");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.provider, "anthropic");
        assert_eq!(settings.base_url, "http://sac-agent-test-fallback.invalid/v1");
        assert_eq!(settings.api_key, "sac-agent-test-anthropic-key");

        // MODEL_API_KEY_ENV overrides the provider's key env.
        std::env::set_var("MODEL_PROVIDER", "kimi");
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::set_var("MODEL_API_KEY_ENV", "SAC_AGENT_TEST_CUSTOM_KEY");
        std::env::set_var("SAC_AGENT_TEST_CUSTOM_KEY", "sac-agent-test-custom-key");
        std::env::set_var("MOONSHOT_API_KEY", "sac-agent-test-moonshot-key");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.provider, "moonshot");
        assert_eq!(settings.api_key, "sac-agent-test-custom-key");
        std::env::remove_var("MODEL_API_KEY_ENV");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.api_key, "sac-agent-test-moonshot-key");

        // LLM_* tuning overrides (whitespace-tolerant, like float()/int()).
        std::env::set_var("LLM_TIMEOUT_SECONDS", " 7.5 ");
        std::env::set_var("LLM_MAX_RETRIES", "3");
        std::env::set_var("LLM_TOP_K_CANDIDATES", "5");
        let settings = ModelSettings::from_environment().unwrap();
        assert_eq!(settings.timeout_seconds, 7.5);
        assert_eq!(settings.max_retries, 3);
        assert_eq!(settings.top_k_candidates, 5);

        // Unparseable tuning values are configuration errors (Python ValueError).
        std::env::set_var("LLM_TIMEOUT_SECONDS", "soon");
        assert!(ModelSettings::from_environment().is_err());
    }

    fn test_settings(base_url: &str) -> ModelSettings {
        ModelSettings {
            provider: "openai".to_string(),
            model: "sac-agent-test-model".to_string(),
            base_url: base_url.to_string(),
            api_key: "sac-agent-test-key".to_string(),
            api_mode: "chat".to_string(),
            timeout_seconds: 5.0,
            max_retries: 0,
            top_k_candidates: 12,
        }
    }

    #[test]
    fn build_chat_model_validates_like_python() {
        let mut settings = test_settings("");
        settings.provider = "deterministic".to_string();
        assert!(build_chat_model(&settings).unwrap().is_none());

        let mut settings = test_settings("");
        settings.model.clear();
        assert!(build_chat_model(&settings).err().unwrap().0.contains("requires a model name"));
        let mut settings = test_settings("");
        settings.api_key.clear();
        assert!(build_chat_model(&settings).err().unwrap().0.contains("requires a model name"));

        let mut settings = test_settings("");
        settings.timeout_seconds = 0.0;
        assert!(build_chat_model(&settings).err().unwrap().0.contains("timeout must be positive"));
        let mut settings = test_settings("");
        settings.max_retries = -1;
        assert!(build_chat_model(&settings).err().unwrap().0.contains("timeout must be positive"));
        let mut settings = test_settings("");
        settings.top_k_candidates = 0;
        assert!(build_chat_model(&settings).err().unwrap().0.contains("LLM_TOP_K_CANDIDATES"));

        let mut settings = test_settings("");
        settings.provider = "anthropic".to_string();
        assert!(build_chat_model(&settings).err().unwrap().0.contains("not OpenAI-compatible"));

        let mut settings = test_settings("");
        settings.provider = "xai".to_string();
        assert!(build_chat_model(&settings).err().unwrap().0.contains("requires OPENAI_BASE_URL"));

        let mut settings = test_settings("");
        settings.api_mode = "responses".to_string();
        assert!(build_chat_model(&settings).err().unwrap().0.contains("chat completions"));

        assert!(build_chat_model(&test_settings("")).unwrap().is_some());
    }

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(head_end) = find_subslice(&buffer, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..head_end]).to_lowercase();
                let content_length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                if buffer.len() >= head_end + 4 + content_length {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    /// Serve `respond(request) ` to every connection on an ephemeral port;
    /// returns the base URL and a channel receiving each raw request.
    fn canned_server(
        respond: impl Fn(&str) -> String + Send + 'static,
    ) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let request = read_request(&mut stream);
                let _ = tx.send(request.clone());
                if stream.write_all(respond(&request).as_bytes()).is_err() {
                    break;
                }
            }
        });
        (format!("http://{address}"), rx)
    }

    #[test]
    fn invoke_posts_chat_completion_and_returns_string_content() {
        let (url, requests) = canned_server(|_| {
            http_response(
                "200 OK",
                &json!({"choices": [{"message": {"content": "observe TILE-1"}}]}).to_string(),
            )
        });
        let model = build_chat_model(&test_settings(&url)).unwrap().unwrap();
        let text = model.invoke("system prompt", "user prompt").unwrap();
        assert_eq!(text, "observe TILE-1");
        let request = requests.recv().unwrap();
        assert!(request.starts_with("POST /chat/completions "));
        let lowered = request.to_lowercase();
        assert!(lowered.contains("authorization: bearer sac-agent-test-key"));
        assert!(request.contains("\"model\":\"sac-agent-test-model\""));
        assert!(request.contains("\"content\":\"system prompt\",\"role\":\"system\""));
    }

    #[test]
    fn invoke_joins_parts_array_content() {
        let (url, _requests) = canned_server(|_| {
            http_response(
                "200 OK",
                &json!({"choices": [{"message": {"content": [
                    {"type": "text", "text": "line one"},
                    {"type": "text", "text": "line two"},
                ]}}]})
                .to_string(),
            )
        });
        let model = build_chat_model(&test_settings(&url)).unwrap().unwrap();
        assert_eq!(model.invoke("s", "u").unwrap(), "line one\nline two");
    }

    #[test]
    fn invoke_surfaces_provider_error_body() {
        let (url, _requests) = canned_server(|_| {
            http_response("500 Internal Server Error", r#"{"error":{"message":"upstream boom"}}"#)
        });
        let model = build_chat_model(&test_settings(&url)).unwrap().unwrap();
        let error = model.invoke("s", "u").unwrap_err();
        assert!(error.to_string().contains("HTTP 500"));
        assert!(error.to_string().contains("upstream boom"));
    }

    #[test]
    fn invoke_retries_server_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = Arc::clone(&attempts);
        let (url, _requests) = canned_server(move |_| {
            if server_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                http_response("503 Service Unavailable", "{}")
            } else {
                http_response(
                    "200 OK",
                    &json!({"choices": [{"message": {"content": "recovered"}}]}).to_string(),
                )
            }
        });
        let mut settings = test_settings(&url);
        settings.max_retries = 1;
        let model = build_chat_model(&settings).unwrap().unwrap();
        assert_eq!(model.invoke("s", "u").unwrap(), "recovered");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
