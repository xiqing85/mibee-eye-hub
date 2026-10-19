//! Feature-level resource gating (SPEC appendix A #40).
//!
//! Small-memory hosts must not boot every AI feature: the LLM/VLM/voice
//! stacks together can outweigh the RAM a netbook-class deployment has.
//! At boot the planner estimates each optional feature's resident cost
//! from its model files on disk and admits features greedily in a fixed
//! priority order while the running sum fits the memory budget
//! (`MemAvailable − reserve`). Non-admitted features are disabled in the
//! config copy handed to the engines, so the existing fail-open
//! machinery reports them inactive; the decision table is exposed via
//! `capabilities.resource` (and `mibee_eye_feature_admitted` gauges).
//!
//! The decision is boot-time only: no runtime eviction. A restart
//! re-evaluates against the then-current memory watermark. Model files
//! that do not exist cost nothing here — the engine's own fail-open
//! reason ("model missing") still surfaces honestly.

use serde::Serialize;

/// Feature was admitted (empty reason).
pub const REASON_ON: &str = "";
/// Feature disabled in config — the gate has nothing to say.
pub const REASON_OFF_CONFIG: &str = "off_config";
/// Feature enabled but its cost does not fit the remaining budget.
pub const REASON_OFF_BUDGET: &str = "off_budget";
/// Feature's dependency (the voice stack) was not admitted.
pub const REASON_DEPENDENCY: &str = "dependency";
/// Feature enabled and affordable, but excluded by the applied named
/// tier (SPEC appendix A #40 named-tier addendum).
pub const REASON_OFF_TIER: &str = "off_tier";

/// SIMD class of the host x86_64 CPU. Ordered: higher = wider kernels
/// usable by llama.cpp / onnxruntime. The llama.cpp GPU backends are
/// build-time properties — runtime tiering never considers a GPU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SimdClass {
    /// SSE4.2/AVX only (e.g. Sandy Bridge).
    #[default]
    Baseline,
    /// x86-64-v3 (AVX2/FMA/F16C kernels available).
    Avx2,
    /// AVX-512 with VNNI (preferred for full-tier chat models).
    Avx512Vnni,
}

impl SimdClass {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Avx2 => "avx2",
            Self::Avx512Vnni => "avx512_vnni",
        }
    }

    /// Classify one /proc/cpuinfo `flags` line (whitespace-separated).
    #[must_use]
    pub fn from_flags(flags: &str) -> Self {
        let has = |k: &str| flags.split_whitespace().any(|f| f == k);
        if has("avx512_vnni") {
            Self::Avx512Vnni
        } else if has("avx2") {
            Self::Avx2
        } else {
            Self::Baseline
        }
    }
}

/// /proc/cpuinfo-derived CPU identity (pure; see
/// [`HardwareProfile::detect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CpuInfo {
    pub physical_cores: u32,
    pub simd: SimdClass,
}

/// Count unique `(physical id, core id)` pairs (SMT siblings share
/// them). Machines without the fields (some VMs) fall back to the
/// logical `processor` count.
#[must_use]
pub fn parse_cpuinfo(text: &str) -> CpuInfo {
    let mut pairs = std::collections::HashSet::new();
    let mut logical = 0_u32;
    let mut saw_physical = false;
    let mut phys = String::new();
    let mut simd = SimdClass::Baseline;
    let mut flags_seen = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "processor" => logical += 1,
            "physical id" => {
                phys = v.trim().to_string();
                saw_physical = true;
            }
            "core id" => {
                pairs.insert((phys.clone(), v.trim().to_string()));
            }
            "flags" if !flags_seen => {
                flags_seen = true;
                simd = SimdClass::from_flags(v.trim());
            }
            _ => {}
        }
    }
    if saw_physical && !pairs.is_empty() {
        CpuInfo {
            physical_cores: pairs.len() as u32,
            simd,
        }
    } else {
        CpuInfo {
            physical_cores: logical,
            simd,
        }
    }
}

