use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::api::{query_network, query_processes, query_services};
use crate::events::{recent_events, EventRecord};
use crate::models::{NetworkConnection, Process, Service};

// ---------------------------------------------------------------------
// Shared context gathering — used by BOTH diagnosis modes
// ---------------------------------------------------------------------

/// Everything a diagnosis is allowed to cite. Both `diagnose_rule_based`
/// and `AnthropicClient::diagnose` see exactly this and nothing else —
/// gathered once, here, before either mode runs — which is what makes
/// "cite only retrieved data" a structural property of this design
/// rather than just an instruction one of the two modes might ignore.
///
/// `recent_events` is scoped to `agent_id` when one is given (via
/// `events::recent_events`'s existing agent_id+since combination — see
/// the Phase 4 bugfix that made this actually work correctly), since
/// "what has this agent been doing" is a meaningful, agent-specific
/// question. Process/network/service state is inherently system-wide,
/// not agent-specific, so those three stay unscoped regardless of
/// `agent_id`.
///
/// Deliberate design choice, stated plainly: gathering this context does
/// NOT go through Phase 2's permission checks or Phase 5's tool
/// registry, even though `agent_id` is accepted. It reads the same way
/// Phase 1's `GET /processes`/`/network`/`/services` and Phase 4's
/// `GET /events` already do — unrestricted behind the API key, not
/// gated per-agent. The real enforcement point stays exactly where it's
/// always been: if a diagnosis suggests an action and the caller goes on
/// to actually propose it (via `POST /actions` or `POST /mcp/tools/call`),
/// that step is fully permission-checked as always. Read-only
/// observability for a diagnosis isn't treated as more sensitive than
/// the existing admin-style read endpoints it's built on.
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticContext {
    pub top_processes_by_cpu: Vec<Process>,
    pub top_processes_by_memory: Vec<Process>,
    pub network_connections: Vec<NetworkConnection>,
    pub services: Vec<Service>,
    pub recent_events: Vec<EventRecord>,
    pub lookback_minutes: i64,
}

pub async fn gather_context(
    pool: &PgPool,
    agent_id: Option<Uuid>,
    lookback_minutes: i64,
) -> Result<DiagnosticContext, sqlx::Error> {
    let top_processes_by_cpu = query_processes(pool, "", Some("cpu"), 0, 10).await?;
    let top_processes_by_memory = query_processes(pool, "", None, 0, 10).await?;
    let network_connections = query_network(pool, 50).await?;
    let services = query_services(pool).await?;

    let since = Utc::now() - Duration::minutes(lookback_minutes);
    let recent_events = recent_events(pool, agent_id, Some(since), None, 50).await?;

    Ok(DiagnosticContext {
        top_processes_by_cpu,
        top_processes_by_memory,
        network_connections,
        services,
        recent_events,
        lookback_minutes,
    })
}

// ---------------------------------------------------------------------
// Mode, result, error types
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosisMode {
    RuleBased,
    Llm,
}

impl DiagnosisMode {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "rule_based" | "rule-based" | "rulebased" => Some(DiagnosisMode::RuleBased),
            "llm" => Some(DiagnosisMode::Llm),
            _ => None,
        }
    }
}

/// A diagnosis-suggested fix, deliberately NOT yet an `actions` row —
/// turning this into a real action still requires an explicit
/// `POST /actions` or `POST /mcp/tools/call` from the caller, which goes
/// through Phase 2/3's full permission-check and approval-gate flow
/// unchanged. This phase composes on top of that flow; it doesn't add a
/// second way to create an action.
#[derive(Debug, Clone, Serialize)]
pub struct SuggestedAction {
    pub action_type: String,
    pub parameters: Value,
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosisResult {
    pub mode: DiagnosisMode,
    pub summary: String,
    pub suggested_action: Option<SuggestedAction>,
    /// The context the diagnosis was grounded in, returned alongside the
    /// summary so a caller (or a human reviewing the response) can check
    /// the summary against the actual data instead of taking it on
    /// faith. This is the "cite only retrieved data" contract made
    /// checkable, not just asserted in a prompt.
    pub context: DiagnosticContext,
}

#[derive(Debug)]
pub enum DiagnosisError {
    Database(sqlx::Error),
    LlmNotConfigured,
    LlmRequestFailed(String),
    LlmResponseMalformed(String),
}

impl std::fmt::Display for DiagnosisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagnosisError::Database(e) => write!(f, "database error: {e}"),
            DiagnosisError::LlmNotConfigured => {
                write!(f, "LLM diagnosis mode was requested but is not configured")
            }
            DiagnosisError::LlmRequestFailed(msg) => write!(f, "LLM request failed: {msg}"),
            DiagnosisError::LlmResponseMalformed(msg) => {
                write!(f, "LLM response could not be parsed: {msg}")
            }
        }
    }
}

