# API Reference

## Overview

The mibee-eye REST API follows the **MiBee Camera Web API unified SPEC v1**
(`mibee-webui/SPEC.md` in the workspace) — the same contract as the Raspberry
Pi camera projects. All API endpoints use TLS and require session
authentication unless noted.

### Base URL
```
https://localhost:8443
```

### Response envelope (SPEC §0)

All JSON endpoints respond with the unified envelope:

- Success: `{"ok": true, "data": …}`
- Failure: `{"ok": false, "error": "<machine code>", "message": "<human text>"}`
  with a semantic HTTP status. Machine codes: `bad_request`, `unauthorized`,
  `forbidden`, `not_found`, `conflict`, `rate_limited`, `not_implemented`,
  `internal_error`.

Binary endpoints (snapshot / MJPEG / MSE / metrics / static assets) and the
SSE stream are outside the envelope. The legacy `/health` endpoint is also
unwrapped; `/api/health` is the canonical enveloped path.

### Authentication (SPEC §2)

Cookie session + CSRF double-submit:

1. `POST /api/auth/login` `{"username","password"}` → sets
   `session=<token>; HttpOnly; Secure; SameSite=Strict` (24 h) and
   `csrf-token=<token>` (readable by JS) cookies.
2. Every state-changing request (POST/PUT/DELETE/PATCH) must echo the
   `csrf-token` cookie value in the `X-CSRF-Token` header
   (login/setup/logout exempt).
3. First boot: `GET /api/auth/me` answers `503 setup_required`;
   `POST /api/auth/setup` creates the admin and signs in.

Login is rate-limited per IP (20 req/min) with per-username exponential
lockout after repeated failures.

## Endpoints

### Public

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/health` | `{"ok":true,"data":{"status":"ok","uptime":N}}` |
| GET | `/health` | Legacy unwrapped alias |
| GET | `/metrics` | Prometheus text exposition |
| GET | `/`, `/style.css`, `/js/{path}` | Embedded web UI (shared mibee-webui build) |

### Auth

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/auth/me` | `{"username","role"}` / 401 / 503 setup_required |
| POST | `/api/auth/setup` | First-boot admin creation; establishes a session |
| POST | `/api/auth/login` | Session login (rate-limited) |
| POST | `/api/auth/logout` | 204, clears the session |
| POST | `/api/auth/reset` | `{"old_password","new_password"}`; invalidates all sessions |

