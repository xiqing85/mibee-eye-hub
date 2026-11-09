//! Tool & skill framework for the dialogue agent (SPEC §3.5, appendix A
//! #43). Built-in device capabilities (time / weather / camera snapshot)
//! and deployer-registered MCP (Model Context Protocol, spec 2025-06-18)
//! stdio subprocess servers are merged into one tool registry the LLM
//! can call during the agent loop (`runner`).
//!
//! Fail-open contract: an empty registry (agent disabled or no tools
//! configured) degrades the dialogue to the plain non-agent path with
//! zero behavior change for existing deployments.

pub mod builtin;
pub mod channels;
pub mod mcp;
pub mod runner;
pub mod tools_api;

pub use runner::{parse_tool_calls, qwen_tools_section};

use crate::server::SharedTools;
use crate::stream_manager::StreamManager;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// `[agent]` configuration section (SPEC appendix A #43).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Master gate. `true` with an empty registry is a no-op (the loop
    /// needs at least one tool before it engages).
    pub enabled: bool,
    /// Tool-calling iteration bound. The final iteration drops the tool
    /// table so the model must produce a text answer.
    pub max_steps: u32,
    /// Per tool-execution timeout (milliseconds).
    pub step_timeout_ms: u64,
    /// Voice turns carry the tool table only when the utterance matches
    /// a lightweight tool-intent heuristic (weather / time / snapshot /
    /// device control). Prefill dominates CPU inference (~28 ms/token),
    /// so un-gated voice turns paid ~700 extra prompt tokens (~20 s)
    /// even for "你好". HTTP chat always carries the full table.
    pub voice_tool_gate: bool,
    /// MCP stdio subprocess servers (`[[agent.mcp_servers]]`).
    pub mcp_servers: Vec<McpServerConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_steps: 3,
            step_timeout_ms: 15_000,
            voice_tool_gate: true,
            mcp_servers: Vec::new(),
        }
    }
}

/// One `[[agent.mcp_servers]]` entry: a local subprocess speaking MCP
/// over stdio (newline-delimited JSON-RPC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Display/source name — tools surface as `mcp:<name>` in /api/tools.
    pub name: String,
    /// Executable to spawn.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Content fingerprint of one MCP server config (trust gate, SPEC
/// appendix A #43 amendment): sha256 over the name, command, args and
/// the env map (sorted by key so map ordering never changes a
/// fingerprint). Any edit to the launch line changes the fingerprint
/// and re-arms the confirmation requirement.
pub fn mcp_fingerprint(s: &McpServerConfig) -> String {
    use sha2::Digest;
    let mut env_keys: Vec<&String> = s.env.keys().collect();
    env_keys.sort();
    let mut hasher = sha2::Sha256::new();
    hasher.update(s.name.as_bytes());
    hasher.update([0]);
    hasher.update(s.command.as_bytes());
    hasher.update([0]);
    for a in &s.args {
        hasher.update(a.as_bytes());
        hasher.update([0]);
    }
    for k in env_keys {
        hasher.update(k.as_bytes());
        hasher.update([1]);
        hasher.update(s.env[k].as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

/// The confirmed-trust map persisted as settings key `agent.mcp_trust`
/// (`{"<name>": "<fingerprint>"}`). A server runs only when its CURRENT
/// fingerprint matches its entry — hex's content-hash trust for config
/// scripts; an MCP server is arbitrary local code, so a new or edited
/// launch line stays REFUSED until the operator confirms.
pub type McpTrustMap = std::collections::HashMap<String, String>;

/// Pure gate: split the configured servers into (allowed, blocked).
pub fn mcp_trust_split<'a>(
    servers: &'a [McpServerConfig],
    trust: &McpTrustMap,
) -> (Vec<&'a McpServerConfig>, Vec<(&'a McpServerConfig, String)>) {
    let mut allowed = Vec::new();
    let mut blocked = Vec::new();
    for s in servers {
        let fp = mcp_fingerprint(s);
        if trust.get(&s.name).is_some_and(|t| t == &fp) {
            allowed.push(s);
        } else {
            blocked.push((s, fp));
        }
    }
    (allowed, blocked)
}

/// A tool as exposed to the model and `GET /api/tools`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema (object) for the tool arguments.
    pub input_schema: serde_json::Value,
    /// `"builtin"` or `"mcp:<server-name>"`.
    pub source: String,
}