impl std::error::Error for DiagnosisError {}

// ---------------------------------------------------------------------
// Rule-based diagnosis — pure, deterministic, fully testable
// ---------------------------------------------------------------------

/// Process names that must never be auto-suggested for `process.stop`,
/// regardless of CPU usage — stopping any of these is far more likely to
/// take the whole system down than fix a slowdown. Matched
/// case-insensitively as a substring of the process name osquery
/// reports (see Phase 1's ingest), which is a heuristic on the reported
/// name, not a verified binary identity check — a process that renames
/// itself to dodge this list would not be caught. Good enough for a
/// deliberately simple rule-based mode; not a security control.
const NEVER_SUGGEST_STOPPING: &[&str] = &[
    "systemd",
    "init",
    "kernel",
    "kthreadd",
    "launchd",
    "postgres",
    "postgresql",
];

const HIGH_CPU_THRESHOLD_PERCENT: f64 = 80.0;

/// Deterministic diagnosis over pre-gathered context — no I/O, no
/// network, no external dependency. This is the mode that's always
/// available regardless of configuration, and the one every test in
/// this module can actually exercise end to end.
///
/// Three outcomes, in priority order, matching phase6-demo-loop.md §3's
/// retrieval sequence:
/// 1. A single process is clearly dominating CPU — the most common,
///    most confidently-diagnosable cause.
/// 2. No process dominates, but a recent `ActionFailed` event falls
///    within the lookback window — a different kind of explanation
///    (something this system did, not a runaway process).
/// 3. Neither applies: explicitly says so, rather than guessing. This
///    branch is tested as rigorously as the other two — the doc is
///    explicit that fabricating a cause when the data doesn't show one
///    is the actual failure mode here, not an acceptable fallback.
pub fn diagnose_rule_based(ctx: DiagnosticContext) -> DiagnosisResult {
    if let Some(top) = ctx.top_processes_by_cpu.first() {
        if let Some(cpu) = top.cpu_percent {
            if cpu >= HIGH_CPU_THRESHOLD_PERCENT {
                let pid = top.pid;
                let name = top.name.clone().unwrap_or_else(|| "<unknown>".to_string());
                let is_protected = NEVER_SUGGEST_STOPPING
                    .iter()
                    .any(|p| name.to_lowercase().contains(p));

                let summary = format!(
                    "Process '{name}' (pid {pid}) is using {cpu:.1}% CPU, well above the \
                     {HIGH_CPU_THRESHOLD_PERCENT:.0}% threshold — this looks like the primary \
                     driver of load right now."
                );

                let suggested_action = if is_protected {
                    None
                } else {
                    Some(SuggestedAction {
                        action_type: "process.stop".to_string(),
                        parameters: serde_json::json!({ "pid": pid }),
                        reasoning: format!("pid {pid} ('{name}') is consuming {cpu:.1}% CPU"),
                    })
                };

                return DiagnosisResult {
                    mode: DiagnosisMode::RuleBased,
                    summary,
                    suggested_action,
                    context: ctx,
                };
            }
        }
    }

    if let Some(failed) = ctx
        .recent_events
        .iter()
        .find(|e| e.event_type == "ActionFailed")
    {
        let event_id = failed.id;
        let lookback_minutes = ctx.lookback_minutes;
        return DiagnosisResult {
            mode: DiagnosisMode::RuleBased,
            summary: format!(
                "No single process is using unusually high CPU, but an action failed within the \
                 last {lookback_minutes} minutes (event {event_id}) — that may be related."
            ),
            suggested_action: None, // a failed action is not something to auto-retry blindly
            context: ctx,
        };
    }

    DiagnosisResult {
        mode: DiagnosisMode::RuleBased,
        summary: "No process is using unusually high CPU, and no recent action failures were \
                   found in the lookback window. Nothing in the gathered data points to a clear \
                   cause."
            .to_string(),
        suggested_action: None,
        context: ctx,
    }
}