/// Battery-powered = any power_supply entry of type `Battery`
/// (informational: surfaced via `capabilities.resource.profile` and a
/// boot log line, never a tier threshold).
#[must_use]
pub fn battery_powered<T: AsRef<str>>(supply_types: &[T]) -> bool {
    supply_types
        .iter()
        .any(|t| t.as_ref().eq_ignore_ascii_case("Battery"))
}

/// Boot-time hardware identity. Read once before any engine is built.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HardwareProfile {
    pub physical_cores: u32,
    pub simd: SimdClass,
    pub battery: bool,
}

impl HardwareProfile {
    /// Probe the host (Linux-only product). Failed reads degrade to an
    /// empty profile — the tier resolver then classifies minimally
    /// instead of guessing up.
    #[must_use]
    pub fn detect() -> Self {
        let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let CpuInfo {
            physical_cores,
            simd,
        } = parse_cpuinfo(&text);
        let battery = std::fs::read_dir("/sys/class/power_supply")
            .map(|entries| {
                let types: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| std::fs::read_to_string(e.path().join("type")).ok())
                    .collect();
                battery_powered(&types)
            })
            .unwrap_or(false);
        Self {
            physical_cores,
            simd,
            battery,
        }
    }
}

/// Named capability tier (appendix A #40 named-tier addendum): a boot
/// preset selecting the wanted feature subset plus an LLM-tier
/// ceiling. Ordered low→high.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Minimal,
    Basic,
    Standard,
    Full,
}

impl Tier {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Basic => "basic",
            Self::Standard => "standard",
            Self::Full => "full",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "minimal" => Some(Self::Minimal),
            "basic" => Some(Self::Basic),
            "standard" => Some(Self::Standard),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Wanted subset, against the ten fixed-priority feature names
    /// (`full` = everything; `standard` = minus meeting/vlm; `basic` =
    /// the conversation loop; `minimal` = sensing only). Unknown
    /// forward-compat names are not tier-scoped (always wanted).
    #[must_use]
    pub fn includes(self, feature: &str) -> bool {
        match feature {
            "ai" | "audio_ai" => true,
            "voice" | "llm" | "tts" | "decision" => self >= Self::Basic,
            "face" | "ocr" => self >= Self::Standard,
            "meeting" | "vlm" => self >= Self::Full,
            _ => true,
        }
    }
}

/// Where the applied tier came from (exposed as
/// `capabilities.resource.preset_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TierSource {
    /// Derived from [`HardwareProfile`] by [`resolve_tier`].
    Hardware,
    /// Pinned via `[resources] tier = "..."`.
    Config,
}

impl TierSource {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Config => "config",
        }
    }
}

/// Tier inputs the planner consumes.
#[derive(Debug, Clone)]
pub struct TierContext {
    pub tier: Tier,
    pub source: TierSource,
    pub profile: HardwareProfile,
}

/// Hardware-derived tier (pure). Thresholds anchored on the three
/// resident hosts: full = 4C/16G AVX2 (i5-1135G7-class); standard =
/// 4C/8G; basic = 2-core with ≥3.5 GiB (both the i5-6200U 2C/4T/8G and
/// the i3-2370M 2C/3.7G prove the chat loop viable there); minimal =
/// the rest (2-core/≤2 GiB netbook corner). `total_mib` is MemTotal —
/// a hardware identity — not MemAvailable (boot-load sensitive).
#[must_use]
pub fn resolve_tier(profile: &HardwareProfile, total_mib: u64) -> Tier {
    if profile.physical_cores >= 4 && profile.simd >= SimdClass::Avx2 && total_mib >= 12 * 1024 {
        Tier::Full
    } else if profile.physical_cores >= 4 && total_mib >= 8 * 1024 {
        Tier::Standard
    } else if profile.physical_cores >= 2 && total_mib >= 3584 {
        Tier::Basic
    } else {
        Tier::Minimal
    }
}

/// Reasons for which the caller must build the engine with the
/// feature config-disabled (fail-open path): budget and tier cuts.
/// `off_config` and `dependency` are already non-constructible by the
/// usual paths; anything else (`""` admission) plays on.
#[must_use]
pub fn disables(reason: &str) -> bool {
    reason == REASON_OFF_BUDGET || reason == REASON_OFF_TIER
}

