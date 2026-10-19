use std::collections::HashMap;
use tracing_loki::BackgroundTask;

/// Runtime configuration for the optional Loki log shipping layer.
///
/// Constructed from the `[observability.logs]` TOML section (see `RemoteLogConfig`
/// in `src/config.rs`), then passed into [`build_loki_layer`].
pub struct LokiConfig {
    pub endpoint: String,
    pub batch_size: usize,
    pub flush_interval_secs: u64,
    pub labels: HashMap<String, String>,
}

/// Build a [`tracing_loki::Layer`] and its [`BackgroundTask`] from the given
/// [`LokiConfig`].
///
/// This function is **fail-open**: if the endpoint URL is malformed, label
/// validation fails, or any other construction error occurs, a warning is
/// emitted via `eprintln` (tracing is not yet initialised) and `None` is
/// returned. The caller should continue with fallback logging (stdout-only).
pub fn build_loki_layer(config: LokiConfig) -> Option<(tracing_loki::Layer, BackgroundTask)> {
    // If the endpoint is empty, treat it as "not configured" and bail silently.
    if config.endpoint.is_empty() {
        return None;
    }

    let url = match tracing_loki::url::Url::parse(&config.endpoint) {
        Ok(u) => u,
        Err(e) => {
            eprintln!(
                "WARN: Failed to parse Loki endpoint URL '{}': {}. \
                 Remote log shipping disabled.",
                config.endpoint, e
            );
            return None;
        }
    };

    let mut builder = tracing_loki::builder();

    // Add configured labels.
    for (key, value) in &config.labels {
        match builder.clone().label(key.as_str(), value.as_str()) {
            Ok(b) => builder = b,
            Err(e) => {
                eprintln!(
                    "WARN: Invalid Loki label '{}={}': {}. Skipping label.",
                    key, value, e
                );
                // Continue with remaining labels — fail sub-element, not the
                // whole layer.
            }
        }
    }

    match builder.build_url(url) {
        Ok((layer, task)) => Some((layer, task)),
        Err(e) => {
            eprintln!(
                "WARN: Failed to build Loki layer for endpoint '{}': {}. \
                 Remote log shipping disabled.",
                config.endpoint, e
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str, labels: &[(&str, &str)]) -> LokiConfig {
        LokiConfig {
            endpoint: endpoint.into(),
            batch_size: 10,
            flush_interval_secs: 1,
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        }
    }

    #[test]
    fn empty_endpoint_is_not_configured() {
        assert!(build_loki_layer(config("", &[])).is_none());
    }

    #[test]
    fn malformed_endpoint_fails_open_to_none() {
        assert!(build_loki_layer(config("not a url", &[])).is_none());
    }

    #[test]
    fn valid_endpoint_with_good_labels_builds() {
        let built = build_loki_layer(config(
            "http://loki.local:3100/loki/api/v1/push",
            &[("host", "cam1")],
        ));
        assert!(built.is_some(), "valid endpoint + valid label must build");
    }

    #[test]
    fn invalid_label_is_skipped_not_fatal() {
        // Loki label names are restricted ([a-zA-Z_][a-zA-Z0-9_]*); a bad
        // one must drop that label while the layer still builds.
        let built = build_loki_layer(config(
            "http://loki.local:3100/loki/api/v1/push",
            &[("1bad-name!", "x"), ("host", "cam1")],
        ));
        assert!(built.is_some(), "one bad label must not kill the layer");
    }
}