// ---------------------------------------------------------------------
// LLM-backed diagnosis — two interchangeable providers behind one trait
// ---------------------------------------------------------------------
//
// IMPORTANT, stated plainly rather than discovered later: NEITHER
// provider below has ever run against its real backend. This
// environment has no network access, so nothing past "does it compile"
// has been verified for either one — not the request shape, not
// response parsing, not error handling under a real failure. Both were
// checked against current, real documentation via web search before
// being written (docs.anthropic.com for the direct client;
// docs.rs/otari and the otari-sdk-rust README/source for the gateway
// client) rather than relied on from memory — but "checked against
// docs" is not the same as "tested against a live endpoint." Treat both
// as real first drafts to validate with real credentials, not as proven
// code.

/// What both LLM-backed diagnosis providers implement. `AnthropicClient`
/// calls `api.anthropic.com` directly; `OtariClient` routes through a
/// self-hosted or platform Otari gateway (https://github.com/mozilla-ai/otari),
/// which can itself reach 40+ providers — this is what actually answers
/// "don't rely only on the Claude API," rather than the direct client
/// alone. `DiagnosisConfig::llm_client` holds whichever one is
/// configured as `Arc<dyn LlmBackend>`, so the rest of this module
/// (and `diagnose_api.rs`) never needs to know which provider is active.
#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn diagnose(&self, ctx: DiagnosticContext) -> Result<DiagnosisResult, DiagnosisError>;
}

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl LlmBackend for AnthropicClient {
    /// Sends the already-gathered context as a single grounded prompt
    /// and asks for a diagnosis.
    ///
    /// This is deliberately NOT an agentic tool-calling loop.
    /// phase6-demo-loop.md's architecture diagram shows the agent
    /// calling `query.processes`/`query.network`/`events` itself via
    /// tool access — a real multi-turn `tool_use`/`tool_result` exchange
    /// is substantially more engineering than this phase's scope, and
    /// it would be unverified in exactly the same way this simpler form
    /// already is, just with more surface area for something to be
    /// wrong. This simpler shape keeps the property that actually
    /// matters: the model only ever sees data this process already
    /// retrieved via the same trusted path the rule-based mode uses, so
    /// it structurally cannot cite something that was never really
    /// there. Upgrading to real tool-calling later replaces this
    /// function's body; it doesn't change anything else in this module.
    async fn diagnose(&self, ctx: DiagnosticContext) -> Result<DiagnosisResult, DiagnosisError> {
        let prompt = build_prompt(&ctx);

        let request = AnthropicRequest {
            model: &self.model,
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user",
                content: &prompt,
            }],
        };

        let response = self
            .http
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| DiagnosisError::LlmRequestFailed(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(DiagnosisError::LlmRequestFailed(format!(
                "HTTP {status}: {body}"
            )));
        }

        let parsed: AnthropicResponse = response
            .json()
            .await
            .map_err(|e| DiagnosisError::LlmResponseMalformed(e.to_string()))?;

        let text: String = parsed
            .content
            .iter()
            .filter(|b| b.block_type == "text")
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if text.trim().is_empty() {
            return Err(DiagnosisError::LlmResponseMalformed(
                "response contained no text content blocks".to_string(),
            ));
        }

        let suggested_action = extract_suggested_action(&text);
        let summary = strip_suggested_action_line(&text);

        Ok(DiagnosisResult {
            mode: DiagnosisMode::Llm,
            summary,
            suggested_action,
            context: ctx,
        })
    }
}