/// One feature's boot-admission decision (capabilities.resource entry).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeatureDecision {
    pub name: &'static str,
    /// Estimated resident cost (model files × factor + engine overhead).
    pub cost_mib: u64,
    pub admitted: bool,
    /// Machine code — `""` / `off_config` / `off_budget` / `dependency`.
    pub reason: &'static str,
}

/// The boot-time admission snapshot served via `capabilities.resource`.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceProfile {
    /// `"auto"` (budget admission) or `"all"` (explicit full boot).
    pub mode: String,
    /// Applied named tier (`minimal`/`basic`/`standard`/`full`;
    /// `full` when `mode == "all"`). Named-tier addendum.
    pub tier: String,
    /// Where the tier came from: `hardware` (auto-derived) or `config`
    /// (pinned).
    pub preset_source: String,
    /// Host hardware identity snapshot (raw inputs the tier resolved
    /// from).
    pub profile: HardwareProfile,
    pub total_mib: u64,
    /// `MemAvailable` sampled at boot (MiB).
    pub available_mib: u64,
    /// `available − reserve` in auto mode; `available` in `all` mode.
    pub budget_mib: u64,
    pub reserve_mib: u64,
    /// In priority order (`ai → audio_ai → voice → llm → tts → decision
    /// → face → ocr → meeting → vlm`).
    pub features: Vec<FeatureDecision>,
}

impl ResourceProfile {
    /// The all-features-on placeholder used by test fixtures and the
    /// legacy `run()` path before a real profile exists.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            mode: "all".into(),
            tier: Tier::Full.name().into(),
            preset_source: TierSource::Config.name().into(),
            profile: HardwareProfile::default(),
            total_mib: 0,
            available_mib: 0,
            budget_mib: 0,
            reserve_mib: 0,
            features: Vec::new(),
        }
    }
}

/// One candidate feature for the planner (pure input — no I/O).
#[derive(Debug, Clone)]
pub struct FeatureInput {
    pub name: &'static str,
    /// Master switch state from the config.
    pub enabled: bool,
    /// Model file paths (resolved relative to the working dir). Missing
    /// files contribute zero — the engine's own fail-open covers them.
    pub model_paths: Vec<String>,
    /// Fixed per-engine resident overhead (ORT arena, buffers, threads).
    pub overhead_mib: u64,
    /// Feature that must be admitted first (`meeting`/`decision` ride
    /// the voice models).
    pub depends_on: Option<&'static str>,
}

/// Estimated resident MiB for the given model files: the on-disk bytes
/// rounded up to MiB, scaled ×1.15 (ORT sessions keep weights resident;
/// llama.cpp mmap touches roughly the file over a conversation).
#[must_use]
pub fn file_cost_mib(paths: &[String]) -> u64 {
    let bytes: u64 = paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mib = bytes.div_ceil(1024 * 1024);
    (mib * 115).div_ceil(100)
}

