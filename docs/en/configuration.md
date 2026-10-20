# Configuration Reference

This document provides a complete reference for the mibee-eye configuration system.

## Overview

Configuration files control all aspects of mibee-eye behavior. The configuration system supports hierarchical precedence, allowing different settings for development, testing, and production environments.

### Configuration File Locations

1. **Default configuration**: `config.toml` (project root)
2. **Local override**: `config.local.toml` (project root)
3. **CLI override**: `--config <path>` command-line argument

### Precedence Rules

Configuration values are loaded in order of precedence (highest wins):

1. Command-line `--config <path>` (absolute highest priority)
2. `config.local.toml` (gitignored, for local development)
3. `config.toml` (default configuration)

When a file doesn't exist, the system falls back to compiled-in defaults. Individual sections and fields not specified in a configuration file will use their default values.

## Configuration Reference

### [web] - Web UI Server

Configure the HTTPS web server hosting the user interface and REST API.

```toml
[web]
port = 8443
host = "0.0.0.0"
advertised_host = "192.168.1.100"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `port` | u16 | `8443` | HTTPS port for web UI and REST API (must be > 1024) |
| `host` | String | `"0.0.0.0"` | Bind address: `"0.0.0.0"` (all interfaces) or `"127.0.0.1"` (localhost only) |
| `advertised_host` | Option<String> | `None` | Advertised hostname/IP for URLs returned to clients. If None, auto-detected at startup via UDP probe. Eliminates hardcoded localhost in external URLs. |
| `http_port` | u16 | `0` | Optional additional plain-HTTP listener for LAN access without TLS ceremony (SPEC appendix A). 0 = disabled. Session cookies issued over this listener omit the `Secure` flag; TLS-listener cookies keep it. |

**Notes:**

- All web traffic uses TLS (HTTPS) via rustls
- Self-signed TLS certificates are auto-generated on first run at `tls/cert.pem` + `tls/key.pem`
- Certificates support hot-reload on file mtime change
- Production environments should supply CA-signed certificates
- Default ports avoid privileged range (< 1024)

### [rtsp] - RTSP Server

Configure the RTSP streaming server for camera feeds and client connections.

```toml
[rtsp]
server_port = 8554
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server_port` | u16 | `8554` | RTSP server listening port |

**Notes:**

- RTSP server operates in SERVER mode (clients pull streams from this machine)
- RTSP protocol supports RFC 2326 with Digest authentication and RTP interleaved
- Default port avoids privileged range; no privilege binding required
- Supports H.264 streaming with proper SPS/PPS headers
- External clients (NVRs, VLC) connect to `rtsp://this-host:8554/stream` to pull streams

### [rtmp_push] - RTMP Push Client

Configure the RTMP push client for outbound streaming to external ingest servers (NVRs, live platforms).

**All RTMP operations are OUTBOUND PUSH — this machine pushes to external endpoints. There is no RTMP ingest server.**

```toml
[rtmp_push]
enabled = false
push_url = "rtmp://192.168.1.100:1935/live"
app_name = "live"
stream_name = "stream1"
reconnect_interval_secs = 5
max_reconnect_attempts = 10
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master enable toggle (default OFF for all outbound protocols) |
| `push_url` | String | `"rtmp://192.168.1.100:1935/live"` | External RTMP ingest endpoint URL |
| `app_name` | String | `"live"` | RTMP application name |
| `stream_name` | String | `"stream1"` | RTMP stream key/name |
| `reconnect_interval_secs` | u64 | `5` | Reconnection attempt interval in seconds (must be > 0 if enabled) |
| `max_reconnect_attempts` | u32 | `10` | Maximum reconnection attempts (must be > 0 if enabled) |

**Notes:**

- This is OUTBOUND push only — this machine pushes to external RTMP ingest servers
- No RTMP ingest server exists; external clients cannot push to this machine via RTMP
- Hand-written RTMP implementation with enhanced timestamp support
- Automatic reconnection with exponential backoff on connection loss
- Protocol hot-toggle available via Web UI without server restart

### [capture] - Local Capture Devices

Configure local webcam and microphone capture.

```toml
[capture]
video_device = "/dev/video0"
audio_device = "default"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `video_device` | String | `"/dev/video0"` | Video capture device path |
| `audio_device` | String | `"default"` | Audio capture device identifier |

**Notes:**

- **Linux**: Video device typically `/dev/video0`, `/dev/video1`, etc.
- **Linux**: Audio device `"default"` uses ALSA default device
- **Windows**: Video devices use MSMF (Media Foundation) device names
- **Windows**: Audio devices use WASAPI device names
- Video capture requires `libv4l-dev` and user in `video` group
- Audio capture runs at high priority; never block in callbacks
- This product is LOCAL-ONLY capture — it does NOT discover or pull from remote network cameras

### [security] - Authentication and Rate Limiting

Configure security policies for authentication and API protection.

```toml
[security]
rate_limit_max = 20
rate_limit_window_secs = 60
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `rate_limit_max` | usize | `20` | Maximum requests per rate limit window (must be > 0) |
| `rate_limit_window_secs` | u64 | `60` | Rate limit time window in seconds |

**Notes:**

- Implements sliding window rate limiting for authentication endpoints using `parking_lot::Mutex`
- Prevents brute force attacks on login system
- Rate limiting is per-IP address basis
- Login failure lockout with exponential backoff after 5 failures
- Mutex is non-poisoning (safe for request handlers)

### [resources] - Boot-time feature gating

Adapts which AI features boot on this host (SPEC appendix A #40). `auto`
(default) admits enabled features greedily against the boot memory budget —
each feature's cost is estimated from its model files on disk — so a
small-memory machine sheds the heavy tail (VLM/LLM) instead of thrashing.
`all` boots every enabled feature regardless of memory. `auto_tier`
additionally swaps the LLM model file by available memory (full/mid/lite).

```toml
[resources]
feature_gate = "auto"   # auto | all
reserve_mib = 512       # headroom kept out of the budget
auto_tier = false       # pick the LLM tier model by available memory
```

The admission table surfaces as `capabilities.resource` (status page
"Resource Profile" card) and as `mibee_eye_feature_admitted` /
`mibee_eye_resource_budget_mib` Prometheus gauges. Re-evaluated on restart.

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `tier` | String | `"auto"` | Named hardware tier (`full` / `balanced` / `lite` / `auto` = derive from detected hardware) |
| `auto_tier` | bool | `false` | Single-key shorthand: pick the LLM tier model by available memory (full/mid/lite per `[llm]` model paths) |
| `feature_gate` | String | `"auto"` | `"auto"` admits enabled features against the boot memory budget; `"all"` boots everything regardless of memory |
| `reserve_mib` | u64 | `512` | MiB of headroom kept out of the admission budget (MemAvailable already includes reclaimable cache) |

### [conversations] - Dialogue turn records (SPEC §3.4)

Master switch for the human-readable dialogue log: every turn (voice or
web chat) is persisted in SQLite — heard/input text, internal "thinking"
entries (decision / cloud / VLM / LLM / TTS, failed fallback legs
included) and the AI reply with its engine — served at
`GET /api/conversations` and pushed live over SSE `conversation` events
(assistant page "Conversation Log" card). FIFO-capped at 1000 turns;
write failures never touch the conversation pipeline. `enabled = false`
records nothing and hides the card (privacy switch).

```toml
[conversations]
enabled = true
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Master switch for dialogue-turn records; `false` records nothing and hides the conversation-log card (privacy switch) |

