use std::error::Error;

pub trait Backend {
    fn name(&self) -> &str;
    fn reply(&self, prompt: &str, system: &str) -> Result<String, Box<dyn Error>>;
}

pub struct MockBackend;

impl MockBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Backend for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }

    fn reply(&self, prompt: &str, system: &str) -> Result<String, Box<dyn Error>> {
        let words = prompt.split_whitespace().count();
        let plural = if words == 1 { "" } else { "s" };
        let sys_hint = if system.is_empty() {
            String::new()
        } else {
            let first_line = system.lines().next().unwrap_or("");
            format!(" [system: {first_line:?}]")
        };
        Ok(format!(
            "(mock){sys_hint} heard {words} word{plural}: {prompt:?}"
        ))
    }
}

pub struct AnthropicBackend {
    api_key: String,
    model: String,
}

impl AnthropicBackend {
    pub fn from_env() -> Result<Self, Box<dyn Error>> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY not set")?;
        let model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());
        Ok(Self { api_key, model })
    }

    /// Like `from_env` but pins a specific model, ignoring ANTHROPIC_MODEL. Used
    /// by the policy broker so the cheap, fast grader is always Haiku regardless
    /// of whatever heavier model the session's fallback backend uses.
    pub fn from_env_with_model(model: &str) -> Result<Self, Box<dyn Error>> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY not set")?;
        Ok(Self {
            api_key,
            model: model.to_string(),
        })
    }
}

impl Backend for AnthropicBackend {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn reply(&self, prompt: &str, system: &str) -> Result<String, Box<dyn Error>> {
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": prompt }],
        });
        if !system.is_empty() {
            body["system"] = serde_json::Value::String(system.to_string());
        }

        let resp = ureq::post("https://api.anthropic.com/v1/messages")
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_json(body)?;

        let v: serde_json::Value = resp.into_json()?;
        let text = v
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            })
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("(no text block in response)")
            .to_string();
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_backend_name() {
        assert_eq!(MockBackend::new().name(), "mock");
    }

    #[test]
    fn mock_backend_reports_word_count_singular() {
        let reply = MockBackend::new().reply("hello", "").unwrap();
        assert!(reply.contains("1 word:"), "got: {reply}");
        assert!(reply.contains("\"hello\""), "got: {reply}");
    }

    #[test]
    fn mock_backend_reports_word_count_plural() {
        let reply = MockBackend::new().reply("hello there friend", "").unwrap();
        assert!(reply.contains("3 words:"), "got: {reply}");
    }

    #[test]
    fn mock_backend_includes_system_hint() {
        let reply = MockBackend::new()
            .reply("hi", "Be terse.")
            .unwrap();
        assert!(reply.contains("[system:"), "got: {reply}");
        assert!(reply.contains("Be terse."), "got: {reply}");
    }

    #[test]
    fn anthropic_from_env_requires_api_key() {
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");
        let result = AnthropicBackend::from_env();
        if let Some(v) = prev {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
        assert!(result.is_err());
    }
}