/// Greedy priority-order admission (pure; file sizes are the only I/O,
/// via [`file_cost_mib`]).
///
/// `mode = "all"` admits every enabled feature (legacy behaviour --
/// tier irrelevant, applied tier reported as `full`); anything else is
/// treated as `"auto"`. Features disabled in config are reported
/// `off_config` and cost nothing. Names outside the applied tier's
/// wanted subset are reported `off_tier` and cost nothing (a pinned
/// tier is a preset: out-of-set `enabled = true` config is ignored
/// honestly). A feature whose dependency was not admitted is reported
/// `dependency` (and costs nothing — it will not start). Budget
/// overflow marks `off_budget` but keeps scanning: cheaper
/// lower-priority features may still fit.
#[must_use]
pub fn plan(
    inputs: &[FeatureInput],
    mode: &str,
    tier: &TierContext,
    total_mib: u64,
    available_mib: u64,
    reserve_mib: u64,
) -> ResourceProfile {
    let all = mode.eq_ignore_ascii_case("all");
    let budget = if all {
        available_mib
    } else {
        available_mib.saturating_sub(reserve_mib)
    };
    let mut used = 0_u64;
    let mut decisions = Vec::with_capacity(inputs.len());
    for f in inputs {
        let cost = file_cost_mib(&f.model_paths) + f.overhead_mib;
        if !f.enabled {
            decisions.push(FeatureDecision {
                name: f.name,
                cost_mib: cost,
                admitted: false,
                reason: REASON_OFF_CONFIG,
            });
            continue;
        }
        if all {
            decisions.push(FeatureDecision {
                name: f.name,
                cost_mib: cost,
                admitted: true,
                reason: REASON_ON,
            });
            continue;
        }
        if !tier.tier.includes(f.name) {
            decisions.push(FeatureDecision {
                name: f.name,
                cost_mib: cost,
                admitted: false,
                reason: REASON_OFF_TIER,
            });
            continue;
        }
        if let Some(dep) = f.depends_on
            && decisions.iter().any(|d| d.name == dep && !d.admitted)
        {
            decisions.push(FeatureDecision {
                name: f.name,
                cost_mib: cost,
                admitted: false,
                reason: REASON_DEPENDENCY,
            });
            continue;
        }
        let admitted = used.saturating_add(cost) <= budget;
        if admitted {
            used += cost;
        }
        decisions.push(FeatureDecision {
            name: f.name,
            cost_mib: cost,
            admitted,
            reason: if admitted {
                REASON_ON
            } else {
                REASON_OFF_BUDGET
            },
        });
    }
    ResourceProfile {
        mode: if all { "all".into() } else { "auto".into() },
        tier: if all {
            Tier::Full.name().into()
        } else {
            tier.tier.name().into()
        },
        preset_source: tier.source.name().into(),
        profile: tier.profile.clone(),
        total_mib,
        available_mib,
        budget_mib: budget,
        reserve_mib,
        features: decisions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feat(name: &'static str, enabled: bool, mib: u64) -> FeatureInput {
        FeatureInput {
            name,
            enabled,
            model_paths: vec![format!("/nonexistent/{name}.onnx")],
            overhead_mib: mib,
            depends_on: None,
        }
    }

    /// Config-pinned context (`Full` reproduces the pre-tier semantics
    /// the round-1 tests were written against).
    fn tctx(t: Tier) -> TierContext {
        TierContext {
            tier: t,
            source: TierSource::Config,
            profile: HardwareProfile::default(),
        }
    }

    #[test]
    fn everything_fits_when_budget_generous() {
        let p = plan(
            &[feat("ai", true, 10), feat("llm", true, 500)],
            "auto",
            &tctx(Tier::Full),
            8192,
            7000,
            768,
        );
        assert!(p.features.iter().all(|d| d.admitted));
        assert_eq!(p.budget_mib, 7000 - 768);
        assert_eq!(p.mode, "auto");
        assert_eq!(p.tier, "full");
        assert_eq!(p.preset_source, "config");
    }

    #[test]
    fn overflow_cuts_the_heavy_tail_not_the_head() {
        // Budget 100: ai(10)+voice(30) fit, llm(500) cut, face(20) still fits.
        let p = plan(
            &[
                feat("ai", true, 10),
                feat("voice", true, 30),
                feat("llm", true, 500),
                feat("face", true, 20),
            ],
            "auto",
            &tctx(Tier::Full),
            4096,
            868,
            768,
        );
        let by = |n: &str| p.features.iter().find(|d| d.name == n).unwrap();
        assert!(by("ai").admitted);
        assert!(by("voice").admitted);
        assert!(!by("llm").admitted);
        assert_eq!(by("llm").reason, REASON_OFF_BUDGET);
        assert!(by("face").admitted, "cheaper later feature still fits");
    }

    #[test]
    fn mode_all_admits_every_enabled_feature() {
        let p = plan(
            &[feat("llm", true, 999_999)],
            "all",
            &tctx(Tier::Minimal),
            2048,
            1024,
            768,
        );
        assert!(p.features[0].admitted);
        assert_eq!(p.mode, "all");
        assert_eq!(p.budget_mib, 1024, "all mode keeps raw available as budget");
        // All-mode boots everything regardless of the pinned tier…
        assert_eq!(p.tier, "full");
    }

    #[test]
    fn disabled_in_config_reports_off_config() {
        let p = plan(
            &[feat("vlm", false, 42)],
            "auto",
            &tctx(Tier::Full),
            8192,
            7000,
            768,
        );
        assert!(!p.features[0].admitted);
        assert_eq!(p.features[0].reason, REASON_OFF_CONFIG);
    }

    #[test]
    fn meeting_and_decision_follow_voice() {
        let voice = feat("voice", true, 600);
        let mut meeting = feat("meeting", true, 5);
        meeting.depends_on = Some("voice");
        // Budget 100 < voice(600) → voice cut, meeting dependency-cut.
        let p = plan(&[voice, meeting], "auto", &tctx(Tier::Full), 2048, 868, 768);
        assert!(!p.features[0].admitted);
        assert!(!p.features[1].admitted);
        assert_eq!(p.features[1].reason, REASON_DEPENDENCY);
    }

    #[test]
    fn missing_model_files_cost_overhead_only() {
        // /nonexistent paths: file bytes 0 → cost == overhead.
        assert_eq!(file_cost_mib(&["/nonexistent/x".into()]), 0);
        let p = plan(
            &[feat("ai", true, 40)],
            "auto",
            &tctx(Tier::Full),
            2048,
            800,
            768,
        );
        assert_eq!(p.features[0].cost_mib, 40);
    }

    #[test]
    fn file_cost_scales_and_rounds_up() {
        let dir = std::env::temp_dir().join("mibee-res-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("cost.bin");
        // 2 MiB + 1 byte → ceil = 3 MiB → ×1.15 → 3.45 → 4.
        let v = vec![0_u8; 2 * 1024 * 1024 + 1];
        std::fs::write(&f, &v).unwrap();
        assert_eq!(file_cost_mib(&[f.to_string_lossy().into_owned()]), 4);
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn zero_budget_admits_nothing_enabled() {
        let p = plan(
            &[feat("ai", true, 40)],
            "auto",
            &tctx(Tier::Full),
            1024,
            100,
            768,
        );
        assert_eq!(p.budget_mib, 0);
        assert!(!p.features[0].admitted);
        assert_eq!(p.features[0].reason, REASON_OFF_BUDGET);
    }

    // ---- named-tier ladder (appendix A #40 named-tier addendum) ----

    #[test]
    fn tier_thresholds_match_anchor_hosts() {
        let mk = |cores, simd, battery: bool| HardwareProfile {
            physical_cores: cores,
            simd,
            battery,
        };
        // Workstation i5-1135G7: 4 physical cores, AVX2 + VNNI, ~15.7 GiB → full.
        assert_eq!(
            resolve_tier(&mk(4, SimdClass::Avx512Vnni, false), 16 * 1024),
            Tier::Full
        );
        // Skylake i5-6200U: 2 physical cores (2C/4T), AVX2, 8 GiB → basic.
        assert_eq!(
            resolve_tier(&mk(2, SimdClass::Avx2, false), 8192),
            Tier::Basic
        );
        // Sandy Bridge i3-2370M: 2 physical cores, baseline SIMD, ~3.7 GiB —
        // the chat loop (voice + lite LLM) is proven viable there → basic.
        assert_eq!(
            resolve_tier(&mk(2, SimdClass::Baseline, true), 3788),
            Tier::Basic
        );
        // 4-core AVX2 below the 12 GiB head → standard.
        assert_eq!(
            resolve_tier(&mk(4, SimdClass::Avx2, false), 8192),
            Tier::Standard
        );
        // 4 wide-ish cores on baseline SIMD → still standard, never full.
        assert_eq!(
            resolve_tier(&mk(4, SimdClass::Baseline, false), 8192),
            Tier::Standard
        );
        // 2-core netbook with 2 GiB → minimal.
        assert_eq!(
            resolve_tier(&mk(2, SimdClass::Baseline, false), 2048),
            Tier::Minimal
        );
        // A battery never lifts the tier (informational only).
        assert_eq!(
            resolve_tier(&mk(2, SimdClass::Baseline, true), 3788),
            Tier::Basic
        );
        // Single vCPU VM → minimal.
        assert_eq!(
            resolve_tier(&mk(1, SimdClass::Avx2, false), 16 * 1024),
            Tier::Minimal
        );
    }

    #[test]
    fn tier_names_round_trip() {
        for t in [Tier::Minimal, Tier::Basic, Tier::Standard, Tier::Full] {
            assert_eq!(Tier::from_name(t.name()), Some(t));
        }
        assert_eq!(Tier::from_name("auto"), None);
        assert_eq!(Tier::from_name(""), None);
    }

    #[test]
    fn tier_feature_sets_ladder() {
        for name in ["ai", "audio_ai"] {
            assert!(Tier::Minimal.includes(name));
            assert!(Tier::Full.includes(name));
        }
        for name in ["voice", "llm", "tts", "decision"] {
            assert!(!Tier::Minimal.includes(name), "{name} not in minimal");
            assert!(Tier::Basic.includes(name));
            assert!(Tier::Standard.includes(name));
            assert!(Tier::Full.includes(name));
        }
        for name in ["face", "ocr"] {
            assert!(!Tier::Basic.includes(name), "{name} not in basic");
            assert!(Tier::Standard.includes(name));
            assert!(Tier::Full.includes(name));
        }
        for name in ["meeting", "vlm"] {
            assert!(!Tier::Standard.includes(name), "{name} not in standard");
            assert!(Tier::Full.includes(name));
        }
        // Unknown forward-compat names are never tier-scoped.
        assert!(Tier::Minimal.includes("future_thing"));
    }

    #[test]
    fn simd_flags_classification_and_cpuinfo_parsing() {
        assert_eq!(
            SimdClass::from_flags("fpu vme sse2 avx2 fma"),
            SimdClass::Avx2
        );
        assert_eq!(
            SimdClass::from_flags("sse2 avx512f avx512_vnni"),
            SimdClass::Avx512Vnni
        );
        assert_eq!(SimdClass::from_flags("sse4_2"), SimdClass::Baseline);
        assert_eq!(SimdClass::Baseline.name(), "baseline");
        assert!(SimdClass::Baseline < SimdClass::Avx2);
        assert!(SimdClass::Avx2 < SimdClass::Avx512Vnni);

        // Two sockets × 4 cores × SMT siblings dedup → 8 physical cores.
        let mut s = String::new();
        for socket in 0..2 {
            for core in 0..4_u32 {
                for thread in 0..2 {
                    let idx = socket * 8 + core * 2 + thread;
                    s.push_str(&format!("processor\t: {idx}\n"));
                    s.push_str(&format!("physical id\t: {socket}\n"));
                    s.push_str(&format!("core id\t: {core}\n"));
                    s.push_str("flags\t: fpu sse2 avx2 fma\n\n");
                }
            }
        }
        let c = parse_cpuinfo(&s);
        assert_eq!(c.physical_cores, 8);
        assert_eq!(c.simd, SimdClass::Avx2, "first flags line wins");

        // VM without physical-id/core-id fields → logical processor count.
        let vm = "processor : 0\nflags : sse2\n\nprocessor : 1\nflags : sse2\n\n";
        let c = parse_cpuinfo(vm);
        assert_eq!(c.physical_cores, 2);
        assert_eq!(c.simd, SimdClass::Baseline);

        // Empty /proc/cpuinfo → zero cores, baseline.
        let c = parse_cpuinfo("");
        assert_eq!(c.physical_cores, 0);
        assert_eq!(c.simd, SimdClass::Baseline);
    }

    #[test]
    fn battery_is_detected_from_supply_types() {
        assert!(battery_powered(&["Mains", "Battery"]));
        assert!(battery_powered(&["UPS".to_string(), "battery".to_string()]));
        assert!(!battery_powered(&["Mains", "UPS"]));
        assert!(!battery_powered::<&str>(&[]));
        assert_eq!(
            HardwareProfile {
                physical_cores: 2,
                simd: SimdClass::Baseline,
                battery: true,
            },
            HardwareProfile {
                physical_cores: 2,
                simd: SimdClass::Baseline,
                battery: true,
            }
        );
        assert_eq!(
            HardwareProfile::default(),
            HardwareProfile {
                physical_cores: 0,
                simd: SimdClass::Baseline,
                battery: false,
            }
        );
    }

    #[test]
    fn pinned_tier_excludes_features_with_off_tier() {
        let p = plan(
            &[
                feat("ai", true, 10),
                feat("llm", true, 500),
                feat("vlm", true, 5),
            ],
            "auto",
            &tctx(Tier::Minimal),
            8192,
            7000,
            768,
        );
        let by = |n: &str| p.features.iter().find(|d| d.name == n).unwrap();
        assert!(by("ai").admitted, "sensing stays in minimal");
        assert!(!by("llm").admitted);
        assert_eq!(by("llm").reason, REASON_OFF_TIER);
        assert_eq!(p.tier, "minimal");
        assert_eq!(p.preset_source, "config");
        // Excluded features cost nothing against the budget.
        assert_eq!(p.budget_mib, 7000 - 768);
    }

    #[test]
    fn tier_exclusions_report_off_tier_for_every_unwanted_name() {
        // minimal wants only ai/audio_ai; llm and tts (later in the fixed
        // priority ladder) are both excluded regardless of budget room.
        let p = plan(
            &[feat("llm", true, 900), feat("tts", true, 20)],
            "auto",
            &tctx(Tier::Minimal),
            8192,
            900,
            0,
        );
        let by = |n: &str| p.features.iter().find(|d| d.name == n).unwrap();
        assert!(!by("llm").admitted);
        assert_eq!(by("llm").reason, REASON_OFF_TIER);
        assert!(!by("tts").admitted);
        assert_eq!(by("tts").reason, REASON_OFF_TIER);
    }

    #[test]
    fn tier_check_precedes_dependency_check() {
        // minimal excludes both voice and meeting: meeting reports
        // off_tier even though voice (its dependency) is also cut.
        let mut meeting = feat("meeting", true, 5);
        meeting.depends_on = Some("voice");
        let p = plan(
            &[feat("voice", true, 600), meeting],
            "auto",
            &tctx(Tier::Minimal),
            2048,
            868,
            768,
        );
        let by = |n: &str| p.features.iter().find(|d| d.name == n).unwrap();
        assert_eq!(by("voice").reason, REASON_OFF_TIER);
        assert_eq!(by("meeting").reason, REASON_OFF_TIER);

        // Wanted tier + cut dependency → dependency reason (unchanged).
        let mut meeting = feat("meeting", true, 5);
        meeting.depends_on = Some("voice");
        let p = plan(
            &[feat("voice", true, 600), meeting],
            "auto",
            &tctx(Tier::Full),
            2048,
            868,
            768,
        );
        let by = |n: &str| p.features.iter().find(|d| d.name == n).unwrap();
        assert_eq!(by("voice").reason, REASON_OFF_BUDGET);
        assert_eq!(by("meeting").reason, REASON_DEPENDENCY);
    }

    #[test]
    fn hardware_context_reports_hardware_source() {
        let ctx = TierContext {
            tier: Tier::Standard,
            source: TierSource::Hardware,
            profile: HardwareProfile {
                physical_cores: 4,
                simd: SimdClass::Avx2,
                battery: true,
            },
        };
        let p = plan(
            &[feat("ai", true, 1), feat("meeting", true, 5)],
            "auto",
            &ctx,
            8192,
            7000,
            768,
        );
        assert_eq!(p.tier, "standard");
        assert_eq!(p.preset_source, "hardware");
        assert!(p.profile.battery);
        assert_eq!(p.profile.physical_cores, 4);
        // standard excludes meeting despite the generous budget.
        let by = |n: &str| p.features.iter().find(|d| d.name == n).unwrap();
        assert_eq!(by("meeting").reason, REASON_OFF_TIER);
    }

    #[test]
    fn disables_covers_budget_and_tier_cuts() {
        assert!(disables(REASON_OFF_BUDGET));
        assert!(disables(REASON_OFF_TIER));
        assert!(!disables(REASON_ON));
        assert!(!disables(REASON_OFF_CONFIG));
        assert!(!disables(REASON_DEPENDENCY));
    }
}
