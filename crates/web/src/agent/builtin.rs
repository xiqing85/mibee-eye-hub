//! Built-in tools (SPEC appendix A #43): device capabilities every
//! deployment gets without any MCP server. All fail-open — an
//! unconfigured built-in either disappears from the registry (weather
//! without a city) or reports an honest error text the model relays.

use super::{ToolOutput, ToolSpec};
use crate::server::SharedTools;
use crate::stream_manager::StreamManager;
use serde_json::json;
use std::sync::Arc;

pub(crate) const TIME_TOOL: &str = "time.now";
pub(crate) const WEATHER_TOOL: &str = "weather.current";
pub(crate) const SNAPSHOT_TOOL: &str = "camera.snapshot";

/// The built-in tool list for this host right now.
pub fn specs(shared: &SharedTools) -> Vec<ToolSpec> {
    let mut out = vec![ToolSpec {
        name: TIME_TOOL.into(),
        description:
            "获取设备本地当前日期时间与星期、时区偏移。任何关于现在几点/今天几号的问题都应调用它。"
                .into(),
        input_schema: json!({"type": "object", "properties": {}}),
        source: "builtin".into(),
    }];
    let tools = shared.read().expect("tools config lock poisoned").clone();
    if !tools.weather_city.is_empty() {
        out.push(ToolSpec {
            name: WEATHER_TOOL.into(),
            description: format!(
                "查询当前天气（气温/体感/湿度/风速）。设备配置城市为 {}；不要向用户编造天气数据，一律调用本工具。",
                tools.weather_city
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string", "description": "可选：查询城市；缺省用设备配置城市" }
                }
            }),
            source: "builtin".into(),
        });
    }
    out.push(ToolSpec {
        name: SNAPSHOT_TOOL.into(),
        description:
            "抓取相机当前画面快照，返回抓取时间与查看地址。用户想看现在画面/门口情况时可调用。"
                .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "camera_id": { "type": "string", "description": "可选：相机 id，缺省用默认相机" }
            }
        }),
        source: "builtin".into(),
    });
    out
}

/// Dispatch a built-in call. `Ok(None)` = not a built-in name (the
/// registry falls through to MCP servers); `Err` = built-in that failed
/// (honest error text for the model).
pub async fn dispatch(
    name: &str,
    args: &serde_json::Value,
    shared: &SharedTools,
    streams: &Arc<StreamManager>,
) -> anyhow::Result<Option<ToolOutput>> {
    match name {
        TIME_TOOL => Ok(Some(time_now())),
        WEATHER_TOOL => weather(args, shared).await.map(Some),
        SNAPSHOT_TOOL => snapshot(args, streams).await.map(Some),
        _ => Ok(None),
    }
}

fn time_now() -> ToolOutput {
    let now = chrono::Local::now();
    const WEEKDAYS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];
    let weekday_idx: usize = now
        .format("%u")
        .to_string()
        .parse()
        .unwrap_or(1)
        .clamp(1, 7);
    ToolOutput {
        text: format!(
            "{}（星期{}，UTC{}）",
            now.format("%Y-%m-%d %H:%M:%S"),
            WEEKDAYS[weekday_idx - 1],
            now.format("%:z"),
        ),
        media_url: None,
    }
}

async fn weather(args: &serde_json::Value, shared: &SharedTools) -> anyhow::Result<ToolOutput> {
    let tools = shared.read().expect("tools config lock poisoned").clone();
    let city = args
        .get("city")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| tools.weather_city.clone());
    if city.is_empty() {
        return Err(anyhow::anyhow!("未配置天气城市（tools.weather_city 为空）"));
    }
    let report = streaming::tools::fetch_weather(&city, tools.timeout_secs).await?;
    Ok(ToolOutput {
        text: report,
        media_url: None,
    })
}

async fn snapshot(
    args: &serde_json::Value,
    streams: &Arc<StreamManager>,
) -> anyhow::Result<ToolOutput> {
    let camera_id = match args
        .get("camera_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        // No explicit id: the registry is UUID-keyed and `"0"` never
        // matched — default to whichever stream actually holds a frame.
        None => streams
            .first_stream_with_frame()
            .await
            .unwrap_or_else(|| "0".into()),
    };
    let jpeg = streams
        .latest_jpeg(&camera_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("相机 {camera_id} 当前无可用画面（流未启动或无帧）"))?;
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    // Same-origin relative endpoint — the frontend renders it as a
    // thumbnail on the tool card; the model gets the same path as text.
    let endpoint = format!("/api/cameras/{camera_id}/snapshot");
    Ok(ToolOutput {
        text: format!(
            "已抓取相机 {camera_id} 当前画面（{ts}，{} 字节 JPEG）。用户可在 Web 界面查看：{endpoint}",
            jpeg.len()
        ),
        media_url: Some(endpoint),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(city: &str) -> SharedTools {
        let cfg = streaming::tools::ToolsConfig {
            weather_enabled: true,
            weather_city: city.into(),
            timeout_secs: 2,
        };
        Arc::new(std::sync::RwLock::new(cfg))
    }

    #[test]
    fn specs_hide_weather_without_city() {
        let no_city = shared("");
        let names: Vec<String> = specs(&no_city).into_iter().map(|t| t.name).collect();
        assert!(names.contains(&TIME_TOOL.into()));
        assert!(names.contains(&SNAPSHOT_TOOL.into()));
        assert!(!names.contains(&WEATHER_TOOL.into()));

        let with_city = specs(&shared("Guangzhou"));
        assert!(with_city.iter().any(|t| t.name == WEATHER_TOOL));
    }

    #[tokio::test]
    async fn time_tool_renders_local_datetime() {
        let out = time_now();
        assert!(out.text.contains("UTC"));
        // A formatted local timestamp: 4-digit year then time.
        assert!(out.text.contains(':'), "{}", out.text);
    }

    #[tokio::test]
    async fn weather_without_city_is_honest_error() {
        let err = weather(&json!({}), &shared("")).await.unwrap_err();
        assert!(err.to_string().contains("weather_city"));
    }

    #[tokio::test]
    async fn dispatch_unknown_name_returns_none() {
        let streams = crate::stream_manager::StreamManager::new();
        let out = dispatch("home.light", &json!({}), &shared(""), &Arc::new(streams))
            .await
            .unwrap();
        assert!(out.is_none());
    }
}
