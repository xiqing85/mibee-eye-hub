//! Dialogue task tools (SPEC appendix A #30-A): internet lookups whose
//! results are injected into the chat system turn as a 【联网】 block.
//!
//! Weather-first via wttr.in (no API key). Intent-gated — a lookup only
//! runs when the user's text actually asks about weather AND the host
//! enabled `[tools] weather_enabled`; every failure is fail-open (no
//! block, the model honestly says it does not know).

use serde::{Deserialize, Serialize};

/// `[tools]` configuration section (SPEC appendix A #30-A).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Weather lookup via wttr.in (off by default — outbound policy).
    pub weather_enabled: bool,
    /// City name (pinyin or Chinese); empty = feature inert.
    pub weather_city: String,
    /// Lookup timeout.
    pub timeout_secs: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            weather_enabled: false,
            weather_city: String::new(),
            timeout_secs: 5,
        }
    }
}

/// `[resources]` configuration section (#30-E + appendix A #40).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourcesConfig {
    /// Auto-pick the LLM tier from available memory at startup
    /// (legacy single key; kept for deployed-config back-compat).
    pub auto_tier: bool,
    /// Feature-level gate mode (SPEC appendix A #40): `"auto"` (default)
    /// admits AI features greedily against the boot memory budget;
    /// `"all"` boots every enabled feature (legacy behaviour).
    pub feature_gate: String,
    /// Headroom kept out of the budget in auto mode (MiB). 512:
    /// MemAvailable already excludes reclaimable page cache, and the
    /// file-size ×1.15 factor plus per-engine overheads carry the rest
    /// of the conservatism.
    pub reserve_mib: u64,
    /// Named boot tier (appendix A #40 named-tier addendum): `"auto"`
    /// (default, hardware-derived by `feature_gate::resolve_tier`) or
    /// pinned `"minimal"|"basic"|"standard"|"full"`. A pin selects the
    /// wanted feature subset preset and caps the LLM tier rank.
    pub tier: String,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            auto_tier: false,
            feature_gate: "auto".into(),
            reserve_mib: 512,
            tier: "auto".into(),
        }
    }
}

/// Resolve the LLM model path for the current tier (#30-E + the #40
/// named-tier addendum). The memory suggestion (available RAM: ≥8 GiB →
/// full, ≥4 GiB → mid, else lite) is unchanged and then clamped by two
/// upper bounds: physical cores (< 4 → at most `mid`, < 2 → `lite` —
/// llama decode throughput collapses on narrow cores, larger files are
/// compute failures not memory failures) and the pinned
/// `[resources] tier` rank (`minimal` → `lite`, `basic` → `mid`,
/// `standard`/`full` → uncapped). The effective tier is the minimum of
/// the three. Empty tier paths fall back to `model_path`; the resolved
/// tier name returns alongside for logging/capabilities.
#[allow(clippy::too_many_arguments)]
pub fn resolve_llm_tier(
    model_path: &str,
    model_path_mid: &str,
    model_path_lite: &str,
    auto_tier: bool,
    avail_mib: u64,
    physical_cores: u32,
    tier_cap: &str,
) -> (String, &'static str) {
    if !auto_tier {
        return (model_path.to_string(), "manual");
    }
    let suggested: &'static str = if avail_mib >= 8 * 1024 {
        "full"
    } else if avail_mib >= 4 * 1024 {
        "mid"
    } else {
        "lite"
    };
    let core_cap: &'static str = if physical_cores < 2 {
        "lite"
    } else if physical_cores < 4 {
        "mid"
    } else {
        "full"
    };
    let rank_cap: &'static str = match crate::feature_gate::Tier::from_name(tier_cap) {
        Some(crate::feature_gate::Tier::Minimal) => "lite",
        Some(crate::feature_gate::Tier::Basic) => "mid",
        // standard/full → no pin below full; auto/unknown pins → no cap.
        _ => "full",
    };
    let rank = |t: &'static str| match t {
        "full" => 2_u8,
        "mid" => 1,
        _ => 0,
    };
    let effective: &'static str = [suggested, core_cap, rank_cap]
        .into_iter()
        .min_by_key(|t| rank(t))
        .unwrap_or("lite");
    match effective {
        "mid" if !model_path_mid.is_empty() => (model_path_mid.to_string(), "mid"),
        "lite" if !model_path_lite.is_empty() => (model_path_lite.to_string(), "lite"),
        // Empty tier paths load the primary model under the honest
        // (smaller) tier name.
        _ => (model_path.to_string(), effective),
    }
}