/// One executed tool call — the API/SSE/record-visible outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub args: serde_json::Value,
    pub ok: bool,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_url: Option<String>,
    pub duration_ms: u64,
}

/// Internal execution result before it becomes a record.
#[derive(Debug)]
pub struct ToolOutput {
    pub text: String,
    pub media_url: Option<String>,
}

/// Live step emitted to SSE (`agent_step`, SPEC §6) and consumed by the
/// caller for conversation thinking entries.
#[derive(Debug, Clone)]
pub enum AgentStep {
    PhaseThinking,
    PhaseAnswering {
        engine: String,
    },
    ToolStarted {
        name: String,
        args: serde_json::Value,
    },
    ToolFinished {
        name: String,
        args: serde_json::Value,
        record: ToolCallRecord,
    },
}

/// Result of feeding one tool observation back to the model (local
/// Qwen3 `<tool_response>` wrapper).
pub(crate) fn tool_feedback_text(name: &str, output: &str) -> String {
    format!("<tool_response>\n工具 {name} 返回：\n{output}\n</tool_response>")
}

/// Truncate tool output before it enters the model context (SPEC #43:
/// 8 KiB cap — unbounded MCP output would blow the local n_ctx).
pub(crate) fn truncate_for_model(s: &str) -> String {
    const CAP: usize = 8 * 1024;
    if s.len() <= CAP {
        return s.to_string();
    }
    let mut cut = CAP;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…(结果过长已截断)", &s[..cut])
}

/// The merged registry: built-ins first, then every configured MCP
/// server's tools. Registry construction never fails — unreachable MCP
/// servers simply contribute no tools until they come back.
pub struct ToolRegistry {
    shared_tools: SharedTools,
    /// Attached once the stream manager exists (main constructs the
    /// registry earlier in the boot order); `None` only during boot.
    streams: RwLock<Option<Arc<StreamManager>>>,
    mcp: Vec<mcp::McpClient>,
    mcp_tools: RwLock<Vec<ToolSpec>>,
    /// Confirmed MCP fingerprints (see [`mcp_trust_split`]); loaded at
    /// boot from settings, updated by the trust endpoint.
    trust: RwLock<McpTrustMap>,
}

impl ToolRegistry {
    pub fn new(shared_tools: SharedTools, cfg: &AgentConfig) -> Self {
        let mcp = cfg
            .mcp_servers
            .iter()
            .map(|s| mcp::McpClient::new(s.clone()))
            .collect();
        Self {
            shared_tools,
            streams: RwLock::new(None),
            mcp,
            mcp_tools: RwLock::new(Vec::new()),
            trust: RwLock::new(McpTrustMap::new()),
        }
    }

    /// Replace the confirmed-trust map (boot load + trust endpoint).
    pub fn set_trust(&self, trust: McpTrustMap) {
        *self.trust.write() = trust;
    }

    /// The confirmed-truth map (the trust endpoint merges into it).
    pub fn trust_map(&self) -> McpTrustMap {
        self.trust.read().clone()
    }

    /// The configured MCP server configs (fingerprinting/UI).
    pub fn mcp_configs(&self) -> Vec<McpServerConfig> {
        self.mcp.iter().map(|c| c.config().clone()).collect()
    }

    /// Attach the stream manager once it exists (camera.snapshot needs
    /// `latest_jpeg`; every other tool is independent of it).
    pub fn attach_streams(&self, streams: Arc<StreamManager>) {
        *self.streams.write() = Some(streams);
    }