### [agent] - Agent tools & skills (SPEC §3.5 / appendix A #43)

The dialogue assistant's tool-calling loop. Built-in tools are always
registered: `time.now` (local clock), `weather.current` (reuses the
`[tools]` weather config — requires `weather_city` to be set), and
`camera.snapshot` (fresh frame + viewing endpoint). **Custom plugins**
plug in as MCP (Model Context Protocol, spec 2025-06-18) stdio
subprocess servers: any MCP server executable works — the device spawns
it, handshakes over newline-delimited JSON-RPC, lists its tools, and
calls them during dialogue (ask about weather → the model calls
`weather.current` → the observation is fed back → the reply cites real
data). Tool results are truncated to 8 KiB before entering the model
context; tool failures are reported to the model honestly (it says so
instead of inventing data). An empty registry (no tools at all)
degrades dialogue to the plain non-agent path with zero behavior
change. Cloud providers use their native `tools`/`tool_calls` API; the
local Qwen3 engine uses its native `<tool_call>` prompt format.
`GET /api/tools` lists everything exposed to the model (transparency
surface). Trust boundary: MCP servers are deployer-configured local
subprocesses — nothing is auto-installed or discovered remotely; later
releases can add per-tool confirmation gates for sensitive actions.

```toml
[agent]
enabled = true
max_steps = 3             # tool-call iterations before forcing a text answer
step_timeout_ms = 15000   # per tool execution timeout

# Custom tool plugin example (any MCP stdio server):
[[agent.mcp_servers]]
name = "home"
command = "python3"
args = ["/home/you/.config/mibee-eye/mcp/home_tools.py"]
# Voice turns carry the tool table only on tool-shaped utterances
# (weather/time/snapshot/device-control intent) — tool prompts are
# prefill-heavy on CPU hosts and chit-chat would pay ~20 s for it.
voice_tool_gate = true
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Master gate. `true` with an empty registry is a no-op — the loop needs at least one tool before it engages |
| `max_steps` | u32 | `3` | Tool-calling iteration bound; the final iteration drops the tool table so the model must produce a text answer |
| `step_timeout_ms` | u64 | `15_000` | Per tool-execution timeout in milliseconds |
| `voice_tool_gate` | bool | `true` | Voice turns carry the tool table only on tool-shaped utterances (weather/time/snapshot/device control); HTTP chat always carries the full table |
| `mcp_servers` | array of tables | `[]` | MCP stdio servers (`[[agent.mcp_servers]]`): `name`, `command`, `args`, `env` per entry |

### [away] - Away mode watch (SPEC §3.6 / appendix A #44)

The armed watch for when nobody is home. It rides the existing AI
detection stream (no second sampling pipeline): person arrivals (gap +
cooldown gated) are recorded with a snapshot, face-matched against the
enrolled gallery (known faces are greeted by name and not interrogated;
strangers get the greeting + identity question and a no-wake-word
listening window whose transcript lands in the record), described by
the VLM asynchronously, and pushed to the desktop. Configured activity
labels (default: pets) are recorded with a cooldown while no person is
present. Armed state survives service restarts (`away.active`
setting). Voice legs need `[tts]` and `[voice]` active — without them
person events are still recorded (`state:"no_voice"`).

```toml
[away]
interval_ms = 1000           # min spacing of away analyses per camera
                             # (stacks on [ai] interval_ms)
person_gap_secs = 10         # absence grace: re-detection inside = same visit
greeting_cooldown_secs = 120 # min spacing between visitor interactions
activity_cooldown_secs = 60  # min spacing between activity records
activity_labels = ["cat", "dog", "bird"]  # non-person anomaly labels
listen_secs = 10             # one-shot listening window after the question
snapshot_dir = "away-snapshots"           # evidence JPEGs (cwd-relative)
greeting_known = "欢迎回家，{name}。"     # {name} = enrolled face name
greeting_unknown = "你好，这里是主人的智能看家助手。主人现在不在家，请问你是谁？"
```

**Field Reference** (every key is **web-configurable live** via
`PUT /api/config` `scene.away.*`, no restart; the `[away]` TOML block above
only seeds boot defaults):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `interval_ms` | u64 | `1000` | Event-throttle floor stacked on the `[ai]` sampling interval (no separate pipeline) |
| `person_gap_secs` | u64 | `10` | Person-absence grace before the visitor stream considers the person gone |
| `greeting_cooldown_secs` | u64 | `120` | Minimum spacing between two greeting interactions |
| `activity_cooldown_secs` | u64 | `60` | Minimum spacing between two activity records of the same class |
| `activity_labels` | Vec<String> | `["cat","dog","bird"]` | Detection classes that count as "activity" records while armed |
| `listen_secs` | u64 | `10` | One-shot wake-free listening window after a greeting question |
| `snapshot_dir` | String | `"away-snapshots"` | Evidence JPEG directory (cwd-relative); pruning/clearing deletes the files |
| `greeting_known` | String | `"欢迎回家，{name}。"` | Greeting for enrolled faces (`{name}` = the person name) |
| `greeting_unknown` | String | `"你好，这里是主人的智能看家助手。主人现在不在家，请问你是谁？"` | Greeting + identity question for unrecognized visitors |
| `voice_control` | bool | `true` | "我要出门了/我回来了" utterances arm/disarm the watch ahead of the LLM |
| `summary_on_disarm` | bool | `true` | Disarm produces a summary (local LLM one-liner, plain-count fallback); returned by the API and spoken on voice disarm |
| `deterrence` | bool | `false` | Unanswered strangers are re-asked once (`reask_text`, half window), then played `deterrence_text` |
| `deterrence_text` | String | `"请注意：您的影像和声音已被记录，主人将收到通知。"` | The deterrence message played to a silent stranger |
| `reask_text` | String | `"请问你是谁？请说明来意。"` | The second-chance question for an unanswered greeting |
| `schedule_arm` | String | `""` | `HH:MM` local-time daily arm edge; empty disables that edge. Manual state inside the window is never fought |
| `schedule_disarm` | String | `""` | `HH:MM` local-time daily disarm edge; empty disables that edge |
| `schedule_weekdays_only` | bool | `false` | Scheduled edges fire Mon–Fri only |
| `push_url` | String | `""` | Visitor push webhook: one JSON POST per visitor event (text only, never images), 5 s timeout, fail-open. **Empty keeps the device fully air-gapped** — configuring a URL is the informed opt-in to outbound text |
| `audio_link` | bool | `true` | While armed, voted sound-alarm rising edges also land as away activity records (labels `🔊 class score`, one per activity cooldown) |

### [desktop] - Tray icon & desktop notifications (SPEC appendix A #42)

Desktop-session presence for hosts with a GUI. All fail-open: without a
session bus (`DBUS_SESSION_BUS_ADDRESS` or `$XDG_RUNTIME_DIR/bus`) the
whole feature is skipped with one log line — headless servers are
unaffected. The tray shows a camera icon; left-click (or the menu entry)
opens the web UI via `xdg-open` (prefers `web.http_port` when set, else
the TLS port). Notifications fire on every accepted alarm rising edge
(visual / sound / zone); the first failed send (no notification daemon)
latches them off for the run.

```toml
[desktop]
tray = true                # StatusNotifierItem tray icon
notifications = true       # desktop notifications on alarm edges
notify_conversations = false  # also notify on completed voice replies
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `tray` | bool | `true` | StatusNotifierItem tray icon (opens the web UI on click) |
| `notifications` | bool | `true` | Desktop notifications on accepted alarm rising edges |
| `notify_conversations` | bool | `false` | Also notify when a voice reply finishes playing |