### Device (SPEC §3)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/status` | device_name / model / vendor / firmware / uptime / cameras |
| GET | `/api/capabilities` | SPEC superset (`multi_camera`, `camera_management`, `camera_control`, `devices`, `mjpeg`, `mse`, `events`, `config_apply`, …) plus the device-intelligence booleans `ai` / `zones` / `audio_ai` / `voice` / `chat` / `vlm` / `ocr` / `substream`, the host hardware probe extension fields `system` and `recommended_profiles`, `events` extended with the capability-gated names, and `resource` — the boot-time feature-admission snapshot (SPEC appendix A #40: `{mode, total_mib, available_mib, budget_mib, reserve_mib, features:[{name, cost_mib, admitted, reason}]}`, reason codes `off_config`/`off_budget`/`dependency`), plus `llm_tier` (full/mid/lite/manual) |

### Cameras (SPEC §4)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/cameras` | List cameras (enveloped array) |
| POST | `/api/cameras` | Create: `{"name","camera_type","config"}` → 201 |
| GET/PUT/DELETE | `/api/cameras/{id}` | Camera CRUD (partial update) |
| POST | `/api/cameras/{id}/start` / `stop` | Start/stop capture (409 when already running) |
| GET | `/api/cameras/{id}/snapshot` | JPEG snapshot |
| GET | `/api/cameras/{id}/live` | MJPEG multipart stream |
| GET | `/api/cameras/{id}/stream.mse` | Chunked fMP4 stream for MSE playback |
| GET | `/api/cameras/{id}/stream.sub.mse` | Low-resolution substream (per-camera `config.substream`; advertised via `capabilities.substream`) |
| GET | `/api/cameras/{id}/zones` | Saved zones: `{"zones":[{name, kind:"intrusion"\|"line_cross", points:[[x,y]…], dwell_secs}], "applied":"immediate"}` (capability `zones`) |
| PUT | `/api/cameras/{id}/zones` | Replace the zone list — body is a **bare array** of zones; structural validation (intrusion ≥ 3 points, line exactly 2) |

### Configuration (SPEC §5)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/config` | `{"settings": {…nested dotted keys…}, "protocols": {"onvif": …, "gb28181": …, "rtmp": …, "recording": …, "webrtc": …}}` |
| PUT | `/api/config` | Partial deep-merge of the document above; protocol sections hot-toggle ONVIF/GB28181 immediately (`config_apply.default = "immediate"`) |

This replaces the former `GET/PUT /api/settings` and the per-protocol
`/api/protocols/{name}` GET/PUT endpoints.

### Events (SPEC §6)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/events` | SSE stream (`text/event-stream`, 15 s keepalive). Events: `camera_added`, `camera_offlined`, `ai_detection`, `ai_model_changed`, `alarm` (SPEC §6: `camera_id`, `active: true`, `source: "ai"` or `"audio"`, `targets` / `class` + `score`, `timestamp` epoch-ms), `zone_event` (`{camera_id, zone, event, track_id, label, timestamp}`), `voice_transcript` (`{keyword, transcript, speaker, timestamp}` — `speaker` is the best-matching enrolled voiceprint, "" when unknown), `chat_reply` (`{source, reply, timestamp}`), `voice_decision` (`{camera_id, transcript, choice, confidence, act_probability, timestamp}` — the intent decision over a voice transcript), `alarm_description` (`{camera_id, alarm_timestamp, description, elapsed_s}`), `meeting_state` (`{camera_id:"all", meeting_id, status:"recording"\|"processing"\|"done"\|"failed", timestamp}` — meeting lifecycle, SPEC appendix A #27), `conversation` (`{id, conversation_id, origin, user_text, thinking[], reply_text, engine}` — one finished dialogue turn, SPEC §3.4; `thinking` may carry `source:"tool"` entries), `agent_step` (`{conversation_id, kind:"tool"\|"phase", state, tool?, args?, result?, duration_ms?}` — live agent tool executions and phase switches, SPEC §3.5). Each capability-gated event is only emitted while its engine is active. |

### On-device intelligence (device extension)

All endpoints below are capability-gated: the engine answers honestly when
inactive (`{"enabled": false}` shapes / `not_implemented`-class errors)
instead of pretending.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/chat` | Grounded dialogue: `{"text","history":[{role,content}],"vision"?}` → `{"reply","engine","grounded"}` (capability `chat`; voice-loop replies also arrive as `chat_reply` SSE). Every turn injects live scene grounding (【画面】detection labels + last VLM description, 【本机】clock/uptime/memory, optional 【联网】weather — appendix A #29). `vision:true` answers from a fresh frame via the VLM (`grounded:"vlm"`, slow on CPU), falling back to the grounded LLM. Cloud-configured devices answer cloud-first with local fallback; `engine` ∈ `cloud\|local\|vlm` discloses the actual path (#35). With tools registered (§3.5) text turns run the agent tool loop and the response carries the additive `tool_calls: [{name, args, ok, result, duration_ms}]` array (#43) |
| GET | `/api/audio/records` | Hearing records (capability `audio_records`): `{"records":[{id, kind:"sound"\|"voice", text, score, keyword, speaker, scene, media_ref, timestamp_ms}]}` (`scene` = what the camera saw at that moment, `media_ref` = the MP4 segment covering it when recording — #30-C), newest first; `?limit=N` (default 100, max 500), `?kind=sound\|voice` |
| DELETE | `/api/audio/records` | Clear every record → `{"applied":"immediate","removed":N}` |
| GET | `/api/voice/speakers` | Voiceprint profiles (capability `voice_speakers`, **no side effects**) → `{"speakers":[{id,name,dim,count,created_at}], "enrollment":{name,collected,needed}\|null, "capable":bool}` |
| POST | `/api/voice/speakers` | Begin enrollment: body `{"name", "utterances"?(default 3, 1..=10)}` — the next `utterances` wake words each collect one embedding sample (poll GET for progress); already-enrolled name → 400 |
| POST | `/api/voice/speakers/commit` | Persist a completed session → `{"enrolled","samples","dim"}`; incomplete → 400 |
| POST | `/api/voice/speakers/cancel` | Abandon the in-flight enrollment |
| DELETE | `/api/voice/speakers/{name}` | Delete a profile (memory + DB; unknown name → 404) |
| POST | `/api/ocr` | Body = raw JPEG bytes → `{"items":[{text, score, bbox}]}` (capability `ocr`) |
| POST | `/api/meetings/start` | Start a meeting recording (capability `meeting`) → `201 {"id","started_at_ms"}`; already recording → 409; engine inactive → 501. Emits `meeting_state` SSE `{camera_id:"all", meeting_id, status:"recording", timestamp}` |
| POST | `/api/meetings/{id}/stop` | Stop and trigger the **async** pipeline (diarize → transcribe → punctuate → name → persist) → `{"id","status":"processing"}`; id not the running session → 409; completion arrives via `meeting_state` SSE (`done`/`failed`) |
| GET | `/api/meetings` | Meeting list (newest first) → `{"meetings":[{id, started_at_ms, ended_at_ms, duration_ms, status:"recording"\|"processing"\|"done"\|"failed", num_speakers, num_segments, audio_path, error}]}` |
| GET | `/api/meetings/{id}` | Meeting detail → `{"meeting":{…}, "segments":[{start_ms, end_ms, speaker_index, speaker, text}]}` (ordered by start_ms; `speaker` is the matched voiceprint name, "" when anonymous — render "Speaker N" from `speaker_index`) |
| DELETE | `/api/meetings/{id}` | Delete the meeting, its segments (and any retained audio); unknown id → 404 |
| GET/PUT | `/api/cameras/{id}/zones` | See the Cameras table above |
| GET | `/api/ai/models` | Detection model store: available/active models |
| POST | `/api/ai/models/{id}/activate` | Activate an uploaded model |
| POST | `/api/ai/models` | Upload a model archive |
| DELETE | `/api/ai/models/{id}` | Delete an uploaded model |

### Conversation traces (SPEC §3.3)

Per-conversation model call-chain records: every model invoked on a
dialogue's answer path (decision triage, VLM, cloud LLM, local LLM, TTS)
produces a span with call order, duration, process-CPU delta and token
counts. The chain is also exported over OTLP when `otel_endpoint` is
configured.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/traces/conversations?limit=` | Recent conversation summaries (default 50, cap 200), newest first |
| GET | `/api/traces/conversations/{id}` | Full span list for one conversation; unknown id → 404 |

List item: `{"id","origin":"chat"|"voice","started_at_ms","duration_ms","turns","models":[...],"status":"ok"|"partial"|"error","open"}`.
Span object: `{"span_id","parent_id","model","variant","label","start_ms","duration_ms","cpu_ms","status","tokens_prompt","tokens_completion","attributes"}`.

### Conversation records (SPEC §3.4)

The human-readable dialogue turn log — what the user said (HTTP prompt or
ASR transcript), what the device "thought" (one summary entry per internal
model call or routing decision, failed fallback legs included), and what
the AI replied with which engine. Voice interactions happen away from the
browser; this is where they become visible. No-reply turns (decision said
ignore) are honestly recorded with `reply_text: null`.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/conversations?limit=` | Recent turns (default 50, cap 200), newest first |

Turn object:
`{"id","conversation_id","origin":"voice"|"http","started_ms","user_text","thinking":[{"source","model","note","duration_ms"}],"reply_text","engine":"cloud"|"local"|"vlm"|null}`.
Every finished turn also rides the SSE `conversation` event (gated by the
`conversations` capability). Stored in SQLite, FIFO-capped at 1000 turns;
`[conversations] enabled = false` turns recording off entirely (privacy
switch — the capability is not advertised either).

### Agent tools & skills (SPEC §3.5)

The dialogue assistant's tool registry — built-in device capabilities
plus deployer-registered MCP (Model Context Protocol 2025-06-18) stdio
subprocess servers. This listing is the "what is exposed to the model"
transparency surface; capability key `tools` (`{enabled, count}`).

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/tools` | Tool list → `{"tools":[{name, description, input_schema, source}]}`; `source` = `"builtin"` or `"mcp:<server>"` |

Built-ins: `time.now`, `weather.current` (requires `[tools] weather_city`),
`camera.snapshot` (fresh frame + viewing endpoint). Live executions ride
the SSE `agent_step` event and land in the conversation record's
`thinking` entries as `source:"tool"`.

### Away mode (SPEC §3.6)

The armed watch for when nobody is home: the device keeps analyzing the
live AI detection stream and records anomalies — person visitors get
greeted over the speaker and asked who they are (enrolled faces by
name), their answer is recorded alongside the snapshot and a VLM
description of the scene. Capability key `away`
(`{available, voice}`); arming requires the AI detection to be active.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/away` | Armed state → `{active, since_ms, voice, stats}` |
| POST | `/api/away` | Arm/disarm `{"active":bool}` (CSRF); 400 with reason when not armable |
| GET | `/api/away/events?limit=` | Event records, newest first (default 50, cap 200) |
| DELETE | `/api/away/events` | Clear all records **and snapshot files** (CSRF) |
| GET | `/api/away/events/{id}/snapshot` | Evidence JPEG for one event (404 when absent) |

Event object:
`{"id","camera_id","kind":"person"|"activity","started_ms","labels","face_name"?,"description"?,"visitor_reply"?,"snapshot"?,"state"}`.
Person states: `greeting → listening → answered|silent` (with the
deterrence knob on: `listening → reask → silent`), or terminal `known` /
`no_voice`; activity rows are `recorded`. Disarming returns an additive
`summary` field when events accumulated and the summary knob is on. Every create and
state migration rides the SSE `away_event` (upsert by `id`); arm/disarm
broadcasts `away_state`. Armed state survives service restarts. Records
are SQLite FIFO-capped at 1000; pruning and clear-all also delete the
snapshot files. Config lives in `[away]` (cadence, gap/cooldowns,
listen window, greetings); the six runtime features (voice arm/disarm,
disarm summary, deterrence, schedule, visitor push webhook, audio link)
are hot-configurable via `scene.away.*` in `PUT /api/config` — see
`docs/en/configuration.md`.

### Devices (SPEC §4.8)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/devices/video` | V4L2 video device enumeration |
| GET | `/api/devices/video/{index}/formats` | Supported capture formats |
| GET | `/api/devices/audio` | ALSA audio device enumeration |

### Protocol runtime (device extension)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/protocols/runtime-status` | `{"onvif":{"running":b},"gb28181":…,"rtmp":…}` |

## Web UI

The embedded frontend is the shared **mibee-webui** build (ES Modules,
zero build step) — the same UI as the Raspberry Pi camera projects,
rendered capability-driven. Source of truth: `mibee-webui/` in the
workspace (`make sync-notebook` copies it in).