/// Read MemAvailable from /proc/meminfo (MiB). Linux-only product.
pub fn available_mem_mib() -> Option<u64> {
    memory_mib().map(|(_, avail)| avail)
}

/// Read MemTotal + MemAvailable from /proc/meminfo (MiB). Linux-only
/// product; the feature gate (#40) samples both at boot.
pub fn memory_mib() -> Option<(u64, u64)> {
    parse_memory_mib(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// Pure meminfo parser behind [`memory_mib`] — MiB pair (total, avail).
/// Returns None when MemTotal is missing.
#[must_use]
pub fn parse_memory_mib(text: &str) -> Option<(u64, u64)> {
    let mut total = None;
    let mut avail = 0_u64;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok();
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .unwrap_or(0);
        }
    }
    Some((total? / 1024, avail / 1024))
}

/// Does this user utterance ask about weather? (intent gate)
#[must_use]
pub fn weather_intent(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "天气",
        "气温",
        "温度几",
        "几度",
        "下雨",
        "落雨",
        "落緊雨",
        "台风",
        "颱風",
        "冷不冷",
        "热不热",
        "weather",
        "temperature",
        "rain",
        "forecast",
    ];
    let lower = text.to_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Does this utterance plausibly need a tool (SPEC §3.5 voice gate)?
/// Superset of [`weather_intent`] plus time / snapshot / device-control
/// needles in the three product languages. Pure, allocation-light — the
/// voice bridge runs it per turn to decide whether the (prefill-heavy)
/// tool table joins the prompt.
#[must_use]
pub fn tool_intent(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        // weather (weather_intent covers these too; kept for clarity)
        "天气",
        "气温",
        "几度",
        "下雨",
        "落雨",
        "台风",
        "颱風",
        "weather",
        "rain",
        "forecast",
        // time
        "几点",
        "时间",
        "日期",
        "几号",
        "今天几",
        "星期几",
        "time",
        "date",
        // snapshot / camera
        "画面",
        "快照",
        "截图",
        "看一下",
        "看看",
        "看到",
        "监控",
        "snapshot",
        "camera",
        // device control (MCP plugins: lights etc.)
        "开灯",
        "关灯",
        "灯",
        "开关",
        "打开",
        "关闭",
        "light",
        "switch",
    ];
    let lower = text.to_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n)) || weather_intent(text)
}

/// One wttr.in `?format=j1` current-condition row (subset).
#[derive(Debug, Deserialize)]
struct WttrCurrent {
    #[serde(rename = "temp_C")]
    temp_c: String,
    #[serde(rename = "FeelsLikeC")]
    feels_c: String,
    #[serde(rename = "humidity")]
    humidity: String,
    #[serde(rename = "windspeedKmph")]
    wind_kmph: String,
    #[serde(rename = "weatherDesc")]
    desc: Vec<WttrDesc>,
}

#[derive(Debug, Deserialize)]
struct WttrDesc {
    value: String,
}

#[derive(Debug, Deserialize)]
struct WttrJson {
    #[serde(rename = "current_condition")]
    current_condition: Vec<WttrCurrent>,
}

/// Fetch the current weather for `city` and render the compact 【联网】
/// payload (Chinese labels; the description stays in wttr.in's English
/// — the LLM translates it in the reply).
///
/// # Errors
///
/// Network failure, timeout, or unexpected payload shape.
pub async fn fetch_weather(city: &str, timeout_secs: u64) -> anyhow::Result<String> {
    let url = format!("https://wttr.in/{}?format=j1", urlencode(city));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .user_agent("mibee-eye")
        .build()?;
    let body = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let parsed: WttrJson = serde_json::from_str(&body)?;
    let cur = parsed
        .current_condition
        .first()
        .ok_or_else(|| anyhow::anyhow!("wttr.in: no current_condition"))?;
    let desc = cur.desc.first().map(|d| d.value.as_str()).unwrap_or("n/a");
    Ok(format!(
        "{city} 当前天气：{desc}，气温 {}°C（体感 {}°C），湿度 {}%，风速 {}km/h",
        cur.temp_c, cur.feels_c, cur.humidity, cur.wind_kmph
    ))
}