/// Routes diagnosis through a self-hosted or platform Otari gateway
/// (https://github.com/mozilla-ai/otari) instead of calling Anthropic
/// directly. The gateway itself can route to 40+ providers (Claude,
/// GPT, and others) behind one interface — this is what actually
/// answers "don't rely only on the Claude API," rather than the direct
/// client alone.
///
/// Built on the official `otari` Rust SDK crate (crates.io, pinned to
/// `0.0.1` — the version confirmed published at the time this was
/// written) rather than a hand-rolled HTTP client, because Otari's own
/// auth conventions are genuinely different from both OpenAI's and
/// native Anthropic's schemes (a custom `Otari-Key: Bearer <key>` header
/// in self-hosted mode, `Authorization: Bearer <token>` in platform
/// mode — confirmed via web search of the SDK's own source and docs),
/// and getting that right from fragmented documentation is a worse bet
/// than using the gateway maintainers' own client, which encapsulates
/// it correctly by construction.
///
/// The SDK reads `OTARI_API_KEY` and `OTARI_API_BASE` itself (its own
/// documented environment variables) — this module does not duplicate
/// that configuration. The one thing this module does own is the model
/// string (`OTARI_MODEL`, this project's own env var): the SDK has no
/// opinion on which model you want, since it's passed per-call as a
/// `"provider:model"` string (e.g. `"anthropic:claude-opus-4-20250514"`
/// or `"openai:gpt-4o-mini"` — Otari's own convention, confirmed from
/// its README examples).
pub struct OtariClient {
    model: String,
}

impl OtariClient {
    pub fn new(model: String) -> Self {
        Self { model }
    }
}

#[async_trait]
impl LlmBackend for OtariClient {
    async fn diagnose(&self, ctx: DiagnosticContext) -> Result<DiagnosisResult, DiagnosisError> {
        let prompt = build_prompt(&ctx);
        let messages = vec![otari::Message::user(prompt)];

        let response =
            otari::completion(&self.model, messages, otari::CompletionOptions::default())
                .await
                .map_err(|e| DiagnosisError::LlmRequestFailed(e.to_string()))?;

        let text = response.content().map(|s| s.to_string()).ok_or_else(|| {
            DiagnosisError::LlmResponseMalformed("response had no content".to_string())
        })?;

        if text.trim().is_empty() {
            return Err(DiagnosisError::LlmResponseMalformed(
                "response content was empty".to_string(),
            ));
        }

        let suggested_action = extract_suggested_action(&text);
        let summary = strip_suggested_action_line(&text);

        Ok(DiagnosisResult {
            mode: DiagnosisMode::Llm,
            summary,
            suggested_action,
            context: ctx,
        })
    }
}

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<AnthropicMessage<'a>>,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: String,
}

fn build_prompt(ctx: &DiagnosticContext) -> String {
    let context_json = serde_json::to_string_pretty(ctx).unwrap_or_else(|_| "{}".to_string());
    format!(
        "You are diagnosing system performance using ONLY the data below — do not assume or \
         invent anything not present in it. If the data does not clearly indicate a cause, say \
         so explicitly rather than guessing.\n\n\
         System state (processes sorted by CPU and by memory, network connections, services, \
         and events from the last {} minutes):\n\n{context_json}\n\n\
         Respond with a short diagnosis (2-4 sentences) citing specific process names, pids, or \
         event ids from the data above. If you believe stopping a specific process would help, \
         end your response with exactly one line in this format (omit the line entirely if no \
         action is warranted):\n\
         SUGGESTED_ACTION: {{\"action_type\": \"process.stop\", \"pid\": <pid>, \"reasoning\": \"<why>\"}}",
        ctx.lookback_minutes
    )
}

/// Best-effort parse of the `SUGGESTED_ACTION:` line described in the
/// prompt above. Deliberately returns `None` — never panics, never
/// errors the whole diagnosis — on anything it doesn't recognize: a
/// missing line, malformed JSON, an unexpected shape, or an action type
/// this parser doesn't model yet. A diagnosis with no usable suggested
/// action is a completely normal, successful outcome; a parse failure
/// here must never be treated as a diagnosis failure.
///
/// Only `process.stop` is modeled right now — this parser's shape would
/// need extending to cover the other three action types, which is a
/// bounded, mechanical follow-up, not a redesign.
fn extract_suggested_action(text: &str) -> Option<SuggestedAction> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("SUGGESTED_ACTION:"))?;
    let json_part = line.split_once("SUGGESTED_ACTION:")?.1.trim();
    let parsed: Value = serde_json::from_str(json_part).ok()?;

    let action_type = parsed.get("action_type")?.as_str()?.to_string();
    if action_type != "process.stop" {
        return None;
    }
    let pid = parsed.get("pid")?.as_i64()?;
    let reasoning = parsed
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(SuggestedAction {
        action_type,
        parameters: serde_json::json!({ "pid": pid }),
        reasoning,
    })
}

