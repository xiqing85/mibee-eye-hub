//! Documentation conformance for the configuration reference.
//!
//! `docs/en/configuration.md` and `docs/zh/configuration.md` are the only
//! places a user learns what a config key defaults to. Nothing in the build
//! connected them to the code, so they drifted silently — the two defaults
//! fixed in this file's own history (`llm.n_ctx` documented as 1024 while the
//! code had been 2048 for months, and `n_threads` documented as a fixed `2`
//! when the code derives it from the core count) were both invisible.
//!
//! These tests close that loop by reading the struct sources and the doc
//! tables and comparing them. They are deliberately source-level rather than
//! reflection-based: the `Default` impls compute some values from
//! `available_parallelism()` and from other config fields, and a runtime
//! comparison could not distinguish "computed" from "wrong".

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `pub struct XxxConfig { … }` → field names, from every workspace crate.
fn struct_fields() -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    visit(&repo_root().join("src"), &mut out);
    for crate_name in ["streaming", "web", "capture", "security", "protocols"] {
        let dir = repo_root().join("crates").join(crate_name).join("src");
        if dir.exists() {
            visit(&dir, &mut out);
        }
    }
    out
}

fn visit(dir: &Path, out: &mut BTreeMap<String, BTreeSet<String>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit(&path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Struct bodies here are flat (no nested braces in the config
        // structs), so a non-greedy body up to the first `}\n` is enough.
        for cap in text.match_indices("pub struct ") {
            let tail = &text[cap.0 + "pub struct ".len()..];
            let Some(name_end) = tail.find(|c: char| !(c.is_alphanumeric() || c == '_')) else {
                continue;
            };
            let name = tail[..name_end].to_string();
            // `*Settings`/`*Manifest` are the same role under different
            // suffixes (WatermarkSettings backs [watermark] via re-export;
            // PluginManifest backs [plugins], SPEC appendix A #47).
            if !(name.ends_with("Config")
                || name.ends_with("Settings")
                || name.ends_with("Manifest"))
            {
                continue;
            }
            let Some(open) = tail.find('{') else { continue };
            let Some(close) = tail[open..].find("\n}") else {
                continue;
            };
            let body = &tail[open..open + close];
            let fields: BTreeSet<String> = body
                .lines()
                .filter_map(|l| l.trim().strip_prefix("pub "))
                .filter_map(|l| l.split(':').next())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .collect();
            if !fields.is_empty() {
                out.insert(name, fields);
            }
        }
    }
}

/// Every `field: <literal>,` inside `impl Default for <Struct> { … }`.
fn default_values(struct_name: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for dir in [
        repo_root().join("src"),
        repo_root().join("crates/streaming/src"),
        repo_root().join("crates/web/src"),
        repo_root().join("crates/capture/src"),
    ] {
        collect_defaults(&dir, struct_name, &mut out);
    }
    out
}

fn collect_defaults(dir: &Path, struct_name: &str, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_defaults(&path, struct_name, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let needle = format!("impl Default for {struct_name} {{");
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(&needle) {
            let start = from + rel + needle.len();
            let Some(len) = text[start..].find("\n    }") else {
                break;
            };
            let body = &text[start..start + len];
            for line in body.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix("Self {") else {
                    if let Some(inner) = line.strip_suffix(",") {
                        let Some((name, value)) = inner.split_once(':') else {
                            continue;
                        };
                        let name = name.trim();
                        let value = value.trim();
                        // Only simple, fully-literal defaults are comparable
                        // against a doc table. Anything computed (a function
                        // call, a field reference, a format!) is skipped.
                        if value.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '"' | '-' | ' ')
                        }) && !value.is_empty()
                        {
                            out.insert(name.to_string(), value.to_string());
                        }
                    }
                    continue;
                };
                let Some((name, value)) = rest.split_once(':') else {
                    continue;
                };
                // The last field of a `Self { … }` body is followed by
                // `,` and then the closing brace on the same line.
                let value = value
                    .trim()
                    .trim_end_matches(',')
                    .trim_end_matches('}')
                    .trim();
                out.insert(name.trim().to_string(), value.to_string());
            }
            from = start + len;
        }
    }
}

/// The documented table rows of one `**Field Reference:**` block, keyed by the
/// `## [\`section\`]` heading it sits under.
fn documented_defaults(doc: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current = String::new();
    for line in doc.lines() {
        // Section headings are `### [name] - Description`; the reference sits
        // one `##` level down from the page title.
        if let Some(rest) = line.strip_prefix("### [") {
            current = rest
                .split(']')
                .next()
                .unwrap_or("")
                .trim_matches('`')
                .to_string();
            sections.entry(current.clone()).or_default();
            continue;
        }
        if line.starts_with("## ") {
            current.clear();
            continue;
        }
        // `| `field` | type | `default` | description |`
        let Some(cells) = parse_row(line) else {
            continue;
        };
        if cells.len() < 3 {
            continue;
        }
        let key = cells[0].trim_matches('`').to_string();
        // Skip markdown table headers — they are not config keys and would
        // otherwise read as keys that exist in no struct.
        if key.is_empty()
            || !key.chars().all(|c| c.is_alphanumeric() || c == '_')
            || matches!(key.as_str(), "Field" | "Key" | "字段" | "键")
        {
            continue;
        }
        sections
            .entry(current.clone())
            .or_default()
            .insert(key, cells[2].trim_matches('`').to_string());
    }
    sections
}

