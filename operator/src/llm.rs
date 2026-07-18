use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use openhpa_core::llm::{build_prompt, parse_llm_output};
use openhpa_core::{
    Candidate, LlmAnalysisOutput, LlmRecommendationItem, RiskLevel, WorkloadConfig,
};
use serde_json::json;

const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Total tries per request (1 initial + retries) for 429/5xx responses. With the per-request
/// timeout this bounds one LLM call to `MAX_ATTEMPTS x timeout` plus ~2s of backoff.
const MAX_ATTEMPTS: u32 = 3;
const BACKOFF_BASE_MS: u64 = 500;
/// How much of a provider error body to carry into the error message.
const ERROR_BODY_MAX_CHARS: usize = 300;

/// LLM backend the operator calls for analysis. `RulesOnly` skips the LLM entirely (air-gapped /
/// no external model). Bedrock (VPC endpoint) is a planned variant - not yet wired. `base_url` is
/// configurable so the backend can point at an in-cluster proxy, a local-model sidecar, or a mock.
pub enum LlmBackend {
    OpenAi { client: reqwest::Client, api_key: String, model: String, base_url: String },
    Anthropic { client: reqwest::Client, api_key: String, model: String, base_url: String },
    RulesOnly,
}

// Manual Debug that never prints the API key, so a stray `?`-format of the backend (or of any
// struct that embeds it) can never leak the key into logs. The key is intentionally omitted.
impl std::fmt::Debug for LlmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenAi { model, base_url, .. } => f
                .debug_struct("OpenAi")
                .field("model", model)
                .field("base_url", base_url)
                .field("api_key", &"<redacted>")
                .finish(),
            Self::Anthropic { model, base_url, .. } => f
                .debug_struct("Anthropic")
                .field("model", model)
                .field("base_url", base_url)
                .field("api_key", &"<redacted>")
                .finish(),
            Self::RulesOnly => f.write_str("RulesOnly"),
        }
    }
}

impl LlmBackend {
    /// Build a backend. `base_url` overrides the provider's real endpoint (empty/`None` keeps it);
    /// `timeout` bounds each request so a stalled model can't stretch a reconcile pass.
    pub fn from_config(
        provider: &str,
        model: &str,
        api_key: Option<String>,
        base_url: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        let base_url =
            base_url.map(|u| u.trim().trim_end_matches('/').to_owned()).filter(|u| !u.is_empty());
        match provider {
            "openai" => Ok(Self::OpenAi {
                client: http_client(timeout)?,
                api_key: api_key.context("llm provider 'openai' requires an API key")?,
                model: model.to_owned(),
                base_url: base_url.unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.to_owned()),
            }),
            "anthropic" => Ok(Self::Anthropic {
                client: http_client(timeout)?,
                api_key: api_key.context("llm provider 'anthropic' requires an API key")?,
                model: model.to_owned(),
                base_url: base_url.unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_owned()),
            }),
            "none" => Ok(Self::RulesOnly),
            other => bail!("unknown llm provider: {other} (expected openai|anthropic|none)"),
        }
    }

    /// Run the LLM judgement over the candidates, or synthesize a rules-only output when no model.
    pub async fn analyze(
        &self,
        workload_desc: &str,
        config: &WorkloadConfig,
        candidates: &[Candidate],
        cost_per_replica_usd_monthly: f64,
    ) -> Result<LlmAnalysisOutput> {
        let (system, user) =
            build_prompt(workload_desc, config, candidates, cost_per_replica_usd_monthly);
        match self.complete(&system, &user).await? {
            Some(text) => parse_llm_output(&text).context("parsing LLM output"),
            None => {
                Ok(rules_only_output(candidates, "Rule-based recommendation (no LLM configured)."))
            }
        }
    }

    async fn complete(&self, system: &str, user: &str) -> Result<Option<String>> {
        match self {
            Self::RulesOnly => Ok(None),
            Self::OpenAi { client, api_key, model, base_url } => {
                let body = json!({
                    "model": model,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": user},
                    ],
                });
                let request = client
                    .post(format!("{base_url}/v1/chat/completions"))
                    .bearer_auth(api_key)
                    .json(&body);
                let value = send_with_retry(request, "openai").await?;
                let text = value["choices"][0]["message"]["content"]
                    .as_str()
                    .context("openai response had no message content")?;
                Ok(Some(text.to_owned()))
            }
            Self::Anthropic { client, api_key, model, base_url } => {
                let body = json!({
                    "model": model,
                    "max_tokens": 1024,
                    "system": system,
                    "messages": [{"role": "user", "content": user}],
                });
                let request = client
                    .post(format!("{base_url}/v1/messages"))
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body);
                let value = send_with_retry(request, "anthropic").await?;
                let text = value["content"][0]["text"]
                    .as_str()
                    .context("anthropic response had no text content")?;
                Ok(Some(text.to_owned()))
            }
        }
    }
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder().timeout(timeout).build().context("building LLM HTTP client")
}