### [observability] - Monitoring and Logging

Configure OpenTelemetry tracing, application logging, and optional remote log shipping.

```toml
[observability]
otel_endpoint = "http://localhost:4317"
log_level = "info"

[observability.logs]
endpoint = "http://loki:3100/loki/api/v1/push"
batch_size = 100
flush_interval_secs = 5

[observability.logs.labels]
environment = "production"
service = "mibee-eye"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `otel_endpoint` | String | `"http://localhost:4317"` | OpenTelemetry collector endpoint (OTLP gRPC). When set, spans flow out for every request, protocol session and **model invocation** (`model_call/<id>` spans carrying `model`/`variant`, plus one `conversation/<origin>` root span per dialogue with its model chain) |
| `log_level` | String | `"info"` | Log level filter |
| `logs` | Option<RemoteLogConfig> | `None` | Optional remote log shipping configuration |

**Observability surface** (2026-10-04): besides the OTLP trace export, `/metrics` (public, Prometheus text) carries per-model resource families — `mibee_model_inferences_total{model,variant}`, `mibee_model_inference_seconds` / `mibee_model_cpu_seconds` histograms, `mibee_model_inflight{model}`, `mibee_model_errors_total`, `mibee_model_tokens_total{...,kind=prompt|completion}` — plus the system/process resource gauges (`mibee_eye_system_*` / `mibee_eye_process_*`). Conversation model-call chains are queryable via `GET /api/traces/conversations` (SPEC v1 §3.3; bounded in-memory ring of 200 conversations × 64 spans each).

**[observability.logs] RemoteLogConfig Fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoint` | String | `""` | Loki-compatible HTTP endpoint URL for remote log shipping |
| `batch_size` | usize | `100` | Number of log entries to batch per flush |
| `flush_interval_secs` | u64 | `5` | Flush interval in seconds |
| `labels` | HashMap<String, String> | `{}` | Additional labels attached to every log stream |

**Log Level Options:**

- `"trace"` - Most detailed logging, debugging information
- `"debug"` - Debug information, function calls
- `"info"` - General operational information (default)
- `"warn"` - Warning conditions that don't stop operation
- `"error"` - Error conditions that may impact operation

**Notes:**

- OpenTelemetry integration is optional — system works without collector
- OTLP (OpenTelemetry Protocol) over gRPC on port 4317
- Structured JSON logging when OpenTelemetry is unavailable
- Remote log shipping to Loki is optional and fail-open: unreachable endpoint logs a warning, application continues
- W3C TraceContext `traceparent` header extracted on incoming requests, injected on outbound
- Prometheus `/metrics` endpoint available for scraping (no auth, firewall in production)

### [onvif] - ONVIF Device Endpoint

Configure the ONVIF device endpoint for external NVR discovery via WS-Discovery.

**This machine acts as the ONVIF camera — external NVRs discover IT. This is NOT an ONVIF client.**

```toml
[onvif]
enabled = false
device_name = "mibee-eye"
manufacturer = "MiBee"
model = "Rec-01"
serial = "NC00000001"
firmware_version = "1.0.0"
port = 3702
events_enabled = true
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master enable toggle (default OFF for all outbound protocols) |
| `device_name` | String | `"mibee-eye"` | ONVIF device name |
| `manufacturer` | String | `"MiBee"` | Manufacturer name |
| `model` | String | `"Rec-01"` | Device model |
| `serial` | String | `"NC00000001"` | Serial number |
| `firmware_version` | String | `"1.0.0"` | Firmware version |
| `port` | u16 | `3702` | WS-Discovery UDP port (hardcoded) |
| `events_enabled` | bool | `true` | Pull-Point events service: AI motion alarms publish as `tns1:VideoSource/MotionAlarm` while an NVR holds a subscription |
| `media2_enabled` | bool | `true` | ONVIF Media2 (Profile T) endpoint alongside classic Media |
| `http_digest` | bool | `false` | HTTP Digest auth for the SOAP listener (default WS-UsernameToken) |
| `ip_filter` | Vec<String> | `[]` | IPv4 allow-list (`a.b.c.d[/prefix]`) for the SOAP listener; empty = no filtering, otherwise peers outside the list get 403 before any SOAP processing. Restart-to-apply |

**Notes:**

- OUTBOUND protocol — this device broadcasts WS-Discovery messages, external NVRs discover it
- Hand-written WS-Discovery server (701 LOC)
- Protocol hot-toggle available via Web UI without server restart

### [gb28181] - GB/T 28181 Device Registration

Configure GB/T 28181 device registration with a Chinese surveillance platform.

**This device registers WITH the platform via SIP REGISTER; the platform sends INVITE, this device pushes RTP.**

```toml
[gb28181]
enabled = false
platform_sip_address = "192.168.1.100"
platform_sip_port = 5060
device_id = "34020000002000000001"
username = ""
password = ""
sip_domain = "3402000000"
alarm_notify_enabled = true
alarm_cooldown_secs = 30
position_longitude = ""
position_latitude = ""
register_interval_secs = 60
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master enable toggle (default OFF for all outbound protocols) |
| `platform_sip_address` | String | `"192.168.1.100"` | SIP platform address |
| `platform_sip_port` | u16 | `5060` | SIP platform port (must be > 1024 if enabled) |
| `device_id` | String | `"34020000002000000001"` | 20-character GB28181 device ID |
| `username` | String | `""` | SIP authentication username |
| `password` | String | `""` | SIP authentication password |
| `sip_domain` | String | `"3402000000"` | SIP domain |
| `register_interval_secs` | u64 | `60` | SIP REGISTER interval in seconds (must be > 0 if enabled) |
| `channel_id` | String | `"34020000001320000001"` | 20-character GB28181 channel ID |
| `local_sip_port` | u16 | `5060` | Local SIP listen port |
| `heartbeat_interval_secs` | u64 | `60` | Keepalive interval in seconds |
| `heartbeat_timeout_count` | u32 | `3` | Missed keepalives before reconnect |
| `talkback_playback` | bool | `true` | Play platform voice talkback on the local output device (fail-open 488 when unavailable) |
| `alarm_notify_enabled` | bool | `true` | Initial AI alarm NOTIFY gate (platform DeviceConfig AlarmReport overrides at runtime) |
| `alarm_cooldown_secs` | u64 | `30` | Rising-edge alarm cooldown in seconds (SPEC §6 `alarm` + NOTIFY) |
| `position_longitude` | String | `""` | Static MobilePosition longitude (empty = no position reporting) |
| `position_latitude` | String | `""` | Static MobilePosition latitude (empty = no position reporting) |