/// Minimal percent-encoding for the city path segment.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_matches_weather_questions_in_three_languages() {
        assert!(weather_intent("今天天气怎么样"));
        assert!(weather_intent("听日会唔会落雨呀"));
        assert!(weather_intent("what's the weather like"));
        assert!(weather_intent("temperature outside?"));
        assert!(!weather_intent("现在几点了"));
        assert!(!weather_intent("讲个笑话"));
    }

    #[test]
    fn tool_intent_covers_tool_topics_only() {
        assert!(tool_intent("现在广州天气怎么样"));
        assert!(tool_intent("现在几点了"));
        assert!(tool_intent("帮我把客厅的灯打开"));
        assert!(tool_intent("看看门口画面"));
        assert!(tool_intent("what's the weather like"));
        // Chit-chat stays on the fast path (no tool table in the prompt).
        assert!(!tool_intent("你好"));
        assert!(!tool_intent("讲个笑话"));
        assert!(!tool_intent("你是谁"));
    }

    #[test]
    fn urlencode_keeps_ascii_and_encodes_cjk() {
        assert_eq!(urlencode("Guangzhou"), "Guangzhou");
        assert_eq!(urlencode("广州"), "%E5%B9%BF%E5%B7%9E");
    }

    #[tokio::test]
    async fn fetch_weather_parses_wttr_payload() {
        // Shape test against a captured wttr.in j1 fragment — no network.
        let payload = r#"{"current_condition":[{"temp_C":"26","FeelsLikeC":"28",
            "humidity":"70","windspeedKmph":"12",
            "weatherDesc":[{"value":"Partly cloudy"}]}]}"#;
        let parsed: WttrJson = serde_json::from_str(payload).unwrap();
        let cur = &parsed.current_condition[0];
        assert_eq!(cur.temp_c, "26");
        assert_eq!(cur.desc[0].value, "Partly cloudy");
    }
}

#[cfg(test)]
mod resources_config_tests {
    use super::*;

    #[test]
    fn absent_section_falls_back_to_gate_defaults() {
        // A config without [resources] (every pre-#40 deployment) must
        // parse into the gating defaults — auto mode, 512 MiB reserve.
        let cfg: ResourcesConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.feature_gate, "auto");
        assert_eq!(cfg.reserve_mib, 512);
        assert!(!cfg.auto_tier);
    }

    #[test]
    fn legacy_auto_tier_only_config_keeps_new_defaults() {
        // The shape already deployed on .41: only auto_tier set.
        let cfg: ResourcesConfig = toml::from_str("auto_tier = true").unwrap();
        assert!(cfg.auto_tier);
        assert_eq!(cfg.feature_gate, "auto");
        assert_eq!(cfg.reserve_mib, 512);
    }

    #[test]
    fn explicit_values_roundtrip() {
        let cfg: ResourcesConfig =
            toml::from_str("feature_gate = \"all\"\nreserve_mib = 1024\nauto_tier = true").unwrap();
        assert_eq!(cfg.feature_gate, "all");
        assert_eq!(cfg.reserve_mib, 1024);
        assert!(cfg.auto_tier);
    }

    #[test]
    fn named_tier_key_defaults_to_auto_and_roundtrips() {
        let cfg: ResourcesConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.tier, "auto");
        // Pinned tier (.41's planned deploy shape) parses standalone.
        let cfg: ResourcesConfig = toml::from_str("tier = \"standard\"").unwrap();
        assert_eq!(cfg.tier, "standard");
    }

    #[test]
    fn memory_parser_takes_kb_to_mib() {
        let text =
            "MemTotal:       3916720 kB\nMemFree:         123456 kB\nMemAvailable:   2987000 kB\n";
        assert_eq!(parse_memory_mib(text), Some((3824, 2916)));
        assert_eq!(
            parse_memory_mib("MemFree: 1 kB"),
            None,
            "no MemTotal -> None"
        );
    }
}

#[cfg(test)]
mod tier_tests {
    use super::*;

