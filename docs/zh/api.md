# API 参考文档

## 概览

mibee-eye REST API 遵循 **MiBee 摄像头设备 Web API 统一规范 v1**
（工作区内 `mibee-webui/SPEC.md`）——与树莓派摄像头项目同一契约。
除特别说明外，所有 API 走 TLS 并要求会话认证。

### 基础地址
```
https://localhost:8443
```

### 响应信封（规范 §0）

所有 JSON 端点使用统一信封：

- 成功：`{"ok": true, "data": …}`
- 失败：`{"ok": false, "error": "<机器码>", "message": "<人类可读>"}`
  并携带语义化 HTTP 状态码。机器码：`bad_request`、`unauthorized`、
  `forbidden`、`not_found`、`conflict`、`rate_limited`、
  `not_implemented`、`internal_error`。

二进制端点（快照 / MJPEG / MSE / metrics / 静态资源）与 SSE 流不套
信封。旧 `/health` 端点同样不包裹；规范路径为 `/api/health`。

### 认证（规范 §2）

Cookie 会话 + CSRF 双提交：

1. `POST /api/auth/login` `{"username","password"}` → 下发
   `session=<token>; HttpOnly; Secure; SameSite=Strict`（24 小时）与
   `csrf-token=<token>`（供 JS 读取）两个 cookie。
2. 所有写请求（POST/PUT/DELETE/PATCH）必须在 `X-CSRF-Token` 头中回传
   `csrf-token` cookie 的值（login/setup/logout 豁免）。
3. 首次启动：`GET /api/auth/me` 返回 `503 setup_required`；
   `POST /api/auth/setup` 创建管理员并直接登录。

登录按 IP 限速（20 次/分钟），连续失败后按用户名指数锁定。

## 端点

### 公开

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/health` | `{"ok":true,"data":{"status":"ok","uptime":N}}` |
| GET | `/health` | 旧版不包裹别名 |
| GET | `/metrics` | Prometheus 文本格式 |
| GET | `/`、`/style.css`、`/js/{path}` | 内嵌 Web UI（mibee-webui 共享构建） |

### 认证

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/auth/me` | `{"username","role"}` / 401 / 503 setup_required |
| POST | `/api/auth/setup` | 首启创建管理员并建立会话 |
| POST | `/api/auth/login` | 登录（限速） |
| POST | `/api/auth/logout` | 204，清除会话 |
| POST | `/api/auth/reset` | `{"old_password","new_password"}`；使所有会话失效 |

### 设备（规范 §3）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/status` | device_name / model / vendor / firmware / uptime / cameras |
| GET | `/api/capabilities` | 规范超集（`multi_camera`、`camera_management`、`camera_control`、`devices`、`mjpeg`、`mse`、`events`、`config_apply` 等）+ 端侧智能布尔位 `ai` / `zones` / `audio_ai` / `voice` / `chat` / `vlm` / `ocr` / `substream` + 主机硬件探测扩展字段 `system`、`recommended_profiles` + `events` 追加按能力门控的事件名 + `resource` 启动期功能准入快照（SPEC 附录 A #40：`{mode, total_mib, available_mib, budget_mib, reserve_mib, features:[{name, cost_mib, admitted, reason}]}`，reason 码 `off_config`/`off_budget`/`dependency`）+ `llm_tier`（full/mid/lite/manual）|

### 相机（规范 §4）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/cameras` | 相机列表（信封数组） |
| POST | `/api/cameras` | 创建：`{"name","camera_type","config"}` → 201 |
| GET/PUT/DELETE | `/api/cameras/{id}` | 相机 CRUD（部分更新） |
| POST | `/api/cameras/{id}/start` / `stop` | 启停采集（重复启动 409） |
| GET | `/api/cameras/{id}/snapshot` | JPEG 快照 |
| GET | `/api/cameras/{id}/live` | MJPEG 多部分流 |
| GET | `/api/cameras/{id}/stream.mse` | MSE 播放的 chunked fMP4 流 |
| GET | `/api/cameras/{id}/stream.sub.mse` | 低分辨率子码流（每相机 `config.substream`；经 `capabilities.substream` 通告） |
| GET | `/api/cameras/{id}/zones` | 已存区域：`{"zones":[{name, kind:"intrusion"\|"line_cross", points:[[x,y]…], dwell_secs}], "applied":"immediate"}`（能力位 `zones`） |
| PUT | `/api/cameras/{id}/zones` | 整体替换区域列表——body 为**裸数组**；结构校验（入侵 ≥ 3 点、越线恰 2 点） |