    /// True when the agent loop may engage (agent on + ≥1 tool source).
    pub fn agent_ready(&self, cfg: &AgentConfig) -> bool {
        cfg.enabled && (!self.builtin_specs().is_empty() || !self.mcp.is_empty())
    }

    /// Built-in tools available on this host right now.
    pub fn builtin_specs(&self) -> Vec<ToolSpec> {
        builtin::specs(&self.shared_tools)
    }

    /// Cached MCP tool list (refreshed by [`Self::refresh_mcp`]).
    pub fn cached_specs(&self) -> Vec<ToolSpec> {
        let mut all = self.builtin_specs();
        all.extend(self.mcp_tools.read().iter().cloned());
        all
    }

    /// Connect/initialize every MCP server and re-list its tools.
    /// Server-level failures log and contribute nothing (fail-open).
    /// Unconfirmed servers (trust gate, SPEC appendix A #43 amendment)
    /// are REFUSED — an MCP server is arbitrary local code; a new or
    /// edited launch line stays down until the operator confirms it
    /// (`POST /api/tools/mcp/{name}/trust`).
    pub async fn refresh_mcp(&self) {
        let trust = self.trust.read().clone();
        let configs: Vec<McpServerConfig> = self.mcp.iter().map(|c| c.config().clone()).collect();
        let (allowed, blocked) = mcp_trust_split(&configs, &trust);
        for (cfg, fp) in &blocked {
            tracing::warn!(
                server = %cfg.name,
                command = %cfg.command,
                fingerprint = %fp,
                "agent: MCP server not confirmed — refusing to start it                  (POST /api/tools/mcp/{}/trust with the current fingerprint                  to confirm; edited launch lines re-arm this gate)",
                cfg.name
            );
        }
        let allowed_names: std::collections::HashSet<&str> =
            allowed.iter().map(|c| c.name.as_str()).collect();
        let mut listed = Vec::new();
        for client in &self.mcp {
            if !allowed_names.contains(client.name()) {
                continue; // unconfirmed — refused above
            }
            match client.ensure_initialized().await {
                Ok(()) => match client.list_tools().await {
                    Ok(tools) => listed.extend(tools),
                    Err(e) => {
                        tracing::warn!("mcp server {}: tools/list failed: {e}", client.name())
                    }
                },
                Err(e) => tracing::warn!("mcp server {}: init failed: {e}", client.name()),
            }
        }
        *self.mcp_tools.write() = listed;
    }

    /// Startup warmup so capabilities/report counts settle quickly.
    pub async fn warmup(self: Arc<Self>) {
        self.refresh_mcp().await;
    }