**Notes:**

- OUTBOUND protocol — this device registers WITH the platform, not the platform role
- GB28181 signaling via the `gb28181-rs` library, RTP/PS push over UDP
- AI detection rising edges fire the SPEC §6 `alarm` SSE event and (when the platform subscribed and the gate allows) an Alarm NOTIFY — priority 4, method 5, type 2 per the 2022 standard table
- SIGTERM / protocol stop de-registers with REGISTER `Expires: 0` (failures logged and ignored)
- Protocol hot-toggle available via Web UI without server restart

### [recording] - Local Recording

Configure local MP4 segment recording to disk.

```toml
[recording]
enabled = false
path = "./recordings"
segment_duration_secs = 900
max_capacity_mb = 10240
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master enable toggle. Individual streams can opt out via Web UI. |
| `path` | String | `"./recordings"` | Directory where MP4 segment files are written (must be writable) |
| `segment_duration_secs` | u64 | `900` | MP4 segment duration in seconds (must be > 0). Default: 15 minutes. |
| `max_capacity_mb` | u64 | `10240` | Max total capacity in megabytes. 0 = unlimited (no pruning). Default: 10 GB. |

**Notes:**

- Captured H.264 frames are muxed into rolling MP4 segments
- Oldest segments are auto-pruned when total size exceeds `max_capacity_mb`
- Individual streams can enable/disable recording via Web UI

### On-device intelligence sections

The sections below configure the optional AI engines. They share a common
contract:

- **All default to `enabled = false`** — microphones and always-on analysis
  are privacy-sensitive, so every engine is opt-in.
- **Fail-open**: a missing model file, a build without the required cargo
  feature, or an under-powered host simply leaves the engine off — the
  capability disappears from `/api/capabilities` and the UI, never breaking
  startup or the rest of the product.
- **Startup-state**: these sections are read once at startup; changes require
  a service restart (unlike protocol sections, which hot-toggle).
- Model files are untracked deploy-time downloads — see
  [`models/README.md`](../../models/README.md) for sources, sizes and
  licenses.

### [watermark] - Video Watermark Overlay

Burns a text/timestamp overlay into the encoded stream (SPEC v1 §5.2).
Bootstrap default only — the runtime settings live in the DB
(`protocols.watermark`) and are edited in the Web UI.

```toml
[watermark]
enabled = false
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch |
| `text` | String | `""` | Custom text (≤128 chars); CJK requires a `font_path` with CJK glyphs |
| `show_timestamp` | bool | `true` | Burn the timestamp line |
| `timestamp_format` | String | `"%Y-%m-%d %H:%M:%S"` | strftime subset: `%Y %m %d %H %M %S %F %T %%` + literals |
| `position` | String | `top_left` | One of `top_left` / `top_right` / `bottom_left` / `bottom_right` |
| `font_size` | u32 | `24` | Pixel height, 12..96 |
| `font_path` | String | `""` | Optional TTF/OTF (needed for CJK); empty = embedded ASCII subset font |

### [webrtc] - WebRTC Playback (Experimental)

Whep-style WebRTC playback endpoint. Ice candidates gather from a small
UDP range; STUN servers enable non-LAN topologies (empty = host
candidates only).

```toml
[webrtc]
enabled = false
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch |
| `udp_port_min` | u16 | `0` | UDP port range start for ICE host candidates (0 = OS chooses) |
| `udp_port_max` | u16 | `0` | UDP port range end (inclusive; 0 = OS chooses) |
| `stun_servers` | String | `""` | Comma-separated STUN URLs (e.g. `stun:stun.l.google.com:19302`); empty = LAN-only host candidates |

### [ai] - Object Detection (NanoDet-Plus)

Visual object detection feeding alarms, zones and the away watch. Off by
default — inference is opt-in like the outbound protocols.

```toml
[ai]
enabled = false
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch |
| `model` | String | `"nanodet-plus-m-320"` | Registry id of the startup model (SPEC §4.6); a non-default `model_path` overrides it |
| `allow_upload` | bool | `false` | Allow runtime model uploads (SPEC §4.6 capability `ai_upload`; model files are untrusted input) |
| `model_path` | String | `"models/nanodet-m.onnx"` | NanoDet-Plus ONNX path (cwd-relative or absolute) |
| `confidence_threshold` | f32 | `0.5` | Minimum confidence for a detection to be reported (0..=1) |
| `interval_ms` | u64 | `1000` | Minimum spacing between inferences per camera |

### [audio_ai] - Sound-Event Detection

Constant microphone listening with YAMNet classification. When a watched
sound class fires (vote-smoothed, per-class cooldown), the SPEC §6 `alarm`
event is emitted with `source: "audio"`, the class display name in `class`
and the voted score in `score`.

