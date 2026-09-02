use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

pub trait LlmTransport {
    /// Returns the model's raw text response, or an error string.
    fn generate(&self, prompt: &str) -> Result<String, String>;
}

pub struct OllamaHttpTransport {
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

impl LlmTransport for OllamaHttpTransport {
    fn generate(&self, prompt: &str) -> Result<String, String> {
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(self.timeout_secs))
            .build();
        let body = json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "format": "json",
        })
        .to_string();
        let response = agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body)
            .map_err(|error| error.to_string())?;
        let body = response.into_string().map_err(|error| error.to_string())?;
        serde_json::from_str::<GenerateResponse>(&body)
            .map(|body| body.response)
            .map_err(|error| error.to_string())
    }
}

pub struct OllamaPlanner<T: LlmTransport> {
    transport: T,
    fallback: ubu_core::DeterministicPlacer,
}

impl<T: LlmTransport> OllamaPlanner<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            fallback: ubu_core::DeterministicPlacer,
        }
    }
}

impl<T: LlmTransport> ubu_core::Planner for OllamaPlanner<T> {
    fn place(&self, input: &ubu_core::PlacementInput) -> ubu_core::PlacementOutput {
        let _ = &self.transport;
        ubu_core::Planner::place(&self.fallback, input)
    }
}