    #[test]
    fn tier_thresholds_and_fallbacks() {
        let (p, t) = resolve_llm_tier(
            "full.gguf",
            "mid.gguf",
            "lite.gguf",
            true,
            12 * 1024,
            4,
            "auto",
        );
        assert_eq!((p.as_str(), t), ("full.gguf", "full"));
        let (p, t) = resolve_llm_tier(
            "full.gguf",
            "mid.gguf",
            "lite.gguf",
            true,
            5 * 1024,
            4,
            "auto",
        );
        assert_eq!((p.as_str(), t), ("mid.gguf", "mid"));
        let (p, t) = resolve_llm_tier(
            "full.gguf",
            "mid.gguf",
            "lite.gguf",
            true,
            2 * 1024,
            4,
            "auto",
        );
        assert_eq!((p.as_str(), t), ("lite.gguf", "lite"));
        // Empty tier path → fall back to the primary model, tier still named.
        let (p, t) = resolve_llm_tier("full.gguf", "", "", true, 2 * 1024, 4, "auto");
        assert_eq!((p.as_str(), t), ("full.gguf", "lite"));
        // auto_tier off → manual, primary path.
        let (p, t) = resolve_llm_tier("full.gguf", "mid.gguf", "lite.gguf", false, 512, 4, "auto");
        assert_eq!((p.as_str(), t), ("full.gguf", "manual"));
    }

    #[test]
    fn llm_tier_anchor_hosts_with_core_cap() {
        // Workstation i5-1135G7 (4 physical cores, plenty of RAM) → full(4B).
        let t = resolve_llm_tier("p", "m", "l", true, 12_000, 4, "auto").1;
        assert_eq!(t, "full");
        // i5-6200U 2C/4T with a fat memory read: memory suggests full, the
        // core cap holds it at mid (1.7B).
        let t = resolve_llm_tier("p", "m", "l", true, 10_000, 2, "auto").1;
        assert_eq!(t, "mid");
        // i3-2370M 2C/3.7G (Sandy Bridge): memory says lite anyway.
        let t = resolve_llm_tier("p", "m", "l", true, 1_700, 2, "auto").1;
        assert_eq!(t, "lite");
        // A single-vCPU VM stays at lite even with huge RAM.
        let t = resolve_llm_tier("p", "m", "l", true, 32_000, 1, "auto").1;
        assert_eq!(t, "lite");
    }

    #[test]
    fn llm_tier_pinned_tier_cap() {
        // Pin minimal on a large box → lite forces the small model.
        let (p, t) = resolve_llm_tier("p", "m", "l", true, 16_000, 8, "minimal");
        assert_eq!((p.as_str(), t), ("l", "lite"));
        // Pin basic → cap at mid.
        let t = resolve_llm_tier("p", "m", "l", true, 16_000, 8, "basic").1;
        assert_eq!(t, "mid");
        // Pin standard/full → no rank cap.
        for pin in ["standard", "full"] {
            let t = resolve_llm_tier("p", "m", "l", true, 16_000, 8, pin).1;
            assert_eq!(t, "full", "{pin}");
        }
        // Unknown pin strings don't cap (hardware derivation decides;
        // main.rs logs the unknown-tier warning separately).
        let t = resolve_llm_tier("p", "m", "l", true, 16_000, 4, "nonsense").1;
        assert_eq!(t, "full");
        // Pinned caps combine with memory: basic pin + 2 GiB → lite.
        let t = resolve_llm_tier("p", "m", "l", true, 2_000, 8, "basic").1;
        assert_eq!(t, "lite");
    }

    #[test]
    fn llm_tier_missing_tier_files_fall_back_to_primary_under_honest_name() {
        // Effective mid (2 cores) with an empty mid path: the primary
        // model file loads under the honest tier name.
        let (p, t) = resolve_llm_tier("p", "", "l", true, 10_000, 2, "auto");
        assert_eq!(t, "mid");
        assert_eq!(p, "p");
        // A present mid path is used verbatim.
        let (p, t) = resolve_llm_tier("p", "m", "l", true, 10_000, 2, "auto");
        assert_eq!((p.as_str(), t), ("m", "mid"));
    }

    #[test]
    fn meminfo_parses_when_present() {
        if let Some(mib) = available_mem_mib() {
            assert!(mib > 100, "{mib}");
        }
    }
}