fn strip_suggested_action_line(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("SUGGESTED_ACTION:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------
// Configuration and top-level entrypoint
// ---------------------------------------------------------------------

/// Which LLM backend implements `DiagnosisMode::Llm`. Set via
/// `LLM_PROVIDER` — defaults to `anthropic_direct` so existing
/// deployments that only ever set `ANTHROPIC_API_KEY`/`ANTHROPIC_MODEL`
/// keep working unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    AnthropicDirect,
    Otari,
}

impl LlmProvider {
    fn parse_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "anthropic_direct" | "anthropic" | "direct" => Some(LlmProvider::AnthropicDirect),
            "otari" => Some(LlmProvider::Otari),
            _ => None,
        }
    }
}

/// Process-lifetime diagnosis configuration: which mode is the default,
/// and whether an LLM client is available at all (and if so, which
/// provider backs it). Built once at startup (`DiagnosisConfig::from_env`)
/// and injected via `Extension`, the same pattern as `auth::ApiKeyConfig`,
/// `rate_limit::RateLimiter`, and `actions_api::ActionsConfig`.
///
/// `DIAGNOSIS_MODE` is the mode switch: `rule_based` (default — always
/// available, no external dependency) or `llm`. A per-request `mode`
/// override is also accepted (see `diagnosis_api.rs`) and is NOT
/// restricted the way Phase 3's `ACTIONS_DRY_RUN` ceiling restricts
/// dry-run — there's no safety asymmetry between the two diagnosis
/// modes the way there is between simulated and real actions, so either
/// mode can be requested per-call as long as it's actually configured
/// server-side.
///
/// `LLM_PROVIDER` is a second, independent switch: which backend
/// implements `llm` mode when it's used. `llm_client` is `Arc<dyn
/// LlmBackend>` specifically so this struct doesn't need to know or care
/// which provider is behind it — see `LlmBackend` above.
#[derive(Clone)]
pub struct DiagnosisConfig {
    pub default_mode: DiagnosisMode,
    pub llm_client: Option<std::sync::Arc<dyn LlmBackend>>,
}

impl DiagnosisConfig {
    pub fn from_env() -> Self {
        let requested_default = std::env::var("DIAGNOSIS_MODE")
            .ok()
            .and_then(|v| DiagnosisMode::parse_str(&v))
            .unwrap_or(DiagnosisMode::RuleBased);

        let provider = std::env::var("LLM_PROVIDER")
            .ok()
            .and_then(|v| LlmProvider::parse_str(&v))
            .unwrap_or(LlmProvider::AnthropicDirect);

        let llm_client: Option<std::sync::Arc<dyn LlmBackend>> = match provider {
            LlmProvider::AnthropicDirect => {
                let api_key = std::env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .filter(|v| !v.trim().is_empty());
                let model = std::env::var("ANTHROPIC_MODEL")
                    .ok()
                    .filter(|v| !v.trim().is_empty());

                match (api_key, model) {
                    (Some(key), Some(model)) => {
                        tracing::info!(provider = "anthropic_direct", model = %model, "LLM diagnosis mode available");
                        Some(std::sync::Arc::new(AnthropicClient::new(key, model)))
                    }
                    _ => {
                        warn_if_llm_default_but_unconfigured(
                            requested_default,
                            "ANTHROPIC_API_KEY and/or ANTHROPIC_MODEL",
                        );
                        None
                    }
                }
            }
            LlmProvider::Otari => {
                // The otari SDK reads OTARI_API_KEY/OTARI_API_BASE
                // itself at call time — checked here only so a missing
                // key/base is caught at startup with a clear warning,
                // the same "fail loud at startup, not silently at
                // request time" pattern every other optional dependency
                // in this project follows (ACTIONS_DRY_RUN, API_KEY,
                // DATABASE_URL_READONLY).
                let api_key = std::env::var("OTARI_API_KEY")
                    .ok()
                    .filter(|v| !v.trim().is_empty());
                let api_base = std::env::var("OTARI_API_BASE")
                    .ok()
                    .filter(|v| !v.trim().is_empty());
                let model = std::env::var("OTARI_MODEL")
                    .ok()
                    .filter(|v| !v.trim().is_empty());

                match (api_key, api_base, model) {
                    (Some(_), Some(_), Some(model)) => {
                        tracing::info!(provider = "otari", model = %model, "LLM diagnosis mode available via Otari gateway");
                        Some(std::sync::Arc::new(OtariClient::new(model)))
                    }
                    _ => {
                        warn_if_llm_default_but_unconfigured(
                            requested_default,
                            "OTARI_API_KEY, OTARI_API_BASE, and/or OTARI_MODEL",
                        );
                        None
                    }
                }
            }
        };

        let default_mode = if requested_default == DiagnosisMode::Llm && llm_client.is_none() {
            DiagnosisMode::RuleBased
        } else {
            requested_default
        };

        tracing::info!(
            ?default_mode,
            ?provider,
            llm_available = llm_client.is_some(),
            "diagnosis engine configured"
        );

        Self {
            default_mode,
            llm_client,
        }
    }
}

