//! Local-config migration (issue #4): merge shipped defaults into a
//! deployed `config.local.toml` **without clobbering operator values**.
//!
//! Deployments drifted for two reasons: `deploy.sh` used to overwrite
//! `config.local.toml` with the shipped `config.toml` on every deploy
//! (now it only seeds the file when absent), and new config sections
//! landed silently via serde defaults — operators never saw them. The
//! explicit `--migrate-config` command merges every *missing* key from
//! the shipped defaults into the local file, keeps every existing value
//! untouched, writes at most one `.bak` alongside, and reports what it
//! added. Idempotent by construction.

use std::collections::BTreeSet;
use std::path::Path;

/// What one migration run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Keys added to the local file, dotted (`gb28181.alarm_cooldown_secs`).
    pub added: BTreeSet<String>,
    /// The backup path written before the first change (None when the
    /// file already matched or did not exist).
    pub backup: Option<String>,
}

/// Deep-merge `defaults` into `local`: every key present only in
/// `defaults` is copied (tables recurse); existing local values always
/// win. Returns the merged table and the dotted paths that were added.
pub fn merge_tables(
    defaults: &toml::Value,
    local: &mut toml::value::Table,
    prefix: &str,
    added: &mut BTreeSet<String>,
) {
    let Some(def_table) = defaults.as_table() else {
        return;
    };
    for (key, def_val) in def_table {
        match local.get_mut(key) {
            Some(existing) => {
                // Recurse only when both sides are tables; a scalar
                // (or type-mismatched) local value is the operator's
                // choice and stays untouched.
                if let (Some(def_inner), Some(loc_inner)) =
                    (def_val.as_table(), existing.as_table_mut())
                {
                    let child_prefix = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    merge_tables(
                        &toml::Value::Table(def_inner.clone()),
                        loc_inner,
                        &child_prefix,
                        added,
                    );
                }
            }
            None => {
                let dotted = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                added.insert(dotted);
                local.insert(key.clone(), def_val.clone());
            }
        }
    }
}

/// One migration pass. `defaults_path` is the shipped `config.toml`;
/// `local_path` the live override file. Missing local file = seed it
/// wholesale from the defaults (report lists the top-level sections).
///
/// # Errors
///
/// Unreadable defaults, unparsable TOML on either side, or a failed
/// write — the local file is left untouched on any error before the
/// atomic rename.
pub fn migrate_local_config(
    defaults_path: &Path,
    local_path: &Path,
) -> anyhow::Result<MigrationReport> {
    let defaults_raw = std::fs::read_to_string(defaults_path)
        .map_err(|e| anyhow::anyhow!("defaults {}: {e}", defaults_path.display()))?;
    let defaults: toml::Value = toml::from_str(&defaults_raw)
        .map_err(|e| anyhow::anyhow!("defaults not valid TOML: {e}"))?;

    let mut report = MigrationReport::default();
    let new_contents = match std::fs::read_to_string(local_path) {
        Ok(local_raw) => {
            let mut local: toml::Value = toml::from_str(&local_raw)
                .map_err(|e| anyhow::anyhow!("local config not valid TOML: {e}"))?;
            let Some(table) = local.as_table_mut() else {
                anyhow::bail!("local config is not a TOML table");
            };
            merge_tables(&defaults, table, "", &mut report.added);
            if report.added.is_empty() {
                return Ok(report); // already current — no write, no backup
            }
            // Backup before the first change (idempotent runs after this
            // write report zero additions and never touch the .bak).
            let backup = local_path.with_extension("toml.bak-migrate");
            std::fs::write(&backup, &local_raw)
                .map_err(|e| anyhow::anyhow!("backup {}: {e}", backup.display()))?;
            report.backup = Some(backup.to_string_lossy().into_owned());
            toml::to_string_pretty(&local)?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Seed: the whole defaults file becomes the local file.
            if let Some(table) = defaults.as_table() {
                for key in table.keys() {
                    report.added.insert(key.clone());
                }
            }
            defaults_raw
        }
        Err(e) => anyhow::bail!("local config {}: {e}", local_path.display()),
    };
    std::fs::write(local_path, new_contents)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", local_path.display()))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mibee-cfgm-{}-{}-{:?}",
            std::process::id(),
            name,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("toml.bak-migrate"));
        p
    }

    fn write(path: &std::path::PathBuf, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn missing_sections_are_added_existing_values_win() {
        let defaults = scratch("def");
        let local = scratch("loc");
        write(
            &defaults,
            "[web]\nport = 8443\nhost = \"0.0.0.0\"\n\n[gb28181]\nenabled = false\nalarm_cooldown_secs = 30\n",
        );
        write(
            &local,
            "[web]\nport = 9999\n\n[away]\nsnapshot_dir = \"mine\"\n",
        );
        let report = migrate_local_config(&defaults, &local).unwrap();
        // web.port kept, web.host added; whole gb28181 section added.
        assert!(report.added.contains("web.host"));
        assert!(report.added.contains("gb28181"));
        assert!(
            !report.added.contains("web.port"),
            "operator value must not be reported"
        );
        assert!(report.backup.is_some());
        let merged = std::fs::read_to_string(&local).unwrap();
        assert!(merged.contains("9999"), "operator value preserved");
        assert!(merged.contains("alarm_cooldown_secs = 30"));
        assert!(
            merged.contains("mine"),
            "unknown-to-defaults local section preserved"
        );
    }

    #[test]
    fn idempotent_second_run_is_a_no_op() {
        let defaults = scratch("def2");
        let local = scratch("loc2");
        write(&defaults, "[web]\nport = 8443\n");
        write(&local, "[away]\na = 1\n");
        let first = migrate_local_config(&defaults, &local).unwrap();
        assert!(!first.added.is_empty());
        let second = migrate_local_config(&defaults, &local).unwrap();
        assert_eq!(
            second,
            MigrationReport::default(),
            "second run adds nothing"
        );
    }

    #[test]
    fn absent_local_file_is_seeded_wholesale() {
        let defaults = scratch("def3");
        let local = scratch("loc3"); // never written
        write(&defaults, "[web]\nport = 8443\n\n[ai]\nenabled = false\n");
        let report = migrate_local_config(&defaults, &local).unwrap();
        assert_eq!(
            report.added,
            ["ai", "web"].into_iter().map(String::from).collect(),
            "seeded sections reported"
        );
        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            "[web]\nport = 8443\n\n[ai]\nenabled = false\n"
        );
        let _ = std::fs::remove_file(&local);
    }
}
