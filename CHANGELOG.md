# Changelog

## [Unreleased]

### Added

- **Four form-factor binaries (roadmap #6 M2).** The single `mibee-eye`
  binary became a family: `mibee-eye` (full), `mibee-cam` (visual AI +
  streaming, no mic/dialogue), `mibee-mic` (audio pickup: sound events,
  ASR, KWS, speaker), `mibee-talk` (intercom: mic upstream + speaker
  playback + PTZ proxy). A `FormProfile` gate force-disables config
  families outside the form before any engine builds — misconfigured
  deployments log why their AI did not load instead of silently
  over-spending; the gates run ahead of the resource gate (#40).
- **GB audio pickup channel (2026-10-22 NVR joint test, accepted).**
  Registering with a type-136 channel ID turns any audio-capable form
  into a network pickup: the platform auto-enrolls it as
  `encoding=audio`, auto-INVITEs `m=audio recvonly`, and the device
  streams 20 ms G.711 upstream (`gb28181.talkback_upstream`). Verified
  end-to-end on real hardware: registration→INVITE <0.5 s, 4×30.2 s
  segments at full 1504-frame cadence, zero transport drops, WAV export.
- **GB35114 A-level TOFU key.** `gb28181.gb35114.include_device_cert`
  carries the device certificate PEM in the Capability cnonce
  (`devicecert:` base64) for platforms that do not pre-provision it;
  the previously undocumented `gb28181.gb35114` sub-table is now in the
  configuration reference (zh/en), including the fail-closed semantics.
- **PTZ proxy (M4-a).** GB `PTZCmd` (A505) commands translate to an
  external ONVIF head: ContinuousMove/Stop, GotoPreset, SetPreset,
  RemovePreset (`gb28181.ptz_onvif_endpoint` + credentials); without a
  head configured PTZ stays a no-op ack.
- **TTS voice change (M4).** `[tts] voice_change = off|high|low` shifts
  playback pitch by resampling the synthesized WAV (1.22×/0.82×,
  verified live at exactly 1.220).
- **Audio telemetry (M3, SPEC #46).** Capture pipeline reports
  `capture_mode`/`source`; the assistant waveform renders with
  semantics per state (idle breathing / listening amplitude /
  thinking pulse / answering syllable envelope).
- **Per-capability model routing (M5).** `[models.<capability>]`
  overrides which model file a capability loads, privacy-facing
  capabilities stay local-first regardless of cloud configuration.
- **Plugin framework (M6, SPEC #47).** Four trigger sources (alarm
  rising edge, sound event, zone event, schedule) drive three action
  families (HTTP webhook, TTS announcement, recording gate); verified
  end-to-end on a live device.
- **Elastic resource controller (M7, SPEC #48).** L1 sheds the heavy
  model tail when available memory enters the warning band; L2 swaps
  the live LLM between tier ladders under memory pressure without a
  restart (same-path swaps are no-ops); L3 (model quality tiering)
  surfaces recommendations only. Closes issue #3 as the systemic fix
  for swap-creep "PC freeze" recurrence.
- **85% coverage gate (CI-enforced).** `tools/check-coverage.sh`
  ratchets line coverage (baseline 85.0, current 85.56) with three
  documented exemption classes: composition roots, model-gated
  inference paths, device-gated I/O.
- **Away mode (SPEC §3.6 / appendix A #44).** Armed watch over the live
  AI detection stream: person arrivals are recorded with evidence
  snapshots, greeted over the speaker (`greeting_known` by enrolled-face
  name / `greeting_unknown` + identity question), answered through a
  one-shot no-wake-word listening window (`[away] listen_secs`, new
  `VoiceEngine::arm_listen`), described asynchronously by the VLM, and
  desktop-notified. Configured activity labels (default pets) are
  recorded with cooldowns. `GET/POST /api/away`, `GET/DELETE
  /api/away/events`, `GET /api/away/events/{id}/snapshot`; SSE
  `away_event` (upsert by id) + `away_state`; capability
  `away:{available,voice}`. Records SQLite FIFO-capped at 1000 with
  snapshot files pruned alongside; armed state survives restarts.
  `[away]` config section (cadence, gap/cooldowns, listen window,
  greetings, snapshot dir).
- **Away mode runtime features (same-day amendment, all hot-configurable
  via `scene.away.*` in `PUT /api/config`).** ① Voice arm/disarm:
  short utterances ("我要出门了" / "我回来了") arm/disarm ahead of the
  LLM (keyword intent, question-shaped utterances excluded). ② Disarm
  summary: window stats upgraded to a one-liner by the local LLM —
  `POST /api/away` disarm response `summary` field, spoken on voice
  disarm. ③ Deterrence escalation: unanswered strangers re-asked once
  (`reask` state, half window) then played the deterrence message
  (`reask_text`/`deterrence_text`). ④ Scheduled arm/disarm
  (`schedule_arm`/`schedule_disarm` HH:MM boundary edges,
  `schedule_weekdays_only`). ⑤ Opt-in visitor push webhook
  (`push_url`; JSON POST, text-only, fail-open; empty = zero egress).
  ⑥ Audio link: voted sound alarms land as away activity records while
  armed (`audio_link`).

- **Agent tools & skills framework (SPEC §3.5 / appendix A #43).** The
  dialogue assistant can now call tools: built-ins (`time.now`,
  `weather.current` reusing the `[tools]` weather config, and
  `camera.snapshot` returning the fresh-frame viewing endpoint) plus
  **custom plugin servers via MCP (Model Context Protocol, spec
  2025-06-18) over stdio** — any MCP server executable configured under
  `[[agent.mcp_servers]]` is spawned, handshaked (newline-delimited
  JSON-RPC), its tools listed, and called during dialogue. Cloud
  providers use native `tools`/`tool_calls`; the local Qwen3 engine
  uses its native `<tool_call>` prompt format (llama-cpp-2 has no
  tools-aware chat template — the section is constructed in-product).
  Bounded loop (`max_steps`, default 3; last iteration drops the tool
  table to force a text answer), per-call timeout, tool output capped
  at 8 KiB before entering model context, tool failures fed back
  honestly. New config `[agent]` (`enabled`/`max_steps`/
  `step_timeout_ms`), new endpoint `GET /api/tools`, new SSE event
  `agent_step` (tool running/done/error + phase thinking/answering),
  new capability key `tools:{enabled,count}`; `/api/chat` responses
  carry the additive `tool_calls` array and conversation records gain
  `source:"tool"` thinking entries. Voice answers run the same loop
  (same fail-open: any error falls back to the legacy single-shot
  legs). Empty registry = plain dialogue, zero behavior change.

- **Assistant page redesign (mibee-eye-webui vNext, synced):** a
  full-width voiceprint hero (real `audio_level` envelope driving a
  scrolling multi-layer mirrored waveform with idle/listening/thinking/
  answering states), WeChat-style chat with history restored from
  `/api/conversations`, inline tool-call cards, collapsible thinking
  drawers, a live thinking side panel (`agent_step` timeline + latest
  turn's chain) and a tools & skills registry card.


- **Conversation records (SPEC v1 §3.4 + appendix A #41).** A persistent,
  human-readable log of every dialogue turn — voice utterances and web
  chats alike. Each turn carries the heard/input text, one "thinking"
  entry per internal model call or routing decision (decision triage,
  cloud chat/vision with their failure-and-fallback legs, local VLM/LLM
  with token counts, TTS playback), and the AI reply with the engine
  that produced it; no-reply turns (decision said ignore) are honestly
  recorded with `reply_text: null`. Served at `GET /api/conversations`
  (SQLite, FIFO-capped at 1000) and pushed live as SSE `conversation`
  events; the assistant page gains a "Conversation Log" card with
  collapsible thinking entries (capability-gated, negative-compatible).
  `[conversations] enabled = false` turns recording off entirely
  (privacy switch), and `DELETE /api/conversations` clears every stored
  turn (`{"applied":"immediate","removed":N}`; assistant-card button
  with a danger confirmation).

- **Desktop integration: tray icon + notifications (SPEC appendix A
  #42).** On Linux hosts with a desktop session (session-bus probe:
  `DBUS_SESSION_BUS_ADDRESS` or `$XDG_RUNTIME_DIR/bus`) the service now
  shows a StatusNotifierItem tray icon (left-click/menu opens the web UI
  via `xdg-open`, preferring `web.http_port` when set) and fires desktop
  notifications on every accepted alarm rising edge (visual / sound /
  zone); `notify_conversations` additionally notifies on completed
  voice replies. Pure-Rust D-Bus (ksni + notify-rust, zbus — no libdbus
  C library). Entirely fail-open: headless servers skip the feature with
  one log line. The service typically starts before the user logs in, so
  the tray keeps retrying registration every 60 s until a desktop
  session appears, and a failed notification pauses its channel and
  re-arms after 10 minutes — both come alive with the session, without
  log spam.

- **Boot-time feature resource gating (SPEC appendix A #40).** Small-memory
  hosts no longer launch every AI feature: `[resources] feature_gate = "auto"`
  (the default) estimates each enabled feature's resident cost from its model
  files and admits them greedily against the boot memory budget
  (`MemAvailable − reserve_mib`, default reserve 512 MiB) in a fixed priority
  order, shedding the heavy tail (VLM/LLM/…) when it does not fit.
  `feature_gate = "all"` restores the boot-everything behaviour. The decision
  table is exposed as `capabilities.resource` (status page "Resource Profile"
  card) plus `mibee_eye_resource_budget_mib` / `mibee_eye_feature_admitted`
  gauges; a runtime low-memory watchdog (three-state latch, 192 MiB) warns
  when available memory stays below the watermark.

- **Full observability: per-model resource metrics + conversation model-call
  chains (SPEC v1 §3.3 + appendix A #37-39).**
  - `/metrics` now exposes per-model families for every invocable model
    (vision/audio/OCR/LLM/VLM/cloud/decision/face/meeting/ASR/speaker/TTS):
    `mibee_model_inferences_total{model,variant}`,
    `mibee_model_inference_seconds` + `mibee_model_cpu_seconds` histograms
    (wall time and process-CPU delta per call), `mibee_model_inflight`,
    `mibee_model_errors_total`, `mibee_model_tokens_total` (prompt/completion
    for token-billed models — local llama.cpp counts real tokens, cloud usage
    comes from the provider response).
  - System/process resource gauges (`mibee_eye_system_*`,
    `mibee_eye_process_*`) mirror the `/api/metrics/summary` sampler onto the
    Prometheus surface.
  - `GET /api/traces/conversations[?limit=]` and
    `GET /api/traces/conversations/{id}`: per-conversation model call-chain
    records (call order, nesting, duration, CPU delta, tokens) — HTTP chat
    turns and voice wake-sessions (120 s slot, follow-ups included) each form
    one trace; bounded ring 200×64.
  - Every model invocation exports an OTel `model_call/<id>` span with
    `model`/`variant` attributes; conversations export a
    `conversation/<origin>` root span — external collectors (Jaeger/Tempo)
    get the full chain when `otel_endpoint` is set.
  - `capabilities.observability` gains additive keys `traces:true`,
    `model_metrics:true`.

- **Grounded dialogue (SPEC appendix A #29).** Every chat turn (web and
  voice alike) injects live scene grounding: the system turn carries a
  【画面】 block (fresh AI-detection label counts plus the last VLM alarm
  description, marked stale when lagging), a 【本机】 block (local clock
  with weekday, uptime, load, available memory — "what time is it" is
  answered from the clock, not model memory) and, when the utterance
  looks weather-related and `[tools] weather_enabled` is set, a 【联网】
  weather block. `vision:true` on `POST /api/chat` answers from a fresh
  camera frame via the VLM (`grounded:"vlm"`, tens of seconds on CPU),
  falling back to the grounded LLM on any failure; the `grounded` field
  discloses the actual path.

- **Dialogue scene capability pack (SPEC appendix A #30).** Continuous
  conversation (`[voice] follow_up_window_secs`, default off: after a
  spoken reply a VAD window accepts follow-ups without the wake word;
  requires the silero VAD model), per-language TTS (Cantonese / English
  / Mandarin models selected by reply language, falling back to the main
  model), LLM resource tiers (`[llm] model_path_mid` / `model_path_lite`
  picked by available memory when `[resources] auto_tier` is on), and
  correlated hearing records (`scene` summary + `media_ref` pointing at
  the MP4 segment covering the moment, when recording is on).

- **HTTP cloud routing for chat (SPEC appendix A #35).** With cloud AI
  configured, `POST /api/chat` goes cloud-first — text via `chat_model`,
  `vision:true` via `vision_model` on a fresh frame — wearing the same
  grounded system turn, and falls back to the local engines on failure
  (configurable). The `engine` field discloses which path answered
  ("cloud" | "local" | "vlm"); a custom OpenAI-compatible gateway can be
  set via `MIBEE_EYE_CLOUD_BASE_URL`.

- **Real-time voice waveform (SPEC appendix A #36).** The assistant chat
  panel gains a live microphone waveform strip fed by the `audio_level`
  SSE event (perceptual dBFS mapping — a linear RMS scale left real
  speech at an unreadable 0.03-0.1; attack/release smoothing, speech
  state highlights the mic icon).

### Fixed

- **Talkback upstream capture xruns (~13% frame gap → zero).** The
  upstream mic capture opened with `BufferSize::Default`; on hosts
  whose audio server pins small periods (~21 ms) cpal's reader thread
  overran ~2.7×/s and the platform measured a 13% frame supply gap.
  The capture now requests a ~100 ms Fixed period clamped to the
  device's supported range and retries with the device default when a
  host rejects Fixed sizes (observed: dsnoop advertising Unknown at
  config level while enforcing a hard range at build time — without
  the fallback the protocol fail-opens to 488).
- **The shipped `config.toml` was not valid TOML (blocks every fresh
  install).** Commit `78a7eeb` spliced the `[away]` block into `[agent]`'s
  comment anchor, leaving a stray `[agent]\` table header at line 305.
  `AppConfig::load` propagates the parse error, so the file refused to
  start — and the file is a *distribution artifact*: `scripts/deploy.sh`
  installs it as the live `config.local.toml` and the release workflow
  ships it as `config.example.toml`. It went unnoticed because no test ever
  parsed it. Both headers fixed, and
  `shipped_example_config_parses_into_app_config` is now the permanent
  gate: valid TOML, every table header maps to a real `AppConfig` section,
  the `[away]` keys land in `cfg.away` (the failure mode a missing header
  produces), and the result passes `validate()`.

- **Four rustls advisories were live on every outbound TLS connection**
  (RUSTSEC-2026-0346/0347/0348/0349, published 2026-10-14). rustls
  0.23.45 → 0.23.46, where all four are patched. 0349 is the one that
  matters here: an unauthenticated ServerHello can strip the post-quantum
  key share from later ClientHellos, which is exactly the path used for the
  cloud AI endpoint, the OTLP collector and the weather fetch. This went
  unnoticed because **no CI job ran an audit at all** — the "audit clean"
  note in the working log was a manual claim that had silently gone stale.
  A `cargo audit` job now runs in CI.

- **Credentials left the device in cleartext (SPEC §5 was never
  implemented).** The spec has mandated since v1 that secret config fields
  read back as `"****"` and that `"****"` submitted back means "keep the
  stored secret". Neither half existed — the placeholder appeared nowhere
  in the codebase. `GET /api/config`, `GET /api/protocols/gb28181` and
  `GET /api/protocols/rtmp_push` all returned the stored document
  verbatim, handing every browser session the GB28181 SIP Digest password
  and an RTMP push URL that conventionally embeds the stream key and
  userinfo. Both halves now exist: redaction is a depth-recursive match on
  a leaf-name allowlist (so a new subsection cannot opt out by nesting),
  and the restore is what makes redacting safe — the settings UI does a
  read-modify-write of the whole document, so without it, saving an
  unrelated setting would have persisted four asterisks as the live SIP
  password and silently broken Digest auth on the next re-register.

- **The auth rate limiter was bypassable and unbounded.** The per-client
  key came from `X-Forwarded-For`, which any client can vary to obtain a
  fresh bucket per request; and the tracked-key map only ever shrank on a
  successful login, so unauthenticated traffic grew a `HashMap<String, _>`
  without bound for the life of the process. Forwarding headers are now
  honoured only from a loopback peer (nginx's `set_real_ip_from`
  convention — unforgeable from the LAN, still works behind a same-host
  reverse proxy), the table prunes expired entries and caps at 8192 keys.

- **Three listeners leaked one task or one registry entry per cycle.**
  ① GB28181 frame bridges: the registry held the sender, so the bridge task
  could only be ended by the library unsubscribing — which it does not do
  on its `abort()` path. Every timed-out GB28181 hot-toggle orphaned a map
  entry, a `SyncSender` and a task still forwarding access units into a
  channel nobody reads. The registry entry now owns the `JoinHandle` and
  `stop_gb28181` drains it.
  ② RTSP: `live_streams` was insert-only, so a stopped camera stayed
  DESCRIBE-able and an NVR could PLAY it and hang waiting for frames that
  would never come; the registry also held its `broadcast::Receiver` via
  `std::mem::forget`, leaking a 300-slot ring per cycle. The receiver now
  lives in the entry that keeps it alive, and `stop_stream` unregisters
  both the main and the substream mount.
  ③ TTS: the interruptible playback pipeline (appendix A #45) spawned the
  audio player and polled it, but `std::process::Child` deliberately does
  not kill on drop. A future cancelled mid-playback — SIGTERM during a
  reply, an aborted round, a timeout — left the device speaking after the
  playback self-mute had already lifted, which is the self-talk loop #45
  exists to prevent, re-entering through the one path the explicit cancel
  flag cannot cover. The player is now wrapped in a kill-on-drop guard.

- **`chat_reply` and `voice_decision` were dead on arrival.** Both are
  emitted by the voice bridge but were absent from
  `capabilities.events`, and the frontend registers SSE listeners *only*
  from that list — so the payloads reached the wire with no handler, and
  the conversations-off fallback toast in `chat.js` could never fire. Both
  now advertised, under their real gates. A test scrapes
  `event_to_sse` for every wire name and requires each to be advertised, so
  the next event arm added without an advertisement fails the build.

- **`capabilities.face` was hardcoded `true`** while `[face]` defaults to
  off and both models are deployment assets, so a stock install rendered
  the "Enrolled faces" card and clicking enroll returned 501. It now
  follows the engine's real state.

- **A model swap froze the web server.** Building an ONNX session reads
  the model file and initialises the runtime graph — hundreds of ms to
  seconds — and the activate, upload-validation and model-manager handlers
  all called it synchronously from inside `async fn`, parking a tokio
  worker thread for the duration. On a single-core deployment that is a
  visible hang. Session construction now runs on the blocking pool; the
  per-frame inference path was already correct.

- **Three latent panics on degenerate input.** `packetize_pcm` divided by
  `sample_rate / 50` (0 for any rate under 50 Hz, which also made the send
  loop spin forever growing a `Vec`) and by `channels`; `hex_decode`
  sliced a `&str` at byte offsets and so panicked on non-ASCII despite
  documenting "returns an error"; and `PUT /api/config` used `unreachable!()`
  on a key list hand-kept in sync with its validator — after the row was
  already persisted, so any drift would turn a valid request into a 500
  plus a stored-but-unapplied setting.

- **The CI feature matrix only compiled, it never ran anything.** Every
  feature-gated module — the whole voice bridge, the tool/MCP agent loop,
  the model download manager, the VLM description layer — was type-checked
  and never executed once. The matrix now runs
  `cargo test --workspace --lib`; it immediately caught two stale call
  sites the default `test` job cannot see.

- **Voice reply latency 50-70 s → seconds, and clipped questions (2026-10-10 real-usage test).**
  Three compounding causes, all fixed:
  ① the agent tool prompt (Qwen3 `<tools>` section, pretty-printed) ran
  ~1300-1700 tokens against an `n_ctx` default of 1024 — overflowing the
  context and paying ~28 ms/token prefill on every turn. The section is
  now compact (single-line JSON, terse instruction), the `n_ctx` default
  is 2048, and `n_threads` defaults to half the logical cores (clamped
  2-4; more threads than physical cores measured slower on 4C/8T).
  ② `n_threads` was a dead config field — `LlamaContextParams` never
  received it (same for the VLM context), so every deployment ran
  llama.cpp's own thread default regardless of config; both contexts
  now apply `n_threads` (measured: honoring 4 threads on the 4C/8T test
  host roughly halves decode latency vs the stray default).
  ③ voice turns carried the tool table even for chit-chat — new
  `[agent] voice_tool_gate` (default on) includes tools only when the
  utterance matches a tool-intent heuristic; plain questions take the
  fast single-shot path (~1.4 s generation vs ~35 s+).
  ④ TTS synthesis was hardwired to 2 threads (~16 s for a 5 s
  utterance) — new `[tts] num_threads`, 4 on ≥4-core hosts halves it.
  ⑤ `[voice] capture_endpointing` (default on): the post-wake capture
  window is VAD-endpointed (speech slides the deadline up to
  capture_secs + 6 s, ~0.6 s of trailing silence ends it) — questions
  spoken after a pause after the wake word are no longer clipped to the
  first 4 s (the "asked several questions, only 2-3 characters came
  through" failure). WebUI: the voiceprint waveform applies a perceptual
  display curve (0.6 gamma) with an always-on midline so quiet speech
  reads as listening instead of a dead trace.

- **VLM intermittent degenerate replies — two root causes, both fixed.**
  Live vision answers occasionally returned looped gibberish (`Extra
  baseUrl…` repeated): ① a data race — llama-cpp-2 blanket-asserts
  `Sync` for `MtmdContext`, but the clip preprocessing reuses mutable
  scratch buffers, and the alarm-frame describer could overlap a chat
  vision answer on the same engine (the ignored concurrent diagnostic
  now **segfaults** on the unfixed code, proving the race); all VLM
  inference is now serialized behind an engine mutex. ② greedy decoding
  itself loops on list items in hard frames even without any race — a
  llama.cpp-style repetition penalty (`[vlm] repeat_penalty`, default
  1.1, over the last 64 tokens) breaks the loops. Degenerate replies
  (if any still occur) are WARN-logged with the reply text via a pure,
  unit-tested repetition detector. Post-fix diagnostic: four concurrent
  answers on one engine, all coherent.
- **Deployed migrations can no longer be silently missing.** Migrations
  are now embedded in the binary at compile time (`include_dir`), with
  site-local directories as an optional overlay. Previously a deployed
  instance relied on `migrations/` next to the working directory —
  forgetting to copy a new file left the new tables missing while
  inserts only WARNed (the `conversation_turns` incident).
- **Audio capture test flake**: a closed capture channel (device
  contention with another mic holder) now skips like the other
  no-hardware paths instead of failing the suite.

- **Dialogue dialect & quality fixes**: Cantonese vernacular detection
  (而家-class questions kept their written-form characters through ASR
  and mis-routed the reply language), explicit reply-language nudges,
  per-language TTS models needing their own companion tokens table (the
  Cantonese model read the Mandarin table's path and exited 255 — the
  table is now derived from each model's own directory), TTS synthesis
  on 2 threads (halves spoken-reply latency on 4+-core hosts), a
  weekday-name off-by-one (Sunday-first array met a Monday-first `%u`),
  and an `llm_tier` newtype fix (the bare `Arc<String>` Extension
  collided with the advertised-host slot and reported the LAN IP as the
  tier).

- **face.recog decoder fixes** (was failing on every detection event,
  fail-open had hidden it): the zoo 2023 YuNet checkpoint needs its
  declared fixed input size (config'd 320 was rejected), its output is
  multi-head cls/obj/bbox (decoded per the model author's reference
  implementation — score = cls·obj, no sigmoid, stride-anchored boxes),
  and SFace's graph input/output names are captured at load instead of
  hardcoded. Verified on a real face photo: score 0.83 with an accurate
  box, and the production instance went from constant failures to 87
  inferences with zero errors.

- **GB28181 SIP password no longer logged**: the `start_gb28181`
  tracing span recorded the whole runtime config — password included —
  into journald; the config is now skipped (identifying fields were
  already logged by the INFO line inside).


- SPEC appendix A #11 stale note corrected upstream: this device has
  implemented §3.2 since the 2026-10-04 audit (notebook side of webui #47).

All notable changes to MiBee-Rec are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] — 2026-09-29

Capability package: voice/audio expansion (meeting minutes, speaker
voiceprints, trilingual ASR, Laya decision assist) plus the ONVIF
capability adoption batch. Version numbering is independent of the Pi
twins since this release train (撞号 with their v0.5.0 is coincidental
format alignment).


### Added

- **Meeting mode** (SPEC appendix A #27): on-demand local meeting
  minutes — explicit `POST /api/meetings/start` → `stop` recording
  sessions (nothing is recorded outside a session; WAV deleted after
  processing unless `meeting.keep_audio`), then offline speaker
  diarization (pyannote segmentation + CAM++ embedding + fast
  clustering) → per-segment trilingual ASR → punctuation restoration →
  per-speaker voiceprint voting for names. New endpoints
  `GET/DELETE /api/meetings`, `GET /api/meetings/{id}`; new SSE
  `meeting_state`; new tables `meeting_records` + `meeting_segments`
  (migration 007); capability `meeting`; auto-stop at
  `max_duration_secs`; `--selftest-meeting <wav>` diarization
  self-test. Records view gains a meeting-minutes card.
- **Speaker voiceprints** (SPEC appendix A #25): enroll wake-word
  voiceprints (3D-Speaker CAM++ via sherpa-onnx, zh_en checkpoint),
  gate wake words on a known speaker (`voice.speaker_verify`, fail-open
  without profiles), and attribute voice hearing records / SSE
  `voice_transcript` to the best-matching speaker. New endpoints
  `GET/POST /api/voice/speakers`, `POST …/commit`, `POST …/cancel`,
  `DELETE …/{name}`; new table `voice_speakers` (migration 006) +
  `hearing_records.speaker` column; Records view gains a speakers card
  and speaker badges. Verified end-to-end acoustically on the
  workstation (enroll → gate pass → tagging → stranger rejection).
- **Trilingual ASR** (Cantonese / Mandarin / English): the optional
  `sherpa-onnx-paraformer-trilingual-zh-cantonese-en` checkpoint
  (234MB int8, Apache-2.0) drops into `[voice] paraformer_model` —
  verified on Cantonese and mixed zh-en samples.
- **Voice decision assist** (SPEC appendix A #26): `[decision]` section
  runs Laya typed decisions (multilingual ONNX, Apache-2.0) over each
  transcript before the local LLM answers; `ignore` skips the reply,
  decisions ride a new `voice_decision` SSE event. Fail-open below
  `min_confidence` or without models.

### Fixed

- Voice enrollment no longer collects samples past `needed` (extra wake
  words between completion and commit were inflating the count — caught
  by the workstation acoustic E2E).

- **Sound events, tracking + zones, OCR** (SPEC appendix A #21): constant
  microphone listening with YAMNet classification (3-vote smoothing,
  per-class cooldown, Silero-VAD voice presence) emitting `source: "audio"`
  alarms; a ByteTrack-subset tracker over the visual detections with
  user-drawn intrusion/tripwire zones (`GET/PUT /api/cameras/{id}/zones`,
  `zone_event` SSE); PP-OCR v4/v5 text recognition (`POST /api/ocr`). All
  opt-in `[audio_ai]` / `[ocr]` config, fail-open.
- **Voice interaction closed loop** (SPEC appendix A #22): wake word 小蜜蜂
  (sherpa-onnx zipformer KWS) → `capture_secs` of audio → offline
  paraformer zh transcription (`voice_transcript` SSE) → local LLM reply
  (`chat_reply` SSE) → TTS playback via the `sherpa-onnx-offline-tts` CLI
  subprocess (GPL espeak-ng isolated outside this binary). Requires the
  `voice` cargo feature; opt-in `[voice]` / `[llm]` / `[tts]` config.
- **Local LLM dialogue**: `POST /api/chat` (`{"text","history"}` →
  `{"reply"}`) plus the web chat panel, llama.cpp + Qwen3 GGUF, greedy
  decoding with `/no_think`.
- **Alarm-frame descriptions** (SPEC appendix A #23): Qwen3-VL (llama.cpp
  mtmd) describes the triggering JPEG of each accepted visual alarm —
  asynchronous `alarm_description` SSE that never delays the alarm;
  single-flight scheduler with a 120 s inter-description floor so flapping
  detectors cannot stack inference. Opt-in `[vlm]` config, `llm` feature,
  memory guardrail before load.
- `--selftest-audio/-ocr/-voice/-llm/-tts/-vlm` offline one-shot CLI
  self-tests (JSON result per engine) and `models/README.md` documenting the
  untracked model assets.
- Documentation: new bilingual **User Guide** (`docs/{en,zh}/user-guide.md`)
  and on-device-intelligence sections across configuration / API /
  architecture / installation / getting-started docs and both READMEs.

### Changed

- Inference thread pools are auto-capped (`OMP_NUM_THREADS` = cores/2, max 4)
  unless the environment sets it; llama.cpp is linked with minimal features
  (its `common`/`android-shared-stdcxx` features abort at static init when
  sherpa-onnx is also linked).
- ALSA capture requests a 2048-frame period clamped to the device range —
  eliminates xruns on small laptop codecs when inference threads preempt the
  audio callback.

### Fixed

- **Hearing dead for a whole run after one lost monitor build**: the audio
  monitor is now opened with a bounded retry (3 s × 10) at startup, and when
  the device rejects the widened 2048-frame period (PipeWire-era ALSA
  plugins enforce the current quantum — observed `341..=342` frames), the
  monitor falls back to the device default instead of pausing sound events
  and the wake word until the next restart. Full error chains are logged.
- llama-cpp-2 batch-relative logits indexing (prompt decode vs generation
  step); `«think»` blocks stripped from replies; one shared llama.cpp
  backend instance instead of per-engine loads.
- Zones PUT accepts the bare zone array the API contract specifies (the
  shared frontend sent a wrapped object; mock leniency had masked it).
- CI feature matrix covers `no-ai` / `voice` / `llm` / `voice+llm` / `vlm`
  builds (vlm-gated mtmd code and audio_ai no-ai hygiene regressions).

## [0.4.0] — 2026-09-27

- **Low-resolution bandwidth-saving substream** (SPEC appendix A #20,
  default off): per-camera `config.substream` `{enabled, width, height,
  fps, bitrate}` (defaults 640x360 / 400 kbps / fps 0 = follow) adds a
  second OpenH264 encoder session fed by the downscaled, already
  rotated/flipped/watermarked frames — a bounded-tap design that never
  stalls the main pipeline (encoder init failure disables the substream
  for the run, fail-open). Exposed as `GET
  /api/cameras/{id}/stream.sub.mse` (+ `capabilities.substream` while
  any active stream runs a substream), the RTSP `/live/{id}/sub` mount
  (`find_live_stream` now resolves longest-match-wins so the sub mount
  can never be captured by the shorter main path) and the ONVIF `sub`
  profile (onvif-device-rs multi-profile, git pin 1826726; advertised
  after the primary; applies on the next protocol toggle). Structural
  validation at the PUT boundary (even dims, sane bitrate) and at stream
  start; applies on stream (re)start like the other per-camera keys.
  `encoder::convert::Yuv420p::downscaled` is the new nearest-neighbour
  scaler (twin of the rs/go implementations).
- **Device-level rotation baked into the stream** (SPEC v1 appendix A
  #19): per-camera `config.rotation` (0 | 90 | 180 | 270, clockwise
  degrees) rotates the frames before encoding — every consumer (RTSP,
  MSE, recordings, snapshots, AI) sees it, and 90/270 swap the stream
  geometry (encoder config + published `StreamDimensions`, so the MP4
  muxer matches). Applied before `hflip`/`vflip`; 180° folds into the
  flip pass. Takes effect on stream (re)start like the flips — the web
  UI camera card gains a rotation button that cycles 0→90→180→270 with
  the same stop→start cycle (webui PR #11). `PUT /api/cameras/{id}`
  rejects invalid values with 400; the MJPEG passthrough tap is disabled
  while rotated (re-encodes from the rotated YUV).
- **Device serial identity** (issue #18): an unset
  `protocols.onvif.serial` no longer reports the shared `NC00000001`
  default — explicit config wins, otherwise the device identity is
  probed (cpuinfo `Serial` / Linux machine-id, cached for the process
  lifetime); the shared default is the last resort with a warning. Two
  distinct installs can no longer collapse into one NVR stable_id.
- **ONVIF Pull-Point events service** (onvif-device-rs 0.7): AI motion
  alarms now also publish as `tns1:VideoSource/MotionAlarm` (Source =
  the camera UUID) while an NVR holds a pull-point subscription — the
  same accepted rising edge that feeds the GB alarm NOTIFY and the
  SPEC v1 §6 `alarm` SSE event. New `protocols.onvif.events_enabled`
  key (default `true`, absent DB rows parse as enabled; protocol
  re-toggle applies). The `alarm` SSE event is now advertised with AI
  active instead of requiring a running GB28181 protocol — each alarm
  channel no-ops independently until its consumer is up/subscribed.- **Device-level rotation** baked into the stream (SPEC appendix A #19),
  with the 180° fold pinned by frame-level regression tests.
- **Talkback upstream** (GB/T 28181 §9.2 send half): microphone audio to
  the platform as G.711 A-law, following the negotiated law (PCMU) per
  session; **voice broadcast receive** completes full-duplex talk.
- **Seamless MSE reconnect**: the live timeline continues across
  connection drops (SPEC §4.1).
- **ONVIF**: AI motion alarms as Pull-Point MotionAlarm events; unset
  device serial no longer reports the shared NC00000001 identity.
- **Fail-open audio device list**: `/api/devices/audio` degrades to an
  empty list when the host cannot enumerate inputs (a user service
  without audio-device access), and per-device config rows are capped —
  the previous multi-megabyte dump was silently 500'd by the response
  envelope middleware, which now fails loudly instead.
- **AI model panel fixed**: the model select rendered empty and the
  upload button stayed hidden (request wrapper passed instead of the
  unwrapped payload).
- Security dependency refresh: onvif-device-rs quick-xml 0.41
  (RUSTSEC-2026-0194/0195), h2 / rustls / crossbeam updates.
- Shared frontend: explicit username login field, alarm toasts, mobile
  toolbar wrap, live-view rotate button, substream quality toggle.

Synchronized with [mibee-eye-go](https://github.com/xiqing85/mibee-eye-go/releases/tag/v0.4.0) and [mibee-eye-rs](https://github.com/xiqing85/mibee-eye-rs/releases/tag/v0.4.0) (same version, same day).
## [0.3.0] - 2026-09-16

Late join to the v0.3.0 train (user decision): the version number aligns
with the raspi twins, whose synchronized v0.3.0 shipped the
GB/T 28181-2022 device-role full coverage. This release brings the
notebook to the same device surface and takes one observation seam
further.

### Added — GB28181 device-surface parity with the raspi twins (v0.3.0 train)

- **FrameMirror (DeviceConfig A.2.3.2.9)**: the platform's runtime mirror
  mode (0-3 per A.2.1.22) now flips every camera's frames — implemented
  as device-level atomic flags shared by all capture loops and
  XOR-composed with each camera's static `hflip`/`vflip` mount
  compensation (two same-axis mirrors cancel). Baked into everything
  downstream: encoder (RTSP/MSE/recordings/GB28181), snapshots and the
  JPEG tap (MJPEG passthrough correctly disabled while mirroring).
  BasicParam keeps the reject posture, matching the raspi twins.
- **SIP-Date drift observation (§9.10.2)**: the platform clock from
  REGISTER response `Date` headers is polled once a minute; drift beyond
  5s WARNs (first excursion and every further 5s move; recovery logs
  once). Observation only — the system clock is never adjusted by the
  app. Ahead of the raspi twins, which expose the library seam but do
  not observe it yet.

## [0.2.0] - 2026-09-16

### Changed — Brand rename to `mibee-eye` (family-wide packaging convention)

The product/binary/service name is now **`mibee-eye`**, aligning with the
MiBee Eye packaging convention (the Raspberry Pi Go twin ships the same
binary name). The repository stays `mibee-eye-notebook`.

- Cargo package and binary: `mibee-rec` → `mibee-eye`
- systemd unit: `mibee-rec.service` → `mibee-eye.service` (binary path
  `/usr/local/bin/mibee-eye`, `User=mibee-eye`, `StateDirectory=mibee-eye`)
- Default database file: `mibee_rec.db` → `mibee_eye.db` (clap default and
  the XDG default `~/.local/share/mibee-eye/mibee_eye.db`)
- Prometheus metric prefix: `mibee_rec_*` → `mibee_eye_*`
  (**breaking for dashboards** — update your queries)
- TLS dev certificate identity: `CN=mibee-rec` / `SAN=mibee-rec.local` →
  `CN=mibee-eye` / `SAN=mibee-eye.local`
- RTSP realm (`mibee-eye RTSP Server`) and `Server` header; RTSP SDP
  session name; GB28181 SIP `User-Agent: mibee-eye/<version>`; ONVIF
  default `device_name`; OpenTelemetry service name; capabilities
  `device.name`
- Docker: paths `/usr/local/share/mibee-eye`, `/var/lib/mibee-eye`, image
  user `mibee-eye`; container migration search path updated accordingly

**Migration steps:**
1. Rename the database before upgrading (stop the service first, WAL
   files included): `mv mibee_rec.db* mibee_eye.db*` — or keep the old
   path by passing `--db-path` explicitly.
2. Update Prometheus/Grafana queries referencing `mibee_rec_*` metrics.
3. Delete `tls/cert.pem` and `tls/key.pem` before restarting (new
   identity is regenerated automatically); old certificates keep working
   until then.
4. If using systemd: install `mibee-eye.service` and
   `systemctl disable --now mibee-rec.service` first.
5. If using Docker: update container name and volume paths from
   `mibee-rec` to `mibee-eye`.

## [0.1.0] - 2026-09-16

First public release (Apache-2.0), open-sourced as
[mibee-eye-notebook](https://github.com/xiqing85/mibee-eye-notebook) — the
notebook/desktop member of the MiBee Eye camera family. The sections below
cover the final stretch of the pre-open-source line, most recently the
unified SPEC v1 web API with the shared web UI and the GB28181-2022
device-surface parity batch.

### Added — GB28181-2022 device-surface parity with the raspi twins

- **Alarm pipeline**: AI detection rising edges fire the SPEC v1 §6
  `alarm` SSE event and a GB28181 Alarm NOTIFY (priority 4 / method 5 /
  type 2, 2022 standard table) through the gb28181-rs notifier seam;
  per-camera rising-edge cooldown (`alarm_cooldown_secs`, default 30)
  and runtime gate from the platform's DeviceConfig AlarmReport switches
  (`alarm_notify_enabled` initial value).
- **DeviceControl family**: IFrameCmd forces the next OpenH264-encoded
  frame to an IDR; RecordCmd gates local recording (the current MP4
  segment closes on pause, the next keyframe reopens); no-actuator
  commands (PTZ/Guard/TeleBoot/HomePosition/DragZoom) ack as no-ops.
- **Graceful deregistration**: SIGTERM / protocol stop sends REGISTER
  `Expires: 0` (library 401 dance, 2s timeouts) before teardown.
- **MobilePosition**: static coordinates
  (`position_longitude` / `position_latitude`, empty = off) reported on
  the subscription cadence.

### Changed — Unified Web API SPEC v1 + shared web UI

**Breaking** (web API): the REST surface now follows the MiBee camera
unified SPEC v1 (`mibee-webui/SPEC.md`), same contract as the Raspberry Pi
camera projects:

- All JSON responses enveloped: `{"ok":true,"data":…}` /
  `{"ok":false,"error":"<machine code>","message":…}` (middleware; binary
  endpoints and the legacy `/health` alias stay unwrapped)
- `GET/PUT /api/settings` and `GET/PUT /api/protocols/{name}` replaced by
  `GET/PUT /api/config` (partial deep-merge; protocol sections still
  hot-toggle ONVIF/GB28181 immediately). `/api/protocols/runtime-status`
  remains as a device extension
- New: `/api/health` (public enveloped), `/api/status`, SPEC capability
  superset on `/api/capabilities` (host probe kept as `system` /
  `recommended_profiles` extension fields)
- `POST /api/auth/setup` now establishes a session (signs the new admin in)

**Changed** (frontend): the Preact SPA is replaced by the shared
**mibee-webui** vanilla ES Modules build (same UI as the Pi cameras,
capability-driven). Removed: `crates/web/build.rs` (esbuild bundling),
`package.json`, `static/src/`, dead `static/app.js`, Dockerfile
node/npm/esbuild steps — a fresh clone now builds with cargo alone.

### Fixed — GB28181 PS-over-RTP interop

PS muxer added; Gb28181Output rewritten from RFC-6184 to PS-over-RTP (PT=96); SSRC parsing fixed to use `y=` SDP field instead of `a=ssrc:`; Digest auth default changed from SHA-256 to MD5; Contact header fixed to use device IP; INVITE handler now sends 200 OK with device SDP answer.

### Added — GB28181 Catalog/DeviceInfo/Keepalive MESSAGE handling

MANSCDP+xml module; inbound MESSAGE dispatcher; keepalive heartbeat loop; device SDP answer with echoed SSRC on INVITE 200 OK.


### Changed — ffmpeg dependency fully removed (native codec stack)

All five former `ffmpeg` subprocess invocations have been replaced with
in-process Rust crates. **`ffmpeg` is no longer a runtime dependency** —
the Docker image, systemd service, and bare-metal install instructions
no longer require it. This eliminates per-source subprocess overhead
(~64 MB/camera), removes the noisy stderr log spam, and makes the
encoding pipeline crash-recoverable without process supervision.

| Former ffmpeg role | Replacement | Crate / module |
|--------------------|-------------|----------------|
| H.264 video encode (V4L2 → libx264 → Annex B) | OpenH264 (Cisco BSD-2) | `openh264` via `streaming::encoder::h264` |
| MJPEG → pixels decode | `jpeg-decoder` | `streaming::encoder::convert` |
| YUYV → JPEG (preview re-encode) | `jpeg-encoder` | `streaming::encoder::convert` |
| AAC audio encode | G.711 μ-law (default) / FDK-AAC (`aac` feature) | `streaming::encoder::audio` |
| MP4 segment muxing | `muxide` (pure-Rust fMP4) | `streaming::output::file` |
| JPEG snapshot (RTSP → 1 frame) | direct cache read from capture loop | `web::routes::streams::snapshot` |
| MJPEG live preview (RTSP → transcode) | JPEG broadcast subscription | `web::routes::streams::live_preview` |

**Architecture changes:**
- `VideoCaptureSource` re-activates the previously-disconnected
  `capture::video::VideoCapture` (nokhwa) path — the camera is now read
  natively via V4L2 in-process, frames converted to YUV420p, and encoded
  to H.264 via OpenH264. The `v4l2-ctl` capability-detection subprocess
  is also gone (nokhwa's `compatible_camera_formats()` is used instead).
- A JPEG tap is maintained in the capture loop: for MJPG cameras the
  raw JPEG bytes are forwarded zero-cost; for YUYV-only cameras a JPEG
  is re-encoded every Nth frame. The snapshot and live-preview endpoints
  consume this tap directly — no RTSP loopback, no transcoding.
- The `MediaFrame::Video` NAL-per-frame contract is preserved (start
  code stripped, `data[0] & 0x1f` is the NAL type), so the downstream
  protocol stack (RTSP/RTMP/GB28181/ONVIF) required **zero changes**.
- `AudioCaptureSource` now encodes i16 PCM to G.711 μ-law in-process
  (pure Rust, reuses `protocols::audio_codec`). AAC encoding is gated
  behind the new `aac` cargo feature (`fdk-aac`) for RTMP/MP4 audio
  compliance — off by default.
- `FileOutput` now uses `muxide` to write rolling MP4 segments directly,
  with no subprocess. Segment rotation happens at keyframe boundaries.
- Profile/level: Baseline, low complexity, GOP = 1 s — mirrors the old
  `libx264 -preset ultrafast -tune zerolatency -g 30` baseline.

**Cross-compile / deploy:** added `scripts/docker-build.sh`,
`scripts/deploy.sh`, `scripts/service.sh`, and four test scripts
(`test-smoke.sh`, `test-perf.sh`, `test-features.sh`, `soak-report.sh`)
to cross-compile from Windows via Docker and deploy to the two test
devices over SSH. See `scripts/README.md`.

### Fixed — MJPEG live preview corruption + camera view re-entry crash
- **MJPEG live preview corruption resolved** (horizontal tearing, green blocks, mosaic artifacts):
  - Root cause 1: ffmpeg capture command missing `-pix_fmt yuv420p` — pixel format mismatch caused decoder corruption.
  - Root cause 2: `-tune zerolatency` enabled sliced threading, producing 8 slice NALs per frame. Each slice incremented the RTP timestamp → 8× too fast (24000/frame instead of 3000), causing complete frame misalignment.
  - Root cause 3: SPS/PPS re-sent before every P-slice (8× per frame) flooded the broadcast channel, causing IDR frame fragments to be dropped.
  - Fix: added `-pix_fmt yuv420p` + `-threads 1` to force single-slice frames; RTP timestamp now only increments for slice NALs (types 1/5) per RFC 6184 §5.1; broadcast channel capacity increased 64→300.
- **Camera view re-entry crash fixed** ("Cannot set properties of null"): `_k` re-entry guard in router `r()` and `K()` prevents hashchange-triggered double-entry; `setText()` null-safe helper wraps all `textContent` assignments.

### Fixed — production hardening (security + reliability)
- **Mutex poisoning eliminated**: `resource.rs` and `rtsp_server.rs` now use `parking_lot::Mutex` (non-poisoning). Eliminates crash-on-panic cascade.
- **CSRF bypass on MJPEG endpoint fixed**: `server.rs` CSRF middleware now correctly covers all state-changing routes.
- **Crypto RNG hardened**: `thread_rng()` replaced with `OsRng` in all cryptographic code paths (session tokens, CSRF tokens).

### Added — Authoritative product positioning
- New [`docs/POSITIONING.md`](docs/POSITIONING.md) (bilingual zh/en) — authoritative product scope, target users, deployment model, capture scope, protocol scope, recording scope, security posture, observability, platform roadmap, UX requirements. **Any conflict between POSITIONING.md and other docs → POSITIONING.md wins.**
- Engineering guidance docs rewritten: added Product Scope, Platform Support, UX Requirements, Observability Stack, Security Posture sections; removed stale Architecture Mismatch Notes (those refactors are already done); accurate Project Status reflecting what is wired vs unwired vs missing.
- README.md Protocol Support Status table replaced with accurate component-level table: Auth no longer marked as stub (fully implemented); only RTSP is wired into runtime; RTMP/ONVIF/GB28181 marked as code-complete-but-not-wired; browser preview, local recording, i18n, theme, CSRF/CSP, remote log shipping, Windows/macOS all marked missing.

### Decision log (locked-in by product owner)
- **GB/T 28181**: retained as first-class protocol (2033 LOC implementation preserved; surfaced as same-tier toggle as ONVIF/RTMP/RTSP in Web UI).
- **Browser live preview**: in scope for v1 (MSE or JPEG sequence; WebRTC explicitly deferred).
- **NVR integration**: both pull (RTSP server) AND push (RTMP/GB28181) supported, all default-OFF, individually toggled via Web UI.
- **Local recording**: MP4 segment archive, configurable segment duration + total capacity + auto-prune-oldest, per-stream toggle.
- **Web UI security**: strong password + session + rate-limit + CSRF + CSP + login-failure lockout + HTTPS-mandatory. 2FA / mTLS explicitly out of scope for v1.
- **Platform**: Linux Tier 1 (only platform that compiles today); Windows v1.1, macOS v1.2 (planned, not blocking).

### Changed — Phase 1 P0 production blockers (5 of 6 resolved)
- **Hardcoded `localhost` eliminated**: new `[web] advertised_host` config option (empty = auto-detect LAN IP via UDP probe). All URL construction sites (RTSP base, snapshot, ONVIF XAddrs) now use the resolved host. Web UI no longer fabricates `rtsp://localhost:8554/...` fallbacks.
- **Mutex poisoning crash chain fixed**: `crates/security/src/rate_limit.rs` now uses `parking_lot::Mutex` (non-poisoning). `auth.rs` `SystemTime::now().unwrap()` calls replaced with `safe_epoch_secs()` helper that falls back to UNIX_EPOCH + logs error.
- **Rate limit now resets on successful login**: `login_handler` extracts client IP (X-Forwarded-For → X-Real-IP → fallback) and calls `reset_rate_limit` after bcrypt verify, so legitimate users don't get locked out after one typo.
- **Protocol configs persisted to SQLite**: new migration `002_protocol_configs.sql`. `routes/protocols.rs` rewritten to read/write DB (was in-memory `HashMap`, lost on restart). `main.rs` seeds DB from `config.toml` on first run only. Subsequent runs use persisted values — Web UI config changes now survive restart.
- **Protocol config type validation**: PUT handlers schema-validate per-protocol (onvif/gb28181/rtmp_push). Numeric fields (`port`, `register_interval_secs`, etc.) and booleans (`enabled`) sent as strings by legacy frontends are coerced. Unknown fields rejected with descriptive error.
- **RTMP push wired into StreamManager**: `StreamManager::create_stream` now reads `rtmp_push` config from DB; when `enabled=true` and `push_url` non-empty, auto-attaches `RtmpOutput` to the hub. `StreamInfo` exposes `rtmp_url` field. Toggle takes effect on next stream start (no server restart needed).
- **ONVIF + GB28181 startup reads DB**: `main.rs` now checks the `enabled` flag from SQLite (source of truth after first run) instead of `config.toml` only. Web UI toggles for these protocols now take effect on next server restart.

### Resolved — GB28181 RTP push + BYE cleanup + tracing instrumentation
- **GB28181 RTP push fully wired**: `Gb28181Output` is dynamically attached to the camera's `StreamHub` via `add_output_at_runtime` when a SIP INVITE arrives, and detached via `remove_output_from_stream` when BYE is received. The `HubHandle::remove_output` API was added to stop the output task and clean up. The `add_output_to_stream` race condition (take/replace pattern) was fixed by using `as_ref()` with a read lock.
- **OpenTelemetry span instrumentation added**: 132 `#[tracing::instrument]` annotations across all Axum HTTP handlers, StreamHub, StreamManager, capture pipeline (VideoCaptureSource/AudioCaptureSource), and protocol handlers (RTSP server, RTMP push, GB28181 SIP, ONVIF WS-Discovery/SOAP). The OTLP export pipeline now produces real trace data.

### Added — protocol hot-toggle, hot-plug, SSE, i18n, recording, and more
- **Protocol hot-toggle via Web UI**: `ProtocolRuntime` (`crates/web/src/protocol_runtime.rs`) starts/stops ONVIF, GB28181, and RTMP at runtime in response to Web UI toggles — **without server restart**. State persists across restarts via SQLite.
- **Hot-plug camera monitor**: udev netlink ADD/REMOVE listener auto-discovers plugged cameras and marks unplugged cameras offline (flushing their FileOutput gracefully).
- **SSE event bus**: `GET /api/events` (Server-Sent Events) pushes real-time camera add/offline events to the browser.
- **Snapshot endpoint**: `GET /api/cameras/{id}/snapshot` fully implemented — captures a JPEG frame via ffmpeg.
- **Browser live preview**: MJPEG multipart stream at `GET /api/cameras/{id}/live` — working end-to-end with single-slice frame fix.
- **i18n (zh-CN / en-US)**: full `t()` translation dictionary in `app.js`, language toggle persisted to user settings.
- **Day/night theme**: system-preference auto-detect on first run, manual toggle persisted.
- **Local recording**: `FileOutput` MP4 segment archive with configurable duration + capacity + auto-prune-oldest, per-stream toggle.
- **Remote log shipping**: optional `tracing-loki` layer (fail-open) via `[observability.logs]` config.
- **SQLite hybrid access**: sqlx pool (`web::db::init_pool`) for web CRUD + rusqlite (`init_auth_db`) for auth.
- **`advertised_host` config**: auto-detect LAN IP via UDP probe, no more hardcoded `localhost` in URLs.
- **W3C TraceContext propagation**: `traceparent` header extraction (incoming, Axum middleware) + injection (outbound HTTP requests).
- **esbuild build pipeline**: frontend bundled via esbuild for faster, smaller output.
- **4th migration** (`004__add_offline_since`): tracks when cameras went offline.

### Known gaps remaining
- **Windows / macOS compilation**: Do not compile yet (planned Tier 2). Blockers: `libc::getifaddrs` POSIX-only, `#[cfg(unix)]` without Windows fallback, hardcoded `/dev/videoN` paths.

### Changed — Brand rename
- **Rebranded from `notebook-cam` to `mibee-rec`** across all source, config, docs, and UI.
- Prometheus metric prefix changed: `notebook_cam_*` → `mibee_rec_*` (BREAKING for dashboards — update your Grafana queries).
- TLS dev certificate identity changed: `CN=notebook-cam` / `SAN=notebook-cam.local` → `CN=mibee-rec` / `SAN=mibee-rec.local` (delete old `tls/cert.pem` and `tls/key.pem` to regenerate).
- systemd service file renamed: `notebook-cam.service` → `mibee-rec.service`.
- Default `onvif.device_name`: `notebook-cam` → `mibee-rec`.
- RTSP Server realm and Server header: `notebook-cam` → `mibee-rec`.
- SIP User-Agent (GB28181): `notebook-cam/0.1` → `mibee-rec/0.1`.
- OpenTelemetry service name: `notebook-cam` → `mibee-rec`.

### Migration Steps
1. Update any Prometheus/Grafana queries referencing `notebook_cam_*` metrics.
2. Delete `tls/cert.pem` and `tls/key.pem` before restarting (new certs generated automatically).
3. If using systemd: `systemctl disable notebook-cam.service`, install `mibee-rec.service`, `systemctl enable --now mibee-rec.service`.
4. If using Docker: update container name and volume paths from `notebook-cam` to `mibee-rec`.