fn warn_if_llm_default_but_unconfigured(requested_default: DiagnosisMode, missing: &str) {
    if requested_default == DiagnosisMode::Llm {
        tracing::warn!(
            "DIAGNOSIS_MODE=llm but {missing} are not set — falling back to rule_based as the \
             default. LLM mode can still be requested per-call once configured."
        );
    }
}

/// The top-level entrypoint: gathers context once, then runs exactly one
/// mode over it. `requested_mode` overrides `config.default_mode` when
/// present; requesting `Llm` without a configured client is a clear
/// error (`LlmNotConfigured`), never a silent fallback to rule-based —
/// a caller who explicitly asked for the LLM path should know it didn't
/// run, not unknowingly get a different kind of answer.
pub async fn diagnose(
    pool: &PgPool,
    config: &DiagnosisConfig,
    agent_id: Option<Uuid>,
    requested_mode: Option<DiagnosisMode>,
    lookback_minutes: i64,
) -> Result<DiagnosisResult, DiagnosisError> {
    let mode = requested_mode.unwrap_or(config.default_mode);

    let ctx = gather_context(pool, agent_id, lookback_minutes)
        .await
        .map_err(DiagnosisError::Database)?;

    match mode {
        DiagnosisMode::RuleBased => Ok(diagnose_rule_based(ctx)),
        DiagnosisMode::Llm => match &config.llm_client {
            Some(client) => client.diagnose(ctx).await,
            None => Err(DiagnosisError::LlmNotConfigured),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn process(pid: i32, name: &str, cpu_percent: Option<f64>) -> Process {
        Process {
            id: Uuid::new_v4(),
            pid,
            name: Some(name.to_string()),
            owner: None,
            status: Some("running".to_string()),
            cpu_percent,
            memory_bytes: Some(1024),
            started_at: None,
            last_seen_at: Utc::now(),
        }
    }

    fn empty_context() -> DiagnosticContext {
        DiagnosticContext {
            top_processes_by_cpu: vec![],
            top_processes_by_memory: vec![],
            network_connections: vec![],
            services: vec![],
            recent_events: vec![],
            lookback_minutes: 10,
        }
    }

    #[test]
    fn high_cpu_process_is_diagnosed_and_suggests_stopping_it() {
        let mut ctx = empty_context();
        ctx.top_processes_by_cpu = vec![process(1234, "runaway-worker", Some(95.0))];

        let result = diagnose_rule_based(ctx);

        assert!(result.summary.contains("runaway-worker"));
        assert!(result.summary.contains("1234"));
        let action = result
            .suggested_action
            .expect("expected a suggested action");
        assert_eq!(action.action_type, "process.stop");
        assert_eq!(action.parameters["pid"], 1234);
    }

    #[test]
    fn cpu_just_under_threshold_does_not_trigger_the_high_cpu_branch() {
        let mut ctx = empty_context();
        ctx.top_processes_by_cpu = vec![process(1234, "busy-but-not-runaway", Some(79.9))];

        let result = diagnose_rule_based(ctx);

        assert!(result.suggested_action.is_none());
        assert!(!result.summary.contains("busy-but-not-runaway"));
    }

    #[test]
    fn protected_process_names_are_never_suggested_for_stopping() {
        let mut ctx = empty_context();
        ctx.top_processes_by_cpu = vec![process(1, "systemd", Some(99.0))];

        let result = diagnose_rule_based(ctx);

        // Still correctly identifies it as the cause in the summary...
        assert!(result.summary.contains("systemd"));
        // ...but never suggests stopping it.
        assert!(result.suggested_action.is_none());
    }

    #[test]
    fn protected_match_is_case_insensitive_and_substring_based() {
        let mut ctx = empty_context();
        ctx.top_processes_by_cpu = vec![process(500, "PostgreSQL-worker-3", Some(90.0))];

        let result = diagnose_rule_based(ctx);
        assert!(result.suggested_action.is_none());
    }

    #[test]
    fn no_cpu_process_but_a_recent_failed_action_is_cited_instead() {
        let mut ctx = empty_context();
        ctx.top_processes_by_cpu = vec![process(1, "idle-process", Some(5.0))];
        ctx.recent_events = vec![EventRecord {
            id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            action_id: Some(Uuid::new_v4()),
            grant_id: None,
            event_type: "ActionFailed".to_string(),
            payload: serde_json::json!({}),
            occurred_at: Utc::now(),
            sequence: 1,
        }];

        let result = diagnose_rule_based(ctx);

        assert!(result.summary.contains("action failed"));
        assert!(
            result.suggested_action.is_none(),
            "a failed action must not be auto-retried"
        );
    }

    #[test]
    fn genuinely_no_cause_says_so_explicitly_rather_than_guessing() {
        // This is the negative case phase6-demo-loop.md insists matters
        // as much as the positive ones.
        let mut ctx = empty_context();
        ctx.top_processes_by_cpu = vec![process(1, "idle-process", Some(2.0))];

        let result = diagnose_rule_based(ctx);

        assert!(result.suggested_action.is_none());
        assert!(
            result.summary.to_lowercase().contains("no")
                && (result.summary.to_lowercase().contains("clear cause")
                    || result.summary.to_lowercase().contains("no process")),
            "expected an explicit 'no clear cause' style summary, got: {}",
            result.summary
        );
    }

    #[test]
    fn empty_context_does_not_panic_and_reports_no_cause() {
        let result = diagnose_rule_based(empty_context());
        assert!(result.suggested_action.is_none());
    }

    #[test]
    fn mode_parsing_accepts_documented_strings_and_rejects_garbage() {
        assert_eq!(
            DiagnosisMode::parse_str("rule_based"),
            Some(DiagnosisMode::RuleBased)
        );
        assert_eq!(
            DiagnosisMode::parse_str("RULE_BASED"),
            Some(DiagnosisMode::RuleBased)
        );
        assert_eq!(
            DiagnosisMode::parse_str("rule-based"),
            Some(DiagnosisMode::RuleBased)
        );
        assert_eq!(DiagnosisMode::parse_str("llm"), Some(DiagnosisMode::Llm));
        assert_eq!(DiagnosisMode::parse_str("LLM"), Some(DiagnosisMode::Llm));
        assert_eq!(DiagnosisMode::parse_str("gpt4"), None);
        assert_eq!(DiagnosisMode::parse_str(""), None);
    }

    #[test]
    fn extract_suggested_action_parses_a_well_formed_line() {
        let text = "The process is busy.\nSUGGESTED_ACTION: {\"action_type\": \"process.stop\", \"pid\": 4242, \"reasoning\": \"high cpu\"}";
        let action = extract_suggested_action(text).expect("expected a parsed action");
        assert_eq!(action.action_type, "process.stop");
        assert_eq!(action.parameters["pid"], 4242);
        assert_eq!(action.reasoning, "high cpu");
    }

    #[test]
    fn extract_suggested_action_returns_none_without_crashing_on_garbage() {
        assert!(extract_suggested_action("no suggestion line here at all").is_none());
        assert!(extract_suggested_action("SUGGESTED_ACTION: not even json").is_none());
        assert!(extract_suggested_action(
            "SUGGESTED_ACTION: {\"action_type\": \"cgroup.set_limit\"}"
        )
        .is_none());
        assert!(
            extract_suggested_action("SUGGESTED_ACTION: {\"action_type\": \"process.stop\"}")
                .is_none()
        ); // missing pid
        assert!(extract_suggested_action("").is_none());
    }

    #[test]
    fn strip_suggested_action_line_removes_only_that_line() {
        let text = "First sentence.\nSUGGESTED_ACTION: {\"action_type\": \"process.stop\", \"pid\": 1}\nignored";
        let stripped = strip_suggested_action_line(text);
        assert!(!stripped.contains("SUGGESTED_ACTION"));
        assert!(stripped.contains("First sentence."));
    }
}