### 配置（规范 §5）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/config` | `{"settings": {…点键嵌套…}, "protocols": {"onvif": …, "gb28181": …, "rtmp": …, "recording": …, "webrtc": …}}` |
| PUT | `/api/config` | 上文档的部分深合并；协议节立即热切换 ONVIF/GB28181（`config_apply.default = "immediate"`） |

取代旧的 `GET/PUT /api/settings` 与 `/api/protocols/{name}` GET/PUT 端点。

### 事件（规范 §6）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/events` | SSE 流（`text/event-stream`，15 秒 keepalive）。事件：`camera_added`、`camera_offlined`、`ai_detection`、`ai_model_changed`、`alarm`（SPEC §6：`camera_id`、`active: true`、`source: "ai"` 或 `"audio"`、`targets` / `class` + `score`、`timestamp` 毫秒时间戳）、`zone_event`（`{camera_id, zone, event, track_id, label, timestamp}`）、`voice_transcript`（`{keyword, transcript, speaker, timestamp}`——`speaker` 为最优匹配的已注册声纹，未知为空串）、`chat_reply`（`{source, reply, timestamp}`）、`voice_decision`（`{camera_id, transcript, choice, confidence, act_probability, timestamp}`——语音转写的意图决策）、`alarm_description`（`{camera_id, alarm_timestamp, description, elapsed_s}`）、`meeting_state`（`{camera_id:"all", meeting_id, status:"recording"\|"processing"\|"done"\|"failed", timestamp}`——会议生命周期，SPEC 附录 A #27）、`conversation`（`{id, conversation_id, origin, user_text, thinking[], reply_text, engine}`——一轮对话完成，SPEC §3.4；`thinking` 可含 `source:"tool"` 工具条目）、`agent_step`（`{conversation_id, kind:"tool"\|"phase", state, tool?, args?, result?, duration_ms?}`——agent 工具调用实时过程与阶段切换，SPEC §3.5）。按能力门控的事件只在对应引擎活跃时送出。 |

### 端侧智能（设备扩展）