/// Send the request, retrying 429/5xx with bounded exponential backoff. Any other non-2xx fails
/// immediately. The error carries (a truncated slice of) the provider's response body, which is
/// what distinguishes e.g. a transient rate limit from an exhausted billing quota. Transport
/// errors (including the per-request timeout) are not retried, so one call stays bounded by
/// `MAX_ATTEMPTS x timeout` plus backoff.
async fn send_with_retry(
    request: reqwest::RequestBuilder,
    provider: &str,
) -> Result<serde_json::Value> {
    let mut attempt = 1;
    loop {
        let response = request
            .try_clone()
            .context("cloning LLM request for retry")?
            .send()
            .await
            .with_context(|| format!("sending {provider} API request"))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json()
                .await
                .with_context(|| format!("decoding {provider} API response"));
        }
        let body: String =
            response.text().await.unwrap_or_default().chars().take(ERROR_BODY_MAX_CHARS).collect();
        let retryable =
            status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        if !retryable || attempt >= MAX_ATTEMPTS {
            bail!("{provider} API returned {status}: {body}");
        }
        tokio::time::sleep(backoff(attempt)).await;
        attempt += 1;
    }
}

/// Exponential backoff with sub-250ms clock-derived jitter (no RNG dependency needed).
fn backoff(attempt: u32) -> Duration {
    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::from(elapsed.subsec_nanos()) % 250);
    Duration::from_millis(BACKOFF_BASE_MS * 2u64.pow(attempt - 1) + jitter_ms)
}