```toml
[audio_ai]
enabled = false
device = "default"
classes = ["Dog", "Bark", "Baby cry, infant cry", "Glass", "Siren"]
threshold = 0.3
cooldown_secs = 30
model_path = "models/audio/yamnet.onnx"
vad_model_path = "models/audio/silero_vad.onnx"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch. Off by default — continuous microphone listening is opt-in. |
| `device` | String | `"default"` | Input device selector: `"default"` or a substring of the ALSA device description. |
| `classes` | Vec<String> | 15 default classes (Dog/Bark/Yip/Howl/Bow-wow, Baby cry, Screaming, Shout, Glass/Shatter/Breaking, Smoke detector/Fire alarm, Siren, Knock) | Watched YAMNet class display names (exact match). Unknown names are logged and ignored at startup. |
| `threshold` | f32 | `0.3` | Voted score a class must reach to fire (0 < threshold ≤ 1). |
| `cooldown_secs` | u64 | `30` | Per-class cooldown between consecutive alarms (must be > 0). |
| `model_path` | String | `"models/audio/yamnet.onnx"` | YAMNet ONNX model path. |
| `vad_model_path` | String | `"models/audio/silero_vad.onnx"` | Silero VAD ONNX path (voice-presence signal). |

**Notes:**

- A rolling 0.96 s window with 50% overlap is classified; three consecutive
  window scores are averaged before a class may fire, so single-patch blips
  never alarm.
- Windows quieter than the RMS floor skip classification entirely.
- Also publishes a live voice-presence flag consumed by the voice
  interaction loop.

### [voice] - Voice Interaction (Wake Word + ASR)

Wake-word detection with offline speech-to-text. Requires the `voice` cargo
feature (sherpa-onnx linked statically at build time). After a wake word is
recognized, `capture_secs` of audio are transcribed offline and emitted as a
`voice_transcript` SSE event; with `[llm]` enabled the transcript is answered
by the local LLM, and with `[tts]` enabled the reply is spoken. With
`follow_up_window_secs > 0` the reply opens a follow-up window in which the
VAD endpoint detector transcribes whole utterances without a new wake word
(hot-adjustable via `PUT /api/config` `scene.voice.follow_up_window_secs`;
`scene.voice.wake_word` is restart-class).

```toml
[voice]
enabled = false
kws_encoder = "models/voice/kws/encoder.int8.onnx"
kws_decoder = "models/voice/kws/decoder.int8.onnx"
kws_joiner = "models/voice/kws/joiner.int8.onnx"
kws_tokens = "models/voice/kws/tokens.txt"
keywords_file = "models/voice/kws/keywords.txt"
keywords_threshold = 0.25
keywords_score = 1.0
paraformer_model = "models/voice/paraformer/model.int8.onnx"
paraformer_tokens = "models/voice/paraformer/tokens.txt"
capture_secs = 4
num_threads = 1
# Continuous dialogue (#30-B): after each spoken reply, a VAD window of
# this length accepts follow-ups WITHOUT the wake word (0 = off).
follow_up_window_secs = 0.0
vad_model = "models/voice/vad/silero_vad.onnx"
# VAD-endpointed capture after a wake word: speech keeps the window
# open (up to capture_secs + 6 s) and ~0.6 s of trailing silence ends
# it — questions spoken after a pause are no longer clipped by the
# fixed window. Needs vad_model; missing → fixed window (fail-open).
capture_endpointing = true
# Interruption (#45)
interrupt_enable = true
interrupt_confidence = 0.6
# 播报期打断（需主机 AEC 采集源） / playback-phase (host AEC source required)
barge_in = "off"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch (requires the `voice` build feature). |
| `kws_encoder` | String | `"models/voice/kws/encoder.int8.onnx"` | Zipformer transducer KWS encoder |
| `kws_decoder` | String | `"models/voice/kws/decoder.int8.onnx"` | Zipformer transducer KWS decoder |
| `kws_joiner` | String | `"models/voice/kws/joiner.int8.onnx"` | Zipformer transducer KWS joiner |
| `kws_tokens` | String | `"models/voice/kws/tokens.txt"` | KWS tokens file |
| `wake_word` | String | `"小蜜蜂"` | The wake phrase the KWS listens for |
| `keywords_file` | String | `"models/voice/kws/keywords.txt"` | Keywords file — one keyword per line, `tokens… @display-name` (zh-en phoneme model, e.g. `x iǎo m ì f ēng @小蜜蜂`). |
| `keywords_threshold` | f32 | `0.25` | Wake sensitivity — lower fires more easily. |
| `keywords_score` | f32 | `1.0` | Minimum score for an accepted wake. |
| `paraformer_model` | String | `"models/voice/paraformer/model.int8.onnx"` | Offline paraformer checkpoint. Default is the zh build (Mandarin + embedded English); **for Cantonese/Mandarin/English use the trilingual build** (`models/voice/paraformer-trilingual/…`, 234MB, Apache-2.0, see `models/README.md`). |
| `paraformer_tokens` | String | `"models/voice/paraformer/tokens.txt"` | Paraformer tokens file |
| `follow_up_window_secs` | f32 | `0.0` | Wake-free follow-up window after each reply (0 = off; armed on demand by the away monitor and interrupt rounds) |
| `vad_model` | String | `"models/voice/vad/silero_vad.onnx"` | Silero VAD for endpointing / listen windows |
| `capture_endpointing` | bool | `true` | VAD-based endpointing while capturing a question: soft deadline extends while speaking, ~0.6 s trailing silence ends the segment |
| `capture_secs` | u32 | `4` | Seconds of audio captured after a wake word. |
| `num_threads` | i32 | `1` | Inference threads (target hosts are small). |
| `speaker_embedding_model` | String | `models/voice/speaker/campplus.onnx` | Speaker-embedding model (3D-Speaker CAM++ zh_en). **A missing file only disables voiceprint features** — wake + ASR keep working. |
| `speaker_verify` | bool | `false` | Voiceprint gate: wake words must match an enrolled speaker before the capture window opens (fail-open with a one-time WARN when no profiles exist). |
| `speaker_threshold` | f32 | `0.55` | Cosine-similarity threshold (CAM++ typical 0.5–0.6; calibrate on the target mic). |
| `verify_window_secs` | f32 | `2.0` | Ring-buffer seconds of pre-wake audio the gate embeds (must cover the wake-word utterance). |
| `interrupt_enable` | bool | `true` | interruption (#45): wake-free listening while a round thinks + stop-word/Laya policy. |
| `interrupt_confidence` | f32 | `0.6` | Minimum Laya confidence to act on an interrupt verdict; below it the utterance is treated as backchannel (conservative fail-open). |
| `barge_in` | String | `"off"` | Playback-phase barge-in: `"off"` fully deaf while replying (default); `"aec"` deafens only the wake word and keeps VAD live (say "stop" or a new question during playback to interrupt). **Requires a host echo-cancelled capture source** (pipewire `module-echo-cancel`, webrtc, source gain ≤100%) or our own TTS feeds straight into ASR. |

**Notes:**

- While waiting for a wake word nothing is recorded or transmitted — the
  keyword model matches locally against a small audio fingerprint.
- `capture_secs` bounds the utterance length; speak after the wake word.
- Best results with an external USB microphone; laptop built-ins often lack
  the sensitivity (see the [user guide](user-guide.md#troubleshooting)).

### [llm] - Local LLM Dialogue

Local chat completions via llama.cpp (GGUF). Powers `POST /api/chat`, the
web chat panel and the voice-loop replies (`chat_reply` SSE). Requires the
`llm` cargo feature and an AVX2-class CPU.

```toml
[llm]
enabled = false
model_path = "models/llm/qwen3-0.6b-q8_0.gguf"
# Resource tiers (#30-E): with [resources] auto_tier = true the boot-time
# available memory picks the tier (>=8 GiB full / >=4 GiB mid / else lite);
# empty values fall back to model_path.
model_path_mid = ""
model_path_lite = ""
n_ctx = 2048
# Default is half the logical cores, clamped to 2..4 — measured on a 4C/8T
# host, more threads than physical cores regressed generation. Shown here as
# the value a 4-thread host resolves to.
n_threads = 2
max_tokens = 200
no_think = true
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch (requires the `llm` build feature + AVX2). |
| `model_path` | String | `"models/llm/qwen3-0.6b-q8_0.gguf"` | GGUF model path (Qwen3-0.6B Q8_0 by default). |
| `n_ctx` | u32 | `2048` | Dialogue context window. Must exceed the tool prompt (SPEC §3.5 runs ~500-1700 tokens) plus the conversation; the previous default of 1024 overflowed and silently degraded the first agent turns. |
| `n_threads` | u32 | logical cores / 2, clamped to `2..=4` | CPU threads. Leave unset to let the device size this from the host. |
| `max_tokens` | u32 | `200` | Generation cap per reply. |
| `no_think` | bool | `true` | Append `/no_think` to user turns (Qwen3 thinking mode off — faster, conversational). |
| `model_path_mid` | String | `""` | Mid-tier GGUF for `[resources] auto_tier` (empty = tier unavailable) |
| `model_path_lite` | String | `""` | Lite-tier GGUF for `[resources] auto_tier` (empty = tier unavailable) |

**Notes:**

- Greedy decoding (temperature 0): deterministic answers, no sampling drift.
- Thinking blocks are stripped from replies regardless of `no_think`.
- A memory guardrail refuses to load when the model exceeds 2/3 of available
  RAM (fail-open).
- Inference thread pools are capped automatically (`OMP_NUM_THREADS` =
  cores/2, max 4) unless the environment already sets it.

### [decision] - Voice decision assist (Laya typed decisions)

One local typed-intent decision (answer/device/ignore) over each voice
transcript before any local-LLM tokens are spent; `ignore` skips the
auto-reply. Needs the `[ai]` runtime (onnxruntime) and a laya
multilingual ONNX checkpoint (see `models/README.md`).

```toml
[decision]
enabled = false
model_path = "models/decision/laya_multilingual.int8.onnx"
tokenizer_path = "models/decision/tokenizer.json"
config_path = "models/decision/laya_config.json"
min_confidence = 0.35
num_threads = 1
```

| Key | Type | Default | Notes |
|------|------|---------|------|
| `enabled` | bool | `false` | Master switch. |
| `model_path` | String | `models/decision/laya_multilingual.int8.onnx` | laya ONNX checkpoint (int8 export recommended). |
| `tokenizer_path` | String | `models/decision/tokenizer.json` | The checkpoint's HuggingFace tokenizer. |
| `config_path` | String | `models/decision/laya_config.json` | json carrying `max_len`/`head_max_len`/calibration; safe defaults when absent. |
| `min_confidence` | f32 | `0.35` | Decisions below this are ignored (fail-open to the old behavior). |
| `num_threads` | u16 | `1` | Inference threads. |

### [meeting] - Meeting mode (on-demand recording + diarization + transcription)

Local meeting minutes: an explicit start → stop recording session, then
offline speaker diarization (pyannote segmentation + CAM++ embedding +
fast clustering) → per-segment trilingual ASR → (optional) punctuation
restoration → per-speaker voiceprint voting for names, persisted as a
text minute. **Privacy posture**: the no-audio-at-standby promise is
unchanged — samples only land on disk inside an explicit session;
`keep_audio=false` (default) deletes the WAV after processing (failures
too), keeping only the text; the session auto-stops at
`max_duration_secs` and feeds the same pipeline. Depends on the
`[voice]` ASR and speaker-embedding model files (meetings reuse them);
processing needs a `voice`-feature build. Model assets: see
`models/README.md`.

```toml
[meeting]
enabled = false
segmentation_model = "models/voice/diarization/pyannote.onnx"
punctuation_model = "models/voice/punct/model.onnx"
clustering_threshold = 0.5
min_duration_on = 0.3
min_duration_off = 0.5
keep_audio = false
max_duration_secs = 7200
audio_dir = "meetings"
num_threads = 1
```

| Key | Type | Default | Notes |
|------|------|---------|------|
| `enabled` | bool | `false` | Master switch (off by default — recording must be explicit). |
| `segmentation_model` | String | `models/voice/diarization/pyannote.onnx` | pyannote segmentation-3.0 (sherpa-onnx int8, ~1.5 MB, MIT). |
| `punctuation_model` | String | `models/voice/punct/model.onnx` | ct-transformer zh-en punctuation (int8 ~75 MB); empty string disables. |
| `clustering_threshold` | f32 | `0.5` | Fast-clustering distance threshold (higher = fewer speakers; a 4-speaker sample yields 5 at 0.5 — calibrate on site). |
| `min_duration_on` | f32 | `0.3` | Minimum voiced segment (s). |
| `min_duration_off` | f32 | `0.5` | Minimum silence (s); same-speaker gaps ≤ this merge into one ASR call. |
| `keep_audio` | bool | `false` | Keep the WAV after processing (deleted by default — privacy first). |
| `max_duration_secs` | u64 | `7200` | Safety cap: auto-stop and process (guards against forgotten recordings). |
| `audio_dir` | String | `meetings` | Session WAV directory (relative to the working directory). |
| `num_threads` | i32 | `1` | Inference threads. |

**Honest boundary**: acoustic clustering may merge same-voiced family
members and over-split distinctive voices (tune
`clustering_threshold`); transcription quality matches `[voice]` ASR;
speaker names depend on voiceprint matches (misses render as
"Speaker N") — convenience features, not precise annotation.

### [tts] - Text-to-Speech Playback

Spoken replies via the `sherpa-onnx-offline-tts` **CLI subprocess** (the GPL
espeak-ng dependency stays isolated in the subprocess, outside this binary)
playing through `aplay`. The LLM reply text is spoken when TTS is enabled.

```toml
[tts]
enabled = false
binary = "tmp/sherpa-libs/tools-bin/bin/sherpa-onnx-offline-tts"
model = "models/voice/melo/model.onnx"
lexicon = "models/voice/melo/lexicon.txt"
tokens = "models/voice/melo/tokens.txt"
dict_dir = "models/voice/melo/dict"
rule_fsts = "models/voice/melo/number.fst,models/voice/melo/date.fst"
player = "aplay -q"
# Per-language models (#30-D): the reply language picks the voice
# (Cantonese cue-chars -> yue, pure ASCII -> en, else the main model);
# unset keys fall back to the main model.
yue_model = ""
yue_lexicon = ""
en_model = ""
en_lexicon = ""
# sherpa-onnx-offline-tts synthesis threads (2 default; 4 halves
# the ~16 s synthesis of a 5 s utterance on 4-core+ hosts).
num_threads = 4
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch. No cargo feature needed. |
| `binary` | String | `"tmp/sherpa-libs/tools-bin/bin/sherpa-onnx-offline-tts"` | sherpa-onnx-offline-tts binary path (downloaded separately from the k2-fsa release). |
| `model` | String | `"models/voice/melo/model.onnx"` | vits-melo-tts-zh_en voice model (Mandarin primary) |
| `lexicon` | String | `"models/voice/melo/lexicon.txt"` | Pronunciation lexicon |
| `tokens` | String | `"models/voice/melo/tokens.txt"` | TTS tokens file |
| `dict_dir` | String | `"models/voice/melo/dict"` | Jieba dictionary directory |
| `num_threads` | u32 | `2` | TTS synthesis threads |
| `yue_model` | String | `""` | Cantonese voice model (empty = Cantonese unavailable) |
| `yue_lexicon` | String | `""` | Cantonese lexicon |
| `yue_dict_dir` | String | `""` | Cantonese dictionary directory |
| `en_model` | String | `""` | English voice model (empty = English unavailable) |
| `en_lexicon` | String | `""` | English lexicon |
| `rule_fsts` | String | `"…/number.fst,…/date.fst"` | Number/date normalization FSTs (comma-joined). |
| `player` | String | `"aplay -q"` | Playback command; empty = synthesize only (no speaker output). |

### [tools] - Dialogue Task Tools

Optional blocks the grounded system turn may inject (#30-A). Currently the
weather: when the user's utterance looks weather-related and
`weather_enabled` is on, current conditions for `weather_city` are fetched
(wttr.in, bounded by `timeout_secs`) into the 【联网】 block. These keys are
hot-adjustable via `PUT /api/config` `scene.tools.*` (#31).

```toml
[tools]
weather_enabled = false
weather_city = ""
timeout_secs = 5
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `weather_enabled` | bool | `false` | Master switch for the weather tool |
| `weather_city` | String | `""` | Query city (empty = tool returns no data) |
| `timeout_secs` | u64 | `5` | Fetch timeout in seconds |

### [vlm] - Alarm-Frame Image Descriptions

Event-triggered "what happened" descriptions of alarm frames via a
vision-language model (Qwen3-VL through llama.cpp mtmd). When a visual alarm
edge is accepted, the triggering JPEG is described asynchronously and the
result is emitted as the `alarm_description` SSE event — the alarm itself is
never delayed. Requires the `llm` build feature (AVX2-class CPU).

```toml
[vlm]
enabled = false
model_path = "models/vlm/qwen3-vl-2b-instruct-q4_k_m.gguf"
mmproj_path = "models/vlm/mmproj-qwen3-vl-2b-instruct-q8_0.gguf"
n_ctx = 2048
n_threads = 2
max_tokens = 100
repeat_penalty = 1.1
prompt = "这是安防摄像头的告警画面。请用一句中文描述画面里发生了什么。"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch (requires the `llm` build feature + AVX2). |
| `model_path` | String | `"models/vlm/qwen3-vl-2b-instruct-q4_k_m.gguf"` | Text-model GGUF. |
| `mmproj_path` | String | `"models/vlm/mmproj-qwen3-vl-2b-instruct-q8_0.gguf"` | Vision-projector (mmproj) GGUF. |
| `n_ctx` | u32 | `2048` | Context window (the image alone costs ~1k positions). |
| `n_threads` | u32 | `2` | CPU threads. |
| `max_tokens` | u32 | `100` | Generation cap per description. |
| `repeat_penalty` | f32 | `1.1` | Repetition penalty over the last 64 generated tokens (llama.cpp semantics). Greedy decoding loops on list items in hard frames without it; `1.0` disables. |
| `prompt` | String | Chinese security-phrasing instruction | Instruction shown to the model; the answer should be one sentence. |

**Notes:**

- **Single-flight scheduling**: at most one description runs at a time and a
  120 s floor separates consecutive starts — a flapping detector cannot
  stack VLM loads on a small host.
- Shares the same llama.cpp backend instance and memory guardrail as `[llm]`.

### [face] - Face Recognition

YuNet detection + SFace 128-d embeddings (enrollment, per-event name
attribution, known-visitor greetings in the away watch). Off by default —
CPU cost is real on old hosts. `detect_input` follows the model author's
reference implementation; the shipped zoo model accepts dynamic input.

```toml
[face]
enabled = false
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch |
| `detect_model` | String | `"models/face/face_detection_yunet_2023mar.onnx"` | YuNet detection model (OpenCV zoo) |
| `recog_model` | String | `"models/face/face_recognition_sface_2021dec.onnx"` | SFace recognition model (128-d embedding) |
| `detect_input` | u32 | `320` | Detection input edge; larger finds smaller faces at quadratic CPU cost |
| `match_threshold` | f32 | `0.363` | Cosine similarity threshold for a name match (OpenCV SFace reference value) |
| `enroll_frames` | usize | `8` | Frames accepted into one enrollment (more = stabler template) |
| `ttl_ms` | u64 | `10_000` | Skip detection when the frame is older than this |

### [models] - Model Asset Directory

Root directory for downloadable model assets (SPEC §4.6 model manager).

```toml
[models]
dir = "models"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `dir` | String | `"models"` | Root directory for model assets (uploads, registry downloads) |

### [ocr] - Text Recognition

On-device OCR (PP-OCR v4 detector + v5 recognizer, zh+en) exposed as
`POST /api/ocr` (JPEG body → `{"items":[{text,score,bbox}]}`) with the `ocr`
capability.

```toml
[ocr]
enabled = false
det_path = "models/ocr/ch_PP-OCRv4_det_infer.onnx"
rec_path = "models/ocr/ppocrv5_mobile_rec.onnx"
dict_path = "models/ocr/ppocrv5_dict.txt"
max_side = 960
det_threshold = 0.3
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch. |
| `det_path` | String | `"models/ocr/ch_PP-OCRv4_det_infer.onnx"` | DBNet text detector. |
| `rec_path` | String | `"models/ocr/ppocrv5_mobile_rec.onnx"` | CRNN text recognizer. |
| `dict_path` | String | `"models/ocr/ppocrv5_dict.txt"` | Recognition dictionary (shipped in git). |
| `max_side` | u32 | `960` | Longest image side fed to the detector (multiples of 32). |
| `det_threshold` | f32 | `0.3` | DBNet binarization threshold. |

### Per-camera stream keys (Web UI / API)

Some camera options are per-camera JSON config (set from the Cameras view or
`PUT /api/cameras/{id}`), not TOML — notably `substream`
(`{enabled, width, height, fps, bitrate}`, SPEC appendix A #20): a
low-resolution secondary H.264 stream exposed as `stream.sub.mse`, the RTSP
`/live/{id}/sub` mount and the ONVIF `sub` profile. Applies on the next
stream (re)start.

### [plugin] - Plugin Trigger Framework (SPEC appendix A #47)

Plugins are declarative manifests keyed by id (`[plugin.<id>]`): trigger
rules over the device's own event sources dispatching registered tools
(built-ins and MCP stdio servers). Every execution is reported on the
SSE bus as `plugin_invocation` (outcome ok | error | skipped).

```toml
[plugin.lamp-link]
version = 1
tools = ["mcp:home"]
triggers = [
  { on = "away_state", when = "armed", cooldown_secs = 60, run = [
    { tool = "lamp.switch", args = { room = "living room", on = true } } ] }
]
[plugin.lamp-link.permissions]
events = ["away_state"]   # minimal event set — rules outside it are skipped
egress = []               # outbound allowlist (informed-consent, as #44)
```

**Field Reference:** the section is an arbitrary-key manifest map with
nested trigger tables — see SPEC appendix A #47 for the full grammar.
All four trigger sources are live: `alarm` (`when` = ai/audio/zone,
empty = any), `away_state` (`when` = armed/disarmed, empty = both),
`schedule` (`when` = "HH:MM" local daily boundary, 20 s tick, one fire
per boundary minute via the cooldown), `audio_level` (`when` = numeric
threshold, fires at level >= threshold, cooldown-gated against the
10 Hz stream). Actions: `tool` (15 s timeout) | `speak` (TTS with the
playback self-mute; inactive TTS reports `skipped:tts-inactive`) |
`notify` (desktop toast; headless is an honest ok no-op). Default
cooldown 60 s. Manifests whose trigger sources are not in
`permissions.events`, or whose `schedule`/`audio_level` `when` is
malformed, are skipped at load with a warning.

### [database] - SQLite Database

Configure SQLite database path for camera settings, protocol configs, sessions, and users.

```toml
[database]
path = "~/.local/share/mibee-eye/mibee_eye.db"
```

**Field Reference:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | String | `~/.local/share/mibee-eye/mibee_eye.db` | SQLite database file path (XDG-compliant default) |

**Notes:**

- Uses XDG data directory for default path: `~/.local/share/mibee-eye/mibee_eye.db`
- Fallback to `/tmp/mibee-eye/mibee_eye.db` if XDG data dir unavailable
- Stores camera configs, protocol configs, sessions, users, and stream session data

## Configuration Validation

Configuration is validated on startup. The following rules apply:

- **Port constraints**: All ports must be > 1024 (web.port, rtsp.server_port, gb28181.platform_sip_port)
- **Port conflicts**: web.port must not equal rtsp.server_port
- **Rate limiting**: security.rate_limit_max must be > 0
- **GB28181 interval**: gb28181.register_interval_secs must be > 0 (if enabled)
- **RTMP push**: rtmp_push.reconnect_interval_secs must be > 0 (if enabled)
- **RTMP push**: rtmp_push.max_reconnect_attempts must be > 0 (if enabled)
- **Log level**: observability.log_level must be one of: trace, debug, info, warn, error
- **Recording path**: recording.path must not be empty
- **Recording segment**: recording.segment_duration_secs must be > 0
- **Sound events**: audio_ai.threshold must be within (0, 1], classes must
  not be empty and cooldown_secs must be > 0 (if enabled)

## Examples

### Production Configuration with All Protocols Enabled

```toml
# Production configuration with all outbound protocols enabled
[web]
port = 8443
host = "0.0.0.0"
advertised_host = "192.168.1.100"

[rtsp]
server_port = 8554

[rtmp_push]
enabled = true
push_url = "rtmp://your-nvr.example.com:1935/live"
app_name = "live"
stream_name = "webcam-stream"
reconnect_interval_secs = 5
max_reconnect_attempts = 10

[onvif]
enabled = true
device_name = "mibee-eye"
manufacturer = "MiBee"
model = "Rec-01"
serial = "NC00000001"
firmware_version = "1.0.0"
port = 3702

[gb28181]
enabled = true
platform_sip_address = "192.168.1.100"
platform_sip_port = 5060
device_id = "34020000002000000001"
username = "admin"
password = "secret123"
sip_domain = "3402000000"
register_interval_secs = 60

[recording]
enabled = true
path = "/var/lib/mibee-eye/recordings"
segment_duration_secs = 900
max_capacity_mb = 20480

[database]
path = "/var/lib/mibee-eye/mibee_eye.db"

[security]
rate_limit_max = 10
rate_limit_window_secs = 30

[observability]
otel_endpoint = "https://monitoring.example.com:4317"
log_level = "warn"

[observability.logs]
endpoint = "http://loki:3100/loki/api/v1/push"
batch_size = 100
flush_interval_secs = 5

[observability.logs.labels]
environment = "production"
service = "mibee-eye"
```

### Local Development Configuration

```toml
# Local development with minimal security constraints
[web]
host = "127.0.0.1"
advertised_host = "127.0.0.1"

[security]
rate_limit_max = 100

[observability]
log_level = "debug"
```

### Minimal Configuration

```toml
# Minimal configuration — most fields use defaults
[web]
port = 8443

[capture]
video_device = "/dev/video0"
audio_device = "default"

[recording]
enabled = true
path = "./recordings"
```

### Resource-Constrained Environment

```toml
# Optimized for low-resource environments
[web]
host = "127.0.0.1"

[observability]
otel_endpoint = ""
log_level = "error"

[security]
rate_limit_max = 5

[recording]
enabled = false
```

### Network-Specific Configuration

```toml
# Configuration for different network environments
[web]
host = "192.168.1.100"
advertised_host = "192.168.1.100"

[rtsp]
server_port = 8554

[capture]
video_device = "/dev/video2"
audio_device = "hw:1"

[rtmp_push]
enabled = true
push_url = "rtmp://streaming.example.com:1935/live"
```

### TLS Certificate Management

```toml
# Production with custom TLS certificates
[web]
port = 8443
# Supply your own CA-signed certificates at tls/cert.pem and tls/key.pem
# Certificates auto-reload on file mtime change
```

**TLS Certificate Notes:**

- Self-signed certificates are auto-generated on first run at `tls/cert.pem` + `tls/key.pem`
- Certificates support hot-reload on file mtime change
- Production environments should supply CA-signed certificates
- CommonName: CN=mibee-eye, SAN: mibee-eye.local
- Validity: ~30 days for auto-generated dev certificates

## Configuration Best Practices

1. **Never commit secrets**: Store sensitive configurations in `config.local.toml` which is gitignored
2. **Use descriptive file names**: `config.prod.toml`, `config.test.toml` for different environments
3. **Validate configuration**: Test configuration files before deployment (run app with --config flag)
4. **Document changes**: Keep configuration documentation updated with new options
5. **Monitor performance**: Use observability to track resource usage with different configurations
6. **Protocol defaults**: All outbound protocols (RTMP push, ONVIF, GB28181) default to OFF for security
7. **Port constraints**: Use ports > 1024 to avoid privilege requirements
8. **Advertised host**: Set `web.advertised_host` for multi-homed networks or behind NAT

## Troubleshooting

### Common Issues

1. **Port already in use**: Try different ports or ensure previous instances are stopped. Check port conflicts in validation.
2. **Device not found**: Verify device paths and permissions (Linux: `video` group membership). This is local-only capture — does not discover remote cameras.
3. **Rate limiting issues**: Check `security` section configuration. Ensure `rate_limit_max` > 0.
4. **TLS errors**: Verify certificate configuration and endpoints. Check `tls/cert.pem` and `tls/key.pem` existence.
5. **OpenTelemetry failures**: System continues to function without collector, but metrics may be lost. Validation passes.
6. **Port conflicts**: web.port must not equal rtsp.server_port. Validation will reject.
7. **GB28181 registration fails**: Check platform_sip_address, platform_sip_port, username, password. Platform must be reachable.
8. **RTMP push fails**: Verify push_url, app_name, stream_name. External ingest server must be accepting connections.
9. **Recording path not writable**: Ensure recording.path exists and is writable. Auto-pruning requires read/write access.

### Validation

Validate configuration files by starting the application with `--config`:

```bash
# Test configuration by starting the application
cargo run -- --config test-config.toml

# If configuration is invalid, startup fails with descriptive error
# Example error: "web.port: must be > 1024, got 80"
# Example error: "rtsp.server_port: must not equal web.port"
```

### Getting Help

For configuration issues:
1. Check this reference document
2. Review the source code in `src/config.rs` and `crates/web/src/config.rs` for authoritative defaults
3. Examine the default `config.toml` file
4. Check GitHub issues and discussions
5. Validate with startup: `cargo run -- --config your-config.toml`