以下端点全部按能力位门控：引擎未激活时如实作答（`{"enabled": false}` 形态
/ `not_implemented` 类错误），绝不假装成功。

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/chat` | 接地对话：`{"text","history":[{role,content}],"vision"?}` → `{"reply","engine","grounded"}`（能力位 `chat`；语音环路的回复另经 `chat_reply` SSE 送出）。每轮注入实时场景接地（【画面】检测标签计数 + 最近 VLM 描述、【本机】时钟/开机时长/内存、可选【联网】天气——附录 A #29）；`vision:true` 以新鲜帧走 VLM 看图直答（`grounded:"vlm"`，CPU 上较慢），失败回落接地 LLM。配置了云端时云优先、失败回落本地，`engine` ∈ `cloud\|local\|vlm` 披露实际路径（#35）。注册了工具（§3.5）时文本轮走 agent 工具循环，响应加法携带 `tool_calls: [{name, args, ok, result, duration_ms}]` 数组（#43） |
| POST | `/api/ocr` | body = JPEG 原始字节 → `{"items":[{text, score, bbox}]}`（能力位 `ocr`） |
| GET | `/api/audio/records` | 听觉记录（能力位 `audio_records`）：`{"records":[{id, kind:"sound"\|"voice", text, score, keyword, speaker, timestamp_ms}]}`，最新在前；`?limit=N`（缺省 100、上限 500）、`?kind=sound\|voice` 过滤 |
| DELETE | `/api/audio/records` | 清空全部记录 → `{"applied":"immediate","removed":N}` |
| GET | `/api/voice/speakers` | 声纹档案列表（能力位 `voice_speakers`，**无副作用**）→ `{"speakers":[{id,name,dim,count,created_at}], "enrollment":{name,collected,needed}\|null, "capable":bool}` |
| POST | `/api/voice/speakers` | 开始注册：体 `{"name", "utterances"?（缺省 3，1..=10）}`——接下来 `utterances` 次唤醒词各采一条嵌入样本，进度经 GET 轮询；已注册名 400 |
| POST | `/api/voice/speakers/commit` | 样本集满后持久化并入内存 → `{"enrolled","samples","dim"}`；未集满 400 |
| POST | `/api/voice/speakers/cancel` | 放弃进行中的注册会话 |
| DELETE | `/api/voice/speakers/{name}` | 删除声纹档案（内存+数据库；不存在 404） |
| POST | `/api/meetings/start` | 开始会议录音（能力位 `meeting`）→ `201 {"id","started_at_ms"}`；已在录 409；引擎未激活 501。录音中 SSE 发 `meeting_state` `{camera_id:"all", meeting_id, status:"recording", timestamp}` |
| POST | `/api/meetings/{id}/stop` | 停止并触发**异步**处理管线（分离→转写→标点→打名→入库）→ `{"id","status":"processing"}`；id 非当前会话 409；完成经 `meeting_state` SSE（`done`/`failed`）通知 |
| GET | `/api/meetings` | 会议列表（倒序）→ `{"meetings":[{id, started_at_ms, ended_at_ms, duration_ms, status:"recording"\|"processing"\|"done"\|"failed", num_speakers, num_segments, audio_path, error}]}` |
| GET | `/api/meetings/{id}` | 会议详情 → `{"meeting":{…}, "segments":[{start_ms, end_ms, speaker_index, speaker, text}]}`（按 start_ms 升序；`speaker` 为声纹档案命中名，未命中空串——前端以 `speaker_index` 渲染"说话人 N"） |
| DELETE | `/api/meetings/{id}` | 删除会议记录+分段（及保留的音频文件）；不存在 404 |
| GET/PUT | `/api/cameras/{id}/zones` | 见上方相机表 |
| GET | `/api/ai/models` | 检测模型库：可用/激活模型列表 |
| POST | `/api/ai/models/{id}/activate` | 激活已上传模型 |
| POST | `/api/ai/models` | 上传模型包 |
| DELETE | `/api/ai/models/{id}` | 删除已上传模型 |

### 对话调用链（规范 §3.3）

对话级模型调用链记录：一次对话应答路径上调用的每个模型（决策分流、VLM、云端 LLM、本地 LLM、TTS）各产生一个 span，含调用顺序、时长、进程 CPU 增量与 token 数。配置了 `otel_endpoint` 时同一棵链路经 OTLP 导出。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/traces/conversations?limit=` | 最近对话摘要列表（缺省 50 上限 200，倒序） |
| GET | `/api/traces/conversations/{id}` | 单条对话全量 span；未知 id → 404 |

列表项：`{"id","origin":"chat"|"voice","started_at_ms","duration_ms","turns","models":[...],"status":"ok"|"partial"|"error","open"}`；span：`{"span_id","parent_id","model","variant","label","start_ms","duration_ms","cpu_ms","status","tokens_prompt","tokens_completion","attributes"}`。

配套 `/metrics`（公开，Prometheus 文本）暴露每模型资源族：`mibee_model_inferences_total{model,variant}`、`mibee_model_inference_seconds`/`mibee_model_cpu_seconds` 直方图、`mibee_model_inflight{model}`、`mibee_model_errors_total`、`mibee_model_tokens_total{...,kind=prompt|completion}`，及系统/进程资源 gauge（`mibee_eye_system_*`/`mibee_eye_process_*`）。对话环容量 200 对话 × 每对话 64 span。

### 对话记录（规范 §3.4）