/// Deterministic output when no LLM is configured: apply every detected candidate, low risk, with
/// `executive_summary` naming the reason.
fn rules_only_output(candidates: &[Candidate], executive_summary: &str) -> LlmAnalysisOutput {
    let recommendations = candidates
        .iter()
        .map(|candidate| LlmRecommendationItem {
            candidate_kind: candidate.kind,
            apply: true,
            reasoning: candidate.description.clone(),
            risk: RiskLevel::Low,
            projected_savings_usd_monthly: None,
        })
        .collect();
    LlmAnalysisOutput {
        recommendations,
        overall_risk: RiskLevel::Low,
        executive_summary: executive_summary.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use openhpa_core::CandidateKind;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn workload_config() -> WorkloadConfig {
        WorkloadConfig {
            min_replicas: 10,
            max_replicas: 40,
            target_cpu_pct: 70,
            scale_down_cooldown_s: 300,
        }
    }

    fn mock_backend(base_url: &str) -> LlmBackend {
        LlmBackend::from_config(
            "openai",
            "gpt-4o-mini",
            Some("test-key".to_owned()),
            Some(base_url.to_owned()),
            Duration::from_secs(5),
        )
        .expect("openai backend builds")
    }

    /// A minimal valid chat completion whose content parses as `LlmAnalysisOutput`.
    fn verdict_completion() -> serde_json::Value {
        let verdict = json!({
            "recommendations": [],
            "overall_risk": "low",
            "executive_summary": "ok"
        })
        .to_string();
        json!({"choices": [{"message": {"role": "assistant", "content": verdict}}]})
    }

    #[tokio::test]
    async fn a_429_is_retried_and_the_call_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(verdict_completion()))
            .expect(1)
            .mount(&server)
            .await;

        let out = mock_backend(&server.uri())
            .analyze("ns/web", &workload_config(), &[], 30.0)
            .await
            .unwrap();
        assert_eq!(out.executive_summary, "ok");
    }

    #[tokio::test]
    async fn a_5xx_is_retried_and_the_call_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(verdict_completion()))
            .expect(1)
            .mount(&server)
            .await;

        let out = mock_backend(&server.uri())
            .analyze("ns/web", &workload_config(), &[], 30.0)
            .await
            .unwrap();
        assert_eq!(out.executive_summary, "ok");
    }

    #[tokio::test]
    async fn a_persistent_429_stops_after_bounded_attempts_and_surfaces_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "error": {"message": "insufficient_quota: billing hard limit reached", "type": "insufficient_quota"}
            })))
            .expect(u64::from(MAX_ATTEMPTS))
            .mount(&server)
            .await;

        let err = mock_backend(&server.uri())
            .analyze("ns/web", &workload_config(), &[], 30.0)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("insufficient_quota: billing hard limit reached"),
            "the provider error body must be surfaced: {msg}"
        );
        assert!(msg.contains("429"), "the status must be surfaced: {msg}");
    }

    #[tokio::test]
    async fn a_non_429_4xx_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": {"message": "model not found"}})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let err = mock_backend(&server.uri())
            .analyze("ns/web", &workload_config(), &[], 30.0)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("model not found"), "the provider error body must be surfaced: {msg}");
    }

    #[test]
    fn rules_only_applies_all_candidates() {
        let candidates = vec![Candidate {
            kind: CandidateKind::Overprovisioned,
            description: "p95 low".to_owned(),
            evidence: BTreeMap::new(),
            proposed_diff: BTreeMap::new(),
            schedule: None,
        }];
        let out = rules_only_output(&candidates, "Rule-based recommendation (no LLM configured).");
        assert_eq!(out.recommendations.len(), 1);
        assert!(out.recommendations[0].apply);
        assert_eq!(out.recommendations[0].candidate_kind, CandidateKind::Overprovisioned);
    }

    #[test]
    fn from_config_rejects_unknown_provider() {
        let err =
            LlmBackend::from_config("grok", "x", None, None, Duration::from_secs(30)).unwrap_err();
        assert!(err.to_string().contains("unknown llm provider"));
    }

    #[test]
    fn openai_requires_key() {
        let err =
            LlmBackend::from_config("openai", "gpt-4o-mini", None, None, Duration::from_secs(30))
                .unwrap_err();
        assert!(err.to_string().contains("requires an API key"));
    }

    #[test]
    fn debug_never_leaks_the_api_key() {
        // A stray `?`-format of the backend (or anything embedding it) must not print the key.
        let backend = LlmBackend::from_config(
            "anthropic",
            "claude",
            Some("super-secret-key".to_owned()),
            None,
            Duration::from_secs(5),
        )
        .expect("anthropic backend builds");
        let rendered = format!("{backend:?}");
        assert!(!rendered.contains("super-secret-key"), "api key leaked into Debug: {rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn openai_uses_the_configured_base_url_trimmed() {
        let backend = LlmBackend::from_config(
            "openai",
            "gpt-4o-mini",
            Some("k".to_owned()),
            Some("http://127.0.0.1:9999/".to_owned()),
            Duration::from_secs(5),
        )
        .unwrap();
        let LlmBackend::OpenAi { base_url, .. } = backend else { panic!("expected OpenAi") };
        assert_eq!(base_url, "http://127.0.0.1:9999");
    }

    #[test]
    fn backends_default_to_the_real_api_base_urls() {
        let openai = LlmBackend::from_config(
            "openai",
            "m",
            Some("k".to_owned()),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        let LlmBackend::OpenAi { base_url, .. } = openai else { panic!("expected OpenAi") };
        assert_eq!(base_url, "https://api.openai.com");

        // An empty override falls back to the default rather than producing a malformed URL.
        let anthropic = LlmBackend::from_config(
            "anthropic",
            "m",
            Some("k".to_owned()),
            Some("  ".to_owned()),
            Duration::from_secs(5),
        )
        .unwrap();
        let LlmBackend::Anthropic { base_url, .. } = anthropic else {
            panic!("expected Anthropic")
        };
        assert_eq!(base_url, "https://api.anthropic.com");
    }
}