fn parse_row(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if !line.starts_with('|') || !line.ends_with('|') {
        return None;
    }
    Some(
        line.trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect(),
    )
}

/// Normalise a Rust literal and a markdown cell to the same shape so a
/// comparison is about the *value*, not the punctuation.
fn normalise(cell: &str) -> String {
    let mut s = cell.trim().trim_matches('`').trim().to_string();
    // `true`.into() / "x".into() → true / "x"
    for suffix in [".into()", ".to_string()", ".to_owned()"] {
        if let Some(base) = s.strip_suffix(suffix) {
            s = base.to_string();
        }
    }
    s = s.trim().to_string();
    // "0.35" == 0.35
    if let Ok(f) = s.parse::<f64>() {
        return format!("{f}");
    }
    s
}

#[test]
fn every_documented_config_key_exists_in_the_code() {
    let fields = struct_fields();
    let all: BTreeSet<String> = fields.values().flatten().cloned().collect();
    assert!(
        fields.len() >= 20,
        "only found {} Config structs — the source scanner drifted and this \
         test would pass vacuously",
        fields.len()
    );

    for locale in ["en", "zh"] {
        let path = repo_root().join(format!("docs/{locale}/configuration.md"));
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let sections = documented_defaults(&doc);
        let mut unknown: Vec<String> = Vec::new();
        for (section, keys) in &sections {
            for key in keys.keys() {
                if !all.contains(key) {
                    unknown.push(format!("docs/{locale}: [{section}] {key}"));
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "configuration.md documents keys that exist in no Config struct:\n  {}",
            unknown.join("\n  ")
        );
    }
}

/// The value-level check that would have caught `n_ctx = 1024` vs `2048` and
/// a hardcoded `n_threads = 2` against a core-derived one.
///
/// Only compared for keys whose code default is a plain literal: a default
/// that calls a function or reads another field is genuinely not a constant
/// and belongs in prose, not a table cell, so those are skipped rather than
/// reported.
#[test]
fn documented_literal_defaults_match_the_code() {
    let struct_fields = struct_fields();
    let doc = std::fs::read_to_string(repo_root().join("docs/en/configuration.md"))
        .expect("configuration.md");
    let sections = documented_defaults(&doc);

    let mut compared = 0usize;
    let mut mismatches: Vec<String> = Vec::new();

    for (section, keys) in &sections {
        // Find every struct whose name plausibly matches this section.
        let section_stem = section.trim_start_matches('[').to_string();
        let candidates: Vec<&String> = struct_fields
            .keys()
            .filter(|s| {
                s.eq_ignore_ascii_case(&section_stem)
                    || s.to_lowercase().contains(&section_stem.to_lowercase())
            })
            .collect();
        let Some(struct_name) = candidates.first() else {
            continue;
        };
        let defaults = default_values(struct_name);
        let declared = &struct_fields[*struct_name];

        for (key, doc_default) in keys {
            if !declared.contains(key) {
                continue;
            }
            let Some(code_default) = defaults.get(key) else {
                continue;
            };
            // Skip anything computed.
            if code_default.contains('(')
                || code_default.contains("String::new")
                || code_default.contains("Vec::new")
            {
                continue;
            }
            let doc_n = normalise(doc_default);
            let code_n = normalise(code_default);
            if doc_n.is_empty() || doc_n.contains(' ') {
                continue;
            }
            compared += 1;
            if doc_n != code_n {
                mismatches.push(format!(
                    "[{section}] {key}: docs say `{doc_n}`, code says `{code_n}`"
                ));
            }
        }
    }

    assert!(
        compared >= 25,
        "only {compared} defaults were comparable — the parser drifted and \
         this test would pass vacuously"
    );
    assert!(
        mismatches.is_empty(),
        "configuration.md defaults disagree with the code:\n  {}",
        mismatches.join("\n  ")
    );
}

// ── Reverse conformance: code → docs (issue #5 second half) ────────────────
//
// The forward test above catches keys documented that exist nowhere; these
// three catch the opposite drift: a section or field added to the config
// structs with no documentation at all.

/// Top-level `[section]` → the Config struct(s) that back it. This list is
/// asserted to equal AppConfig's field set, so adding a config section
/// without touching this map (or the docs) fails here.
const SECTION_STRUCTS: &[(&str, &[&str])] = &[
    ("web", &["WebConfig"]),
    ("rtsp", &["RtspConfig"]),
    ("rtmp_push", &["RtmpPushConfig"]),
    ("capture", &["CaptureConfig"]),
    ("security", &["SecurityConfig"]),
    ("resources", &["ResourcesConfig"]),
    ("conversations", &["ConversationsConfig"]),
    ("agent", &["AgentConfig"]),
    ("plugins", &["PluginManifest"]),
    ("away", &["AwayConfig"]),
    ("desktop", &["DesktopConfig"]),
    ("observability", &["ObservabilityConfig", "RemoteLogConfig"]),
    ("onvif", &["OnvifConfig"]),
    ("gb28181", &["Gb28181Config"]),
    ("recording", &["RecordingConfig"]),
    ("watermark", &["WatermarkSettings"]),
    ("webrtc", &["WebRtcConfig"]),
    ("ai", &["AiConfig"]),
    ("audio_ai", &["AudioAiConfig"]),
    ("voice", &["VoiceConfig"]),
    ("llm", &["LlmConfig"]),
    ("decision", &["DecisionConfig"]),
    ("meeting", &["MeetingConfig"]),
    ("tts", &["TtsConfig"]),
    ("tools", &["ToolsConfig"]),
    ("vlm", &["VlmConfig"]),
    ("face", &["FaceConfig"]),
    ("models", &["ModelsConfig"]),
    ("ocr", &["OcrConfig"]),
    ("database", &["DatabaseConfig"]),
];

/// Struct fields documented somewhere other than the section's field table
/// (nested sub-tables use dotted keys the flat row parser cannot see, or
/// array-of-tables syntax), with the reason.
const EXEMPT_FIELDS: &[(&str, &[&str])] = &[
    // Nested sub-struct, documented as a nested key block in the section.
    ("gb28181", &["gb35114"]),
    // Documented as `[[agent.mcp_servers]]` array-of-tables in prose+toml.
    ("agent", &["mcp_servers"]),
    // M5 routing family: nested sub-tables ([models.chat] provider/
    // fallback…), dotted-key form invisible to the flat row parser.
    ("models", &["chat", "vision", "asr", "tts", "routing"]),
    // M6 plugin manifests: [plugin.<id>] arbitrary-key map + nested
    // trigger tables — no flat field table applies (SPEC appendix A #47).
    ("plugins", &["version", "tools", "triggers", "permissions"]),
];

#[test]
fn section_map_covers_exactly_the_appconfig_fields() {
    let app = struct_fields()
        .get("AppConfig")
        .expect("AppConfig must be scannable")
        .clone();
    let mapped: BTreeSet<&str> = SECTION_STRUCTS.iter().map(|(s, _)| *s).collect();
    let actual: BTreeSet<&str> = app.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        mapped, actual,
        "SECTION_STRUCTS and AppConfig drifted — new sections must be mapped \
         AND documented (docs/<locale>/configuration.md)"
    );
}

#[test]
fn every_config_section_is_documented() {
    for locale in ["en", "zh"] {
        let path = repo_root().join(format!("docs/{locale}/configuration.md"));
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let documented: BTreeSet<String> = documented_defaults(&doc)
            .into_keys()
            .filter(|s| !s.is_empty())
            .collect();
        let mut missing: Vec<String> = Vec::new();
        for (section, _) in SECTION_STRUCTS {
            if !documented.contains(*section) {
                missing.push(format!("docs/{locale}: no [{section}] section"));
            }
        }
        assert!(
            missing.is_empty(),
            "configuration.md is missing sections:\n  {}",
            missing.join("\n  ")
        );
    }
}

#[test]
fn every_struct_field_is_documented() {
    let fields = struct_fields();
    for locale in ["en", "zh"] {
        let path = repo_root().join(format!("docs/{locale}/configuration.md"));
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let sections = documented_defaults(&doc);
        let mut missing: Vec<String> = Vec::new();
        for (section, structs) in SECTION_STRUCTS {
            let Some(doc_keys) = sections.get(*section) else {
                continue; // covered by every_config_section_is_documented
            };
            let exempt: BTreeSet<&str> = EXEMPT_FIELDS
                .iter()
                .filter(|(s, _)| *s == *section)
                .flat_map(|(_, fs)| fs.iter().copied())
                .collect();
            for struct_name in *structs {
                let Some(declared) = fields.get(*struct_name) else {
                    panic!("scanner cannot see {struct_name} — fix the map or scanner");
                };
                for field in declared {
                    if doc_keys.contains_key(field) || exempt.contains(field.as_str()) {
                        continue;
                    }
                    missing.push(format!(
                        "docs/{locale}: [{section}] {struct_name}.{field} has no doc row"
                    ));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "config struct fields without documentation (add a row or EXEMPT_FIELDS entry):\n  {}",
            missing.join("\n  ")
        );
    }
}