人读的对话逐轮记录——用户说了什么（HTTP 提问或语音转写原文）、设备"想了什么"（每次内部模型调用/路由决策一条摘要条目，含失败回落腿）、AI 答了什么、用的哪个引擎。语音交互发生在浏览器之外，这里是其可见载体；无回复轮（决策判 ignore）以 `reply_text: null` 诚实落库。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/conversations?limit=` | 最近对话轮（缺省 50 上限 200，倒序） |

轮对象：`{"id","conversation_id","origin":"voice"|"http","started_ms","user_text","thinking":[{"source","model","note","duration_ms"}],"reply_text","engine":"cloud"|"local"|"vlm"|null}`。
每轮完成同时经 SSE `conversation` 事件推送（`conversations` 能力门控）。SQLite 持久、FIFO 封顶 1000 轮；`[conversations] enabled = false` 整体关闭记录（隐私开关，能力亦不通告）。

### 对话 Agent 工具与技能（规范 §3.5）

对话助手的工具注册表——内置设备能力 + 部署者注册的 MCP
（Model Context Protocol 2025-06-18）stdio 子进程服务器。本清单即
"暴露给模型的工具"透明面；能力键 `tools`（`{enabled, count}`）。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/tools` | 工具清单 → `{"tools":[{name, description, input_schema, source}]}`；`source` = `"builtin"` 或 `"mcp:<服务器名>"` |

内置工具：`time.now`、`weather.current`（需 `[tools] weather_city`）、
`camera.snapshot`（最新画面 + 查看地址）。实时执行经 SSE `agent_step`
事件送出，并落进对话记录 `thinking` 的 `source:"tool"` 条目。

### 离家模式（规范 §3.6）

主人不在家时的值守：设备持续分析既有 AI 检测流并把异常逐条落档——
发现人员时经语音问候并询问来人是谁（已登记人脸按名问候），回答与
现场快照、VLM 画面描述一并记入同一事件。能力键 `away`
（`{available, voice}`）；布防要求 AI 检测可用。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/away` | 布防状态 → `{active, since_ms, voice, stats}` |
| POST | `/api/away` | 布防/撤防 `{"active":bool}`（CSRF）；不可布防时 400 附原因 |
| GET | `/api/away/events?limit=` | 事件记录倒序（缺省 50 上限 200） |
| DELETE | `/api/away/events` | 清空全部记录**连同快照文件**（CSRF） |
| GET | `/api/away/events/{id}/snapshot` | 单条事件的现场快照 JPEG（无则 404） |

事件对象：
`{"id","camera_id","kind":"person"|"activity","started_ms","labels","face_name"?,"description"?,"visitor_reply"?,"snapshot"?,"state"}`。
人员事件状态：`greeting → listening → answered|silent`（威慑开启时
`listening → reask → silent`），旁路终态 `known` / `no_voice`；活动类为
`recorded`。撤防响应带加法字段 `summary`（有事件且摘要开关开启时）。创建与每次状态迁移经 SSE
`away_event` 推送（按 `id` 就地更新）；布防/撤防广播 `away_state`。
布防状态服务重启后保持。记录 SQLite 持久、FIFO 封顶 1000 条；修剪与
清空同步删除快照文件。配置见 `[away]`（节拍/间隔/冷却/问候语/听窗）；
六项运行时能力（语音布防/撤防、撤防摘要、威慑二问、定时布防、访客
推送、声音联动）经 `PUT /api/config` 的 `scene.away.*` 热配置——见
`docs/zh/configuration.md`。

### 主机设备（规范 §4.8）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/devices/video` | V4L2 视频设备枚举 |
| GET | `/api/devices/video/{index}/formats` | 支持的采集格式 |
| GET | `/api/devices/audio` | ALSA 音频设备枚举 |

### 协议运行态（设备扩展）

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/protocols/runtime-status` | `{"onvif":{"running":b},"gb28181":…,"rtmp":…}` |

## Web UI

内嵌前端是共享的 **mibee-webui** 构建（ES Modules、零构建步骤）——
与树莓派摄像头项目同一套界面，按能力通告渲染。唯一真源：工作区
`mibee-webui/`（`make sync-notebook` 拷入）。