    /// Execute one tool by name (built-in dispatch, then MCP servers).
    pub async fn call(
        &self,
        name: &str,
        args: serde_json::Value,
        timeout: std::time::Duration,
    ) -> anyhow::Result<ToolOutput> {
        let fut = async {
            let streams = self
                .streams
                .read()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("stream manager not attached yet"))?;
            match builtin::dispatch(name, &args, &self.shared_tools, &streams).await {
                Ok(Some(out)) => return Ok(out),
                Ok(None) => {} // not a built-in — fall through to MCP
                Err(e) => return Err(e),
            }
            for client in &self.mcp {
                if client.has_tool(name).await {
                    return client.call_tool(name, args).await;
                }
            }
            Err(anyhow::anyhow!("unknown tool: {name}"))
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(anyhow::anyhow!(
                "tool {name} timed out after {}ms",
                timeout.as_millis()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(name: &str, command: &str, args: &[&str]) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            command: command.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn absent_section_parses_to_defaults() {
        let cfg: AgentConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_steps, 3);
        assert_eq!(cfg.step_timeout_ms, 15_000);
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn mcp_fingerprint_is_stable_and_sensitive() {
        let a = srv("home", "python3", &["/opt/home_tools.py"]);
        let b = srv("home", "python3", &["/opt/home_tools.py"]);
        assert_eq!(
            mcp_fingerprint(&a),
            mcp_fingerprint(&b),
            "identical configs match"
        );
        // Any launch-line edit re-arms the gate.
        let c = srv("home", "python3", &["/opt/other.py"]);
        assert_ne!(
            mcp_fingerprint(&a),
            mcp_fingerprint(&c),
            "arg edit changes fp"
        );
        let d = srv("home2", "python3", &["/opt/home_tools.py"]);
        assert_ne!(
            mcp_fingerprint(&a),
            mcp_fingerprint(&d),
            "name edit changes fp"
        );
        // Env map ordering never matters.
        let mut e1 = srv("home", "python3", &[]);
        e1.env.insert("B".into(), "2".into());
        e1.env.insert("A".into(), "1".into());
        let mut e2 = srv("home", "python3", &[]);
        e2.env.insert("A".into(), "1".into());
        e2.env.insert("B".into(), "2".into());
        assert_eq!(mcp_fingerprint(&e1), mcp_fingerprint(&e2));
    }

    #[test]
    fn trust_split_confirms_only_matching_fingerprints() {
        let servers = vec![srv("a", "/bin/a", &[]), srv("b", "/bin/b", &[])];
        let mut trust = McpTrustMap::new();
        trust.insert("a".into(), mcp_fingerprint(&servers[0]));
        let (allowed, blocked) = mcp_trust_split(&servers, &trust);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].name, "a");
        assert_eq!(blocked.len(), 1, "unconfirmed b refused");
        assert_eq!(blocked[0].0.name, "b");
        // A STALE fingerprint (server edited since confirmation) is
        // refused, not grandfathered.
        let mut stale = McpTrustMap::new();
        stale.insert("a".into(), "deadbeef".into());
        let (allowed, blocked) = mcp_trust_split(&servers, &stale);
        assert!(allowed.is_empty(), "stale fp must not run");
        assert_eq!(blocked.len(), 2);
        // Empty map = everything needs confirmation (fail-closed for
        // trust, fail-open for the service itself).
        let (allowed, blocked) = mcp_trust_split(&servers, &McpTrustMap::new());
        assert!(allowed.is_empty());
        assert_eq!(blocked.len(), 2);
    }

    #[test]
    fn mcp_servers_parse_from_array_of_tables() {
        let cfg: AgentConfig = serde_json::from_str(
            r#"{"mcp_servers": [
                 {"name": "home", "command": "python3", "args": ["s.py"], "env": {"KEY": "1"}},
                 {"name": "lab", "command": "./lab-mcp"}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        assert_eq!(cfg.mcp_servers[0].args, vec!["s.py"]);
        assert_eq!(
            cfg.mcp_servers[0].env.get("KEY").map(String::as_str),
            Some("1")
        );
        assert_eq!(cfg.mcp_servers[1].command, "./lab-mcp");
    }

    #[test]
    fn tool_feedback_wraps_qwen3_tool_response() {
        assert_eq!(
            tool_feedback_text("weather.current", "Sunny 31°C"),
            "<tool_response>\n工具 weather.current 返回：\nSunny 31°C\n</tool_response>"
        );
    }

    #[test]
    fn truncation_respects_char_boundaries_and_caps_size() {
        let tiny = "短文本";
        assert_eq!(truncate_for_model(tiny), tiny);
        let long = "a".repeat(9 * 1024);
        let out = truncate_for_model(&long);
        assert!(out.len() < long.len());
        assert!(out.ends_with("…(结果过长已截断)"));
        // Multibyte boundary: cut lands inside a CJK char without panicking.
        let cjk = "中".repeat(5000);
        let out = truncate_for_model(&cjk);
        let body: String = out.chars().take_while(|&c| c == '中').collect();
        assert!(body.chars().count() > 2000, "kept most of the input");
        assert!(out.ends_with("…(结果过长已截断)"));
        assert!(
            out.chars()
                .all(|c| c == '中' || c == '\n' || "…(结果过长已截断)".contains(c))
        );
    }
}
