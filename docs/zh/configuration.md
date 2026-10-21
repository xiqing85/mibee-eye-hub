# 配置参考

本文档提供了 mibee-eye 配置系统的完整参考。

## 概述

配置文件控制 mibee-eye 行为的所有方面。配置系统支持层次化优先级，允许为开发、测试和生产环境设置不同的配置。

### 配置文件位置

1. **默认配置**：`config.toml`（项目根目录）
2. **本地覆盖**：`config.local.toml`（项目根目录）
3. **CLI 覆盖**：`--config <path>` 命令行参数

### 优先级规则

配置值按优先级顺序加载（优先级高的获胜）：

1. 命令行 `--config <path>`（绝对最高优先级）
2. `config.local.toml`（gitignored，用于本地开发）
3. `config.toml`（默认配置）

当文件不存在时，系统会回退到编译时的默认值。配置文件中未指定的个别字段和部分将使用其默认值。

## 配置参考

### [web] - Web UI 服务器

配置承载用户界面和 REST API 的 HTTPS Web 服务器。

```toml
[web]
port = 8443
host = "0.0.0.0"
advertised_host = "192.168.1.100"
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `port` | u16 | `8443` | Web UI 和 REST API 的 HTTPS 端口（必须 > 1024） |
| `host` | String | `"0.0.0.0"` | 绑定地址：`"0.0.0.0"`（所有接口）或 `"127.0.0.1"`（仅本地主机） |
| `advertised_host` | Option<String> | `None` | 返回给客户端的 URL 的通告主机名/IP。如果为 None，则在启动时通过 UDP 探测自动检测。消除外部 URL 中的硬编码 localhost。 |
| `http_port` | u16 | `0` | 可选的附加纯 HTTP 监听口，局域网免 TLS 访问（SPEC 附录 A）。0 = 关闭。该监听口签发的会话 cookie 不带 `Secure` 标记；TLS 口的照常携带。 |

**注意事项：**

- 所有 Web 流量都通过 rustls 使用 TLS（HTTPS）
- 首次运行时在 `tls/cert.pem` + `tls/key.pem` 自动生成自签名 TLS 证书
- 证书支持文件 mtime 更改时的热重载
- 生产环境应提供 CA 签名的证书
- 默认端口避免特权范围（< 1024）

### [rtsp] - RTSP 服务器

配置用于摄像头流和客户端连接的 RTSP 流媒体服务器。

```toml
[rtsp]
server_port = 8554
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `server_port` | u16 | `8554` | RTSP 服务器监听端口 |

**注意事项：**

- RTSP 服务器以服务器模式运行（客户端从该机器拉取流）
- RTSP 协议支持 RFC 2326，具有 Digest 身份验证和 RTP 交错
- 默认端口避免特权范围；无需特权绑定
- 支持带有正确 SPS/PPS 头的 H.264 流媒体
- 外部客户端（NVR、VLC）连接到 `rtsp://this-host:8554/stream` 以拉取流

### [rtmp_push] - RTMP 推送客户端

配置用于将流推送到外部推流服务器（NVR、直播平台）的 RTMP 推送客户端。

**所有 RTMP 操作均为出站推送 — 此机器推送到外部端点。不存在 RTMP 接收服务器。**

```toml
[rtmp_push]
enabled = false
push_url = "rtmp://192.168.1.100:1935/live"
app_name = "live"
stream_name = "stream1"
reconnect_interval_secs = 5
max_reconnect_attempts = 10
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主启用开关（所有出站协议默认关闭） |
| `push_url` | String | `"rtmp://192.168.1.100:1935/live"` | 外部 RTMP 推流端点 URL |
| `app_name` | String | `"live"` | RTMP 应用名称 |
| `stream_name` | String | `"stream1"` | RTMP 流密钥/名称 |
| `reconnect_interval_secs` | u64 | `5` | 重连尝试间隔（秒）（如果启用，必须 > 0） |
| `max_reconnect_attempts` | u32 | `10` | 最大重连尝试次数（如果启用，必须 > 0） |

**注意事项：**

- 这只是出站推送 — 此机器推送到外部 RTMP 推流服务器
- 不存在 RTMP 接收服务器；外部客户端无法通过 RTMP 推送到此机器
- 手写的 RTMP 实现，支持增强的时间戳
- 连接丢失时自动重连，具有指数退避
- 可通过 Web UI 进行协议热切换，无需重启服务器

### [capture] - 本地捕获设备

配置本地网络摄像头和麦克风捕获。

```toml
[capture]
video_device = "/dev/video0"
audio_device = "default"
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `video_device` | String | `"/dev/video0"` | 视频捕获设备路径 |
| `audio_device` | String | `"default"` | 音频捕获设备标识符 |

**注意事项：**

- **Linux**：视频设备通常为 `/dev/video0`、`/dev/video1` 等
- **Linux**：音频设备 `"default"` 使用 ALSA 默认设备
- **Windows**：视频设备使用 MSMF（Media Foundation）设备名称
- **Windows**：音频设备使用 WASAPI 设备名称
- 视频捕获需要 `libv4l-dev` 和用户在 `video` 组中
- 音频捕获以高优先级运行；切勿在回调中阻塞
- 本产品是仅本地捕获 — 它不会发现或从远程网络摄像头拉取

### [security] - 身份验证和速率限制

配置身份验证和 API 保护的安全策略。

```toml
[security]
rate_limit_max = 20
rate_limit_window_secs = 60
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `rate_limit_max` | usize | `20` | 每个速率限制窗口的最大请求数（必须 > 0） |
| `rate_limit_window_secs` | u64 | `60` | 速率限制时间窗口（秒） |

**注意事项：**

- 使用 `parking_lot::Mutex` 为身份验证端点实现滑动窗口速率限制
- 防止对登录系统的暴力破解攻击
- 速率限制基于每 IP 地址进行
- 5 次失败后登录失败锁定，具有指数退避
- Mutex 不可中毒（对请求处理程序安全）

### [resources] - 启动期功能资源门控

按主机资源自适应启动哪些 AI 功能（SPEC 附录 A #40）。`auto`（缺省）以启动时的
内存预算（MemAvailable − reserve_mib）贪心准入已启用的功能——每项成本按其模型
文件的实际大小估算——小内存机器自动裁掉重模型尾部（VLM/LLM），避免抖动换页。
`all` 无视内存全量启动。`auto_tier` 另按可用内存切换 LLM 模型文件
（full/mid/lite）。

```toml
[resources]
feature_gate = "auto"   # auto | all
reserve_mib = 512       # 预算外保留的余量
auto_tier = false       # 按可用内存选 LLM 档位模型
```

准入表经 `capabilities.resource` 暴露（状态页"资源档位"卡），并以
`mibee_eye_feature_admitted` / `mibee_eye_resource_budget_mib` Prometheus
gauge 输出。重启时按届时水位重算。

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `tier` | String | `"auto"` | 具名硬件档位（`full` / `balanced` / `lite` / `auto` = 按检测到的硬件推导） |
| `auto_tier` | bool | `false` | 单键简写：按可用内存选 LLM 档位模型（full/mid/lite，对应 `[llm]` 的模型路径） |
| `feature_gate` | String | `"auto"` | `"auto"` 按启动内存预算贪心准入；`"all"` 无视内存全量启动 |
| `reserve_mib` | u64 | `512` | 从准入预算中保留的余量（MiB；MemAvailable 本已含可回收缓存） |

### [elastic] - 运行期硬件弹性控制器

30s 采样（MemAvailable + 电池供电）驱动 LLM 档位决策（SPEC 附录 A #48，#3 的
系统性解法）：低水位持续 → 档位降一级，**运行中热换模型**（无需重启；换芯失败
回落为仅持久化、下次启动生效）；恢复水位持续 → 升一级；电池供电立即钉 lite；
临界水位只建议切换 minimal 形态（绝不自动重启）。每次决策发 SPEC §6 `scale`
事件（状态页"资源档位"卡实时显示）。阈值带可按主机调：

```toml
[elastic]
enabled = true        # 总开关（false = 不采样、不发 scale 事件）
enter_low_mib = 1024  # 可用内存持续低于此值 → 降档
exit_low_mib = 2560   # 持续高于此值 → 允许升档（滞回防抖动）
critical_mib = 320    # 持续低于此值 → 建议 minimal 形态（仅告警）
cooldown_secs = 180   # 两个动作之间的最小间隔
sustain_samples = 3   # 进带持续采样数（去抖）
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `true` | 控制器总开关 |
| `enter_low_mib` | u64 | `1024` | 降档进入阈值（MiB） |
| `exit_low_mib` | u64 | `2560` | 升档恢复阈值（MiB，滞回） |
| `critical_mib` | u64 | `320` | minimal 形态建议阈值（MiB） |
| `cooldown_secs` | u64 | `180` | 动作冷却（秒） |
| `sustain_samples` | u32 | `3` | 进带去抖采样数（最小 1） |

### [conversations] - 对话记录（规范 §3.4）

人读对话日志总开关：每轮对话（语音或网页）落 SQLite——听到/输入文本、内部"思考"条目（决策/云端/VLM/本地 LLM/TTS，含失败回落腿）与 AI 回复及引擎——经 `GET /api/conversations` 查询、SSE `conversation` 事件实时推送（助手页"对话记录"卡）。FIFO 封顶 1000 轮；写失败绝不影响对话管线。`enabled = false` 不记录且隐藏卡片（隐私开关）。

```toml
[conversations]
enabled = true
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `true` | 对话记录总开关；`false` 不记录且隐藏对话记录卡（隐私开关） |

### [agent] - 对话 Agent 工具与技能（SPEC §3.5 / 附录 A #43）

对话助手的工具调用循环。内置工具恒注册：`time.now`（本地时间）、
`weather.current`（复用 `[tools]` 天气配置——需设置 `weather_city`）、
`camera.snapshot`（最新画面 + 查看地址）。**自定义插件**以 MCP
（Model Context Protocol，spec 2025-06-18）stdio 子进程服务器接入：任意
MCP 服务器可执行文件皆可——设备 spawn 它、经换行分帧 JSON-RPC 握手、
列出其工具、在对话中按需调用（问天气 → 模型调 `weather.current` → 结果
回填 → 回复引用真实数据）。工具结果进入模型上下文前截断 8 KiB；工具
失败如实回填（模型据实告知，绝不编造）。注册表为空（无任何工具）时对话
退化为普通路径，行为零变化。云端走原生 `tools`/`tool_calls` API；本地
Qwen3 走其原生 `<tool_call>` 提示格式。`GET /api/tools` 列出暴露给模型的
全部工具（透明性清单）。信任边界：MCP 服务器是部署者显式配置的本地子
进程——不自动安装、不远程发现；后续版本可为敏感操作增加逐工具确认门。

```toml
[agent]
enabled = true
max_steps = 3             # 工具调用轮数上限，超限强制文字收口
step_timeout_ms = 15000   # 单次工具执行超时

# 自定义工具插件示例（任意 MCP stdio 服务器）：
[[agent.mcp_servers]]
name = "home"
command = "python3"
args = ["/home/you/.config/mibee-eye/mcp/home_tools.py"]
# 语音轮仅在话术命中工具意图（天气/时间/画面/设备控制）时才携带工具表——
# 工具提示词 prefill 很重，闲聊不该为它多等 ~20 秒。
voice_tool_gate = true
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `true` | 总开关。注册表为空时开=true 也是 no-op——循环至少需要一个工具才介入 |
| `max_steps` | u32 | `3` | 工具调用轮数上限；最后一轮去掉工具表，模型必须给文字答案 |
| `step_timeout_ms` | u64 | `15_000` | 单次工具执行超时（毫秒） |
| `voice_tool_gate` | bool | `true` | 语音轮仅在话术命中工具意图（天气/时间/画面/设备控制）时携带工具表；HTTP 恒全量 |
| `mcp_servers` | 表数组 | `[]` | MCP stdio 服务器（`[[agent.mcp_servers]]`）：每项 `name`、`command`、`args`、`env` |

### [away] - 离家模式值守（规范 §3.6 / 附录 A #44）

主人不在家时的值守。搭在既有 AI 检测流上（不另建采样管线）：
人员到达（离开宽限 + 问候冷却门控）触发记录——存现场快照、比对
已登记人脸（已登记者按名问候、不询问；陌生人问候 + 询问身份并开
免唤醒监听窗，转写记入记录）、VLM 异步描述画面、桌面通知。配置的
活动标签（缺省宠物类）在无人在场时按冷却逐条记录。布防状态跨服务
重启保持（settings 袋 `away.active`）。语音腿需要 `[tts]` 与
`[voice]` 可用——不可用时人员事件照记（`state:"no_voice"`）。

```toml
[away]
interval_ms = 1000           # 每相机离家分析最小间隔（叠加在 [ai] interval_ms 上）
person_gap_secs = 10         # 离开宽限：窗内再检出视为同一次在场
greeting_cooldown_secs = 120 # 两次访客交互最小间隔
activity_cooldown_secs = 60  # 两条活动记录最小间隔
activity_labels = ["cat", "dog", "bird"]  # 非人员异常标签
listen_secs = 10             # 问候后的一次性免唤醒监听窗
snapshot_dir = "away-snapshots"           # 证据快照目录（cwd 相对）
greeting_known = "欢迎回家，{name}。"     # {name} = 已登记人脸名
greeting_unknown = "你好，这里是主人的智能看家助手。主人现在不在家，请问你是谁？"
```

**字段参考**（全部**可 Web 热配置**：`PUT /api/config` 的 `scene.away.*`，
即时生效；上方 `[away]` TOML 仅播种启动默认值）：

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `interval_ms` | u64 | `1000` | 事件节流下限，叠加在 `[ai]` 采样间隔上（不另建管线） |
| `person_gap_secs` | u64 | `10` | 人员消失宽限秒数，超过即视为访客离开 |
| `greeting_cooldown_secs` | u64 | `120` | 两次问候交互的最小间隔 |
| `activity_cooldown_secs` | u64 | `60` | 同类活动记录的最小间隔 |
| `activity_labels` | Vec<String> | `["cat","dog","bird"]` | 计入"活动"记录的检测类别 |
| `listen_secs` | u64 | `10` | 问候后的一次性免唤醒监听窗 |
| `snapshot_dir` | String | `"away-snapshots"` | 访客快照 JPEG 目录（相对工作目录）；修剪/清空同步删文件 |
| `greeting_known` | String | `"欢迎回家，{name}。"` | 已登记人脸的问候（`{name}` = 人名） |
| `greeting_unknown` | String | `"你好，这里是主人的智能看家助手。主人现在不在家，请问你是谁？"` | 陌生访客的问候+询问来意 |
| `voice_control` | bool | `true` | "我要出门了/我回来了"语音布防/撤防（内建意图，先于 LLM） |
| `summary_on_disarm` | bool | `true` | 撤防生成摘要（本地 LLM 一句话，回落纯统计文案）；API 响应携带、语音撤防时播报 |
| `deterrence` | bool | `false` | 陌生人未应答先二次追问（`reask_text`、半窗），再播 `deterrence_text` |
| `deterrence_text` | String | `"请注意：您的影像和声音已被记录，主人将收到通知。"` | 对沉默陌生人的威慑话术 |
| `reask_text` | String | `"请问你是谁？请说明来意。"` | 问候未应答时的二次追问话术 |
| `schedule_arm` | String | `""` | `HH:MM` 本地时间每日布防沿，空=关闭。窗口内手动状态不被对抗 |
| `schedule_disarm` | String | `""` | `HH:MM` 本地时间每日撤防沿，空=关闭 |
| `schedule_weekdays_only` | bool | `false` | 定时沿仅工作日触发 |
| `push_url` | String | `""` | 访客推送 webhook：每访客事件一次 JSON POST（纯文本、永不含图像），5s 超时 fail-open。**空值保持完全离线**——配置 URL 即知情选择外发文本 |
| `audio_link` | bool | `true` | 布防期间投票声音上升沿也落离家活动记录（labels `🔊 类别 置信度`，按活动冷却） |

### [desktop] - 托盘图标与桌面通知（规范 附录 A #42）

带桌面会话主机的本机存在感。全部 fail-open：无会话总线（`DBUS_SESSION_BUS_ADDRESS` 或 `$XDG_RUNTIME_DIR/bus`）整体跳过、仅记一行日志——headless 服务器零影响。托盘显示相机图标，左键（或菜单项）经 `xdg-open` 打开 Web 界面（优先 `web.http_port`，否则 TLS 端口）。每次告警上升沿（视觉/声音/区域）发桌面通知；首次发送失败（无通知守护）即在本轮运行内停用。

```toml
[desktop]
tray = true                # StatusNotifierItem 托盘图标
notifications = true       # 告警上升沿桌面通知
notify_conversations = false  # 语音回复完成时也通知
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `tray` | bool | `true` | StatusNotifierItem 托盘图标（点击打开 Web 界面） |
| `notifications` | bool | `true` | 告警上升沿桌面通知 |
| `notify_conversations` | bool | `false` | 语音回复播报完成时也通知 |


### [observability] - 监控和日志记录

配置 OpenTelemetry 跟踪、应用程序日志记录和可选的远程日志推送。

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

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `otel_endpoint` | String | `"http://localhost:4317"` | OpenTelemetry 收集器端点（OTLP gRPC）。设置后导出请求、协议会话与**每次模型调用**的 span（`model_call/<id>`，携带 `model`/`variant`；每次对话另有 `conversation/<origin>` 根 span 及其模型链） |
| `log_level` | String | `"info"` | 日志级别过滤器 |
| `logs` | Option<RemoteLogConfig> | `None` | 可选的远程日志推送配置 |

**[observability.logs] RemoteLogConfig 字段：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `endpoint` | String | `""` | 用于远程日志推送的兼容 Loki 的 HTTP 端点 URL |
| `batch_size` | usize | `100` | 每次刷新批处理的日志条目数 |
| `flush_interval_secs` | u64 | `5` | 刷新间隔（秒） |
| `labels` | HashMap<String, String> | `{}` | 附加到每个日志流的额外标签 |

**日志级别选项：**

- `"trace"` - 最详细的日志记录，调试信息
- `"debug"` - 调试信息，函数调用
- `"info"` - 一般操作信息（默认）
- `"warn"` - 不停止操作的警告条件
- `"error"` - 可能影响操作的错误条件

**注意事项：**

- OpenTelemetry 集成是可选的 — 没有收集器时系统也能工作
- OTLP（OpenTelemetry 协议）通过 gRPC 在端口 4317 上传输
- 当 OpenTelemetry 不可用时使用结构化 JSON 日志记录
- 远程日志推送到 Loki 是可选的并且是失败开放：不可达的端点记录警告，应用程序继续
- 在传入请求上提取 W3C TraceContext `traceparent` 标头，在传出请求上注入
- 可用于 Prometheus 抓取的 `/metrics` 端点（无身份验证，生产环境中的防火墙）

### [onvif] - ONVIF 设备端点

配置用于通过 WS-Discovery 进行外部 NVR 发现的 ONVIF 设备端点。

**此机器充当 ONVIF 摄像头 — 外部 NVR 发现它。这不是 ONVIF 客户端。**

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

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主启用开关（所有出站协议默认关闭） |
| `device_name` | String | `"mibee-eye"` | ONVIF 设备名称 |
| `manufacturer` | String | `"MiBee"` | 制造商名称 |
| `model` | String | `"Rec-01"` | 设备型号 |
| `serial` | String | `"NC00000001"` | 序列号 |
| `firmware_version` | String | `"1.0.0"` | 固件版本 |
| `port` | u16 | `3702` | WS-Discovery UDP 端口（硬编码） |
| `events_enabled` | bool | `true` | Pull-Point 事件服务：AI 运动告警以 `tns1:VideoSource/MotionAlarm` 推送给持有订阅的 NVR |
| `media2_enabled` | bool | `true` | 在经典 Media 之外提供 ONVIF Media2（Profile T）端点 |
| `http_digest` | bool | `false` | SOAP 监听的 HTTP Digest 认证（缺省 WS-UsernameToken） |
| `ip_filter` | Vec<String> | `[]` | SOAP 监听 IPv4 白名单（`a.b.c.d[/prefix]`）；空=不过滤，非空时表外对端在任何 SOAP 处理前得 403。重启生效 |

**注意事项：**

- 出站协议 — 此设备广播 WS-Discovery 消息，外部 NVR 发现它
- 手写的 WS-Discovery 服务器（701 LOC）
- 可通过 Web UI 进行协议热切换，无需重启服务器

### [gb28181] - GB/T 28181 设备注册

配置与中国监控平台的 GB/T 28181 设备注册。

**此设备通过 SIP REGISTER 向平台注册；平台发送 INVITE，此设备推送 RTP。**

```toml
[gb28181]
enabled = false
platform_sip_address = "192.168.1.100"
platform_sip_port = 5060
device_id = "34020000002000000001"
username = ""
password = ""
sip_domain = "3402000000"
register_interval_secs = 60
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主启用开关（所有出站协议默认关闭） |
| `platform_sip_address` | String | `"192.168.1.100"` | SIP 平台地址 |
| `platform_sip_port` | u16 | `5060` | SIP 平台端口（如果启用，必须 > 1024） |
| `device_id` | String | `"34020000002000000001"` | 20 字符 GB28181 设备 ID |
| `username` | String | `""` | SIP 身份验证用户名 |
| `password` | String | `""` | SIP 身份验证密码 |
| `sip_domain` | String | `"3402000000"` | SIP 域 |
| `register_interval_secs` | u64 | `60` | SIP REGISTER 间隔（秒）（如果启用，必须 > 0） |
| `channel_id` | String | `"34020000001320000001"` | 20 字符 GB28181 通道 ID |
| `local_sip_port` | u16 | `5060` | 本地 SIP 监听端口 |
| `heartbeat_interval_secs` | u64 | `60` | 心跳间隔（秒） |
| `heartbeat_timeout_count` | u32 | `3` | 判定重连前允许丢失的心跳次数 |
| `talkback_playback` | bool | `true` | 在本机输出设备播放平台语音对讲（不可用时 fail-open 488） |
| `ptz_onvif_endpoint` | String | `""` | 外接云台 ONVIF 端点（M4-a PTZ 代理）：`http://host:port/onvif/device_service`；空 = 无云台，PTZ 指令保持 no-op 应答 |
| `ptz_profile_token` | String | `"Profile_1"` | PTZ 命令面向的媒体 profile token |
| `ptz_username` | String | `""` | ONVIF WS-UsernameToken 认证用户名（空 = 匿名） |
| `ptz_password` | String | `""` | ONVIF 认证密码 |
| `alarm_notify_enabled` | bool | `true` | AI 告警 NOTIFY 初始门控（平台 DeviceConfig AlarmReport 可运行时覆盖） |
| `alarm_cooldown_secs` | u64 | `30` | 告警上升沿冷却（秒）（SPEC §6 `alarm` + NOTIFY） |
| `position_longitude` | String | `""` | 静态 MobilePosition 经度（空 = 不上报位置） |
| `position_latitude` | String | `""` | 静态 MobilePosition 纬度（空 = 不上报位置） |

**注意事项：**

- 出站协议 — 此设备向平台注册，而不是平台角色
- GB28181 信令由 `gb28181-rs` 库提供，RTP/PS over UDP 推流
- AI 检测上升沿触发 SPEC §6 `alarm` SSE 事件，并在平台已订阅且门控允许时发送 Alarm NOTIFY——优先级 4、方法 5、类型 2（2022 标准表）
- SIGTERM / 协议停止时以 REGISTER `Expires: 0` 注销（失败仅记录日志并忽略）
- 可通过 Web UI 进行协议热切换，无需重启服务器

### [recording] - 本地录制

配置本地 MP4 段录制到磁盘。

```toml
[recording]
enabled = false
path = "./recordings"
segment_duration_secs = 900
max_capacity_mb = 10240
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主启用开关。单个流可以通过 Web UI 选择退出。 |
| `path` | String | `"./recordings"` | 写入 MP4 段文件的目录（必须可写） |
| `segment_duration_secs` | u64 | `900` | MP4 段持续时间（秒）（必须 > 0）。默认：15 分钟。 |
| `max_capacity_mb` | u64 | `10240` | 最大总容量（MB）。0 = 无限制（无修剪）。默认：10 GB。 |

**注意事项：**

- 捕获的 H.264 帧被复用到滚动 MP4 段中
- 当总大小超过 `max_capacity_mb` 时，最旧的段自动修剪
- 单个流可以通过 Web UI 启用/禁用录制

### 端侧智能配置节

以下各节配置可选的 AI 引擎，共用同一套契约：

- **全部默认 `enabled = false`**——麦克风与常驻分析属于隐私敏感输入，
  每个引擎都是主动开启（opt-in）。
- **故障开放（fail-open）**：模型文件缺失、构建未编入对应 cargo feature
  或主机性能不足时，引擎只是保持关闭——能力位从 `/api/capabilities` 与
  界面消失，绝不影响启动或产品其余部分。
- **启动态**：这些节只在启动时读取一次，修改后需重启服务（与可热切换的
  协议节不同）。
- 模型文件是不进 git 的部署期下载——来源、体积与许可见
  [`models/README.md`](../../models/README.md)。

### [watermark] - 视频水印叠加

在编码流上烙入文本/时间戳叠加（SPEC v1 §5.2）。此处仅为启动引导默认——
运行时设置存 DB（`protocols.watermark`），在 Web UI 编辑。

```toml
[watermark]
enabled = false
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 总开关 |
| `text` | String | `""` | 自定义文本（≤128 字符）；CJK 需带 CJK 字形的 `font_path` |
| `show_timestamp` | bool | `true` | 烙入时间戳行 |
| `timestamp_format` | String | `"%Y-%m-%d %H:%M:%S"` | strftime 子集：`%Y %m %d %H %M %S %F %T %%` + 字面量 |
| `position` | String | `top_left` | `top_left` / `top_right` / `bottom_left` / `bottom_right` 之一 |
| `font_size` | u32 | `24` | 像素高，12..96 |
| `font_path` | String | `""` | 可选 TTF/OTF（CJK 必需）；空 = 内嵌 ASCII 子集字体 |

### [webrtc] - WebRTC 播放（实验性）

Whep 风格 WebRTC 播放端点。ICE candidate 从一个小 UDP 端口段收集；
STUN 服务器支撑非局域网拓扑（空 = 仅 host candidates）。

```toml
[webrtc]
enabled = false
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 总开关 |
| `udp_port_min` | u16 | `0` | ICE host candidate 的 UDP 端口段起点（0 = 系统自选） |
| `udp_port_max` | u16 | `0` | UDP 端口段终点（含；0 = 系统自选） |
| `stun_servers` | String | `""` | 逗号分隔的 STUN URL（如 `stun:stun.l.google.com:19302`）；空 = 仅局域网 host candidates |

### [ai] - 目标检测（NanoDet-Plus）

视觉目标检测，喂给告警、区域与离家看家。默认关——与出站协议同为
opt-in。

```toml
[ai]
enabled = false
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 总开关 |
| `model` | String | `"nanodet-plus-m-320"` | 启动模型注册表 id（SPEC §4.6）；非缺省 `model_path` 覆盖它 |
| `allow_upload` | bool | `false` | 允许运行时上传模型（SPEC §4.6 能力 `ai_upload`；模型文件属不可信输入） |
| `model_path` | String | `"models/nanodet-m.onnx"` | NanoDet-Plus ONNX 路径（相对工作目录或绝对） |
| `confidence_threshold` | f32 | `0.5` | 检测上报的最小置信度（0..=1）。**热键**：`PUT /api/config` 改动即刻生效（运行中的采样工作器下一帧起读取新值） |
| `interval_ms` | u64 | `1000` | 每相机两次推理的最小间隔。**热键**：同上，改动即刻生效 |

### [audio_ai] - 声音事件检测

常驻麦克风监听 + YAMNet 分类。关注的声音类别触发时（投票平滑、逐类冷却），
发出 SPEC §6 `alarm` 事件：`source: "audio"`，`class` 带类别显示名，
`score` 带投票得分。

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

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关。默认关闭——常驻麦克风监听属主动开启。 |
| `device` | String | `"default"` | 输入设备选择：`"default"` 或 ALSA 设备描述的子串。 |
| `classes` | Vec<String> | 15 个默认类别（Dog/Bark/Yip/Howl/Bow-wow、Baby cry、Screaming、Shout、Glass/Shatter/Breaking、Smoke detector/Fire alarm、Siren、Knock） | 关注的 YAMNet 类别显示名（精确匹配）。未知名称启动时记日志并忽略。 |
| `threshold` | f32 | `0.3` | 类别触发所需投票得分（0 < threshold ≤ 1）。 |
| `cooldown_secs` | u64 | `30` | 同类别两次告警的最小间隔（必须 > 0）。 |
| `model_path` | String | `"models/audio/yamnet.onnx"` | YAMNet ONNX 模型路径。 |
| `vad_model_path` | String | `"models/audio/silero_vad.onnx"` | Silero VAD ONNX 路径（语音存在信号）。 |

**注意事项：**

- 滚动 0.96 秒窗口、50% 重叠分类；连续三个窗口得分平均后才可能触发，
  单窗口毛刺不会告警。
- 低于 RMS 底噪的窗口直接跳过分类。
- 同时发布实时语音存在标志，供语音交互环路消费。

### [voice] - 语音交互（唤醒词 + 转写）

唤醒词检测 + 离线语音转文字。需要 `voice` cargo feature（构建期静态链接
sherpa-onnx）。识别到唤醒词后，离线转写 `capture_secs` 秒音频并发送
`voice_transcript` SSE 事件；`[llm]` 开启时转写文本交本地 LLM 作答，
`[tts]` 开启时回复会被播报。

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
# 连续对话（#30-B）：每次语音回复后开启等长跟问窗口，窗口内无需唤醒词
# （0 = 关闭）。
follow_up_window_secs = 0.0
vad_model = "models/voice/vad/silero_vad.onnx"
# 唤醒后 VAD 断句采音：说话期间窗口自动延长（上限 capture_secs + 6 秒），
# 收到约 0.6 秒尾静音即结束——先说唤醒词停顿再提问不再被固定窗口截断。
# 依赖 vad_model，缺文件回落固定窗口（fail-open）。
capture_endpointing = true
# 打断（#45）
interrupt_enable = true
interrupt_confidence = 0.6
# 播报期打断（需主机 AEC 采集源） / playback-phase (host AEC source required)
barge_in = "off"
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关（需 `voice` 构建特性）。 |
| `kws_encoder` | String | `"models/voice/kws/encoder.int8.onnx"` | Zipformer transducer KWS 编码器 |
| `kws_decoder` | String | `"models/voice/kws/decoder.int8.onnx"` | Zipformer transducer KWS 解码器 |
| `kws_joiner` | String | `"models/voice/kws/joiner.int8.onnx"` | Zipformer transducer KWS joiner |
| `kws_tokens` | String | `"models/voice/kws/tokens.txt"` | KWS tokens 文件 |
| `wake_word` | String | `"小蜜蜂"` | KWS 监听的唤醒短语 |
| `keywords_file` | String | `"models/voice/kws/keywords.txt"` | 关键词文件，每行 `音素串 @显示名`（zh-en 音素模型，如 `x iǎo m ì f ēng @小蜜蜂`）。 |
| `keywords_threshold` | f32 | `0.25` | 唤醒灵敏度——越低越容易触发。 |
| `keywords_score` | f32 | `1.0` | 接受唤醒的最低得分。 |
| `paraformer_model` | String | `"models/voice/paraformer/model.int8.onnx"` | 离线 paraformer 转写模型。缺省为中文版（普通话+嵌入英文）；**粤语/普通话/英语请换三语版**（`models/voice/paraformer-trilingual/…`，234MB，Apache-2.0，见 `models/README.md`）。 |
| `paraformer_tokens` | String | `"models/voice/paraformer/tokens.txt"` | paraformer tokens 文件 |
| `follow_up_window_secs` | f32 | `0.0` | 回复后的免唤醒跟问窗（0 = 关闭；离家监听与打断轮按需临时武装） |
| `vad_model` | String | `"models/voice/vad/silero_vad.onnx"` | 断句/监听窗用的 Silero VAD |
| `capture_endpointing` | bool | `true` | 采音期 VAD 断句：说话中软死线顺延，~0.6s 尾静音即切段转写 |
| `capture_secs` | u32 | `4` | 唤醒词识别后采集音频的秒数。 |
| `num_threads` | i32 | `1` | 推理线程数（目标主机较小）。 |
| `speaker_embedding_model` | String | `models/voice/speaker/campplus.onnx` | 说话人声纹嵌入模型（3D-Speaker CAM++ zh_en）。**文件缺失只禁用声纹特性**，唤醒+转写照常。 |
| `speaker_verify` | bool | `false` | 声纹验证门控：开启后唤醒词须匹配已注册说话人才开采集窗（无注册档案时 fail-open 放行并 WARN 一次）。 |
| `speaker_threshold` | f32 | `0.55` | 声纹余弦相似度阈值（CAM++ 典型 0.5–0.6，建议按麦克风实测标定）。 |
| `verify_window_secs` | f32 | `2.0` | 唤醒词验证取样的环形缓冲秒数（须覆盖唤醒词发音时长）。 |
| `interrupt_enable` | bool | `true` | 打断（#45）：思考期免唤醒监听 + 停词/Laya 打断策略。 |
| `interrupt_confidence` | f32 | `0.6` | 打断判定的最低 Laya 置信度，低于则按附和忽略（保守 fail-open）。 |
| `barge_in` | String | `"off"` | 播报期打断：`"off"` 全哑（默认）；`"aec"` 播报期间只哑唤醒词、VAD 保持监听（可对正在播报的回答说"停"或新问题打断）。**需主机回声消除采集源**（pipewire `module-echo-cancel`，webrtc，增益 ≤100%），否则自身 TTS 会直灌 ASR。 |

**注意事项：**

- 等待唤醒词期间不录制、不传输任何音频——关键词模型只在本地比对极短的
  音频指纹。
- `capture_secs` 限定一句话的长度；唤醒词之后再说话。
- 声纹门控是**便利性过滤，不是安全认证**：短语音（唤醒词 <1s）的声纹判别
  弱于整句，嗓音相近的家人可能通过——请勿将其作为唯一安全边界。
- 说话人注册走 Web UI（记录页「说话人声纹」卡片）或 `/api/voice/speakers`
  端点（见 API 文档）；注册即念 3 遍唤醒词。
- 外接 USB 麦克风效果远好于笔记本内置麦克风（见
  [用户手册](user-guide.md#故障排查)）。

### [llm] - 本地 LLM 对话

基于 llama.cpp（GGUF）的本地对话补全。支撑 `POST /api/chat`、Web 对话
面板与语音环路回复（`chat_reply` SSE）。需要 `llm` cargo feature 与
AVX2 档 CPU。

```toml
[llm]
enabled = false
model_path = "models/llm/qwen3-0.6b-q8_0.gguf"
# 资源分层（#30-E）：[resources] auto_tier = true 时按启动可用内存选档
# （≥8 GiB full / ≥4 GiB mid / 否则 lite）；空值回落 model_path。
model_path_mid = ""
model_path_lite = ""
n_ctx = 2048
# 缺省取逻辑核数的一半，钳制在 2..=4——在 4C/8T 实机上，线程数超过物理核
# 反而使生成变慢。此处按 4 线程机器解析出的值展示。
n_threads = 2
max_tokens = 200
no_think = true
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关（需 `llm` 构建特性 + AVX2）。 |
| `model_path` | String | `"models/llm/qwen3-0.6b-q8_0.gguf"` | GGUF 模型路径（默认 Qwen3-0.6B Q8_0）。 |
| `n_ctx` | u32 | `2048` | 对话上下文窗口。必须大于工具提示词（SPEC §3.5 约 500-1700 token）加对话本身；此前缺省 1024 会溢出并静默劣化首个 agent 轮次。 |
| `n_threads` | u32 | 逻辑核数 / 2，钳制 `2..=4` | CPU 线程数。留空则由设备按本机核数自行决定。 |
| `max_tokens` | u32 | `200` | 每次回复的生成上限。 |
| `no_think` | bool | `true` | user 轮追加 `/no_think`（关闭 Qwen3 思考模式——更快、对话向）。 |
| `model_path_mid` | String | `""` | `[resources] auto_tier` 的中档 GGUF（空 = 该档不可用） |
| `model_path_lite` | String | `""` | `[resources] auto_tier` 的轻档 GGUF（空 = 该档不可用） |

**注意事项：**

- 贪心解码（temperature 0）：答案确定，无采样漂移。
- 无论 `no_think` 与否，回复中的思考块一律剥离。
- 内存 guardrail：模型超过可用内存 2/3 时拒绝加载（fail-open）。
- 推理线程池自动设上限（`OMP_NUM_THREADS` = 核数/2，最高 4），环境已设
  则不覆盖。

### [decision] - 语音决策辅助（Laya 类型化决策）

对语音转写先做一次本地类型化意图决策（answer/device/ignore）再决定是否
花本地 LLM 应答；`ignore` 直接跳过应答。需要 `[ai]` 运行时（onnxruntime），
模型为 laya 多语 ONNX 检查点（见 `models/README.md`）。

```toml
[decision]
enabled = false
model_path = "models/decision/laya_multilingual.int8.onnx"
tokenizer_path = "models/decision/tokenizer.json"
config_path = "models/decision/laya_config.json"
min_confidence = 0.35
num_threads = 1
```

| 键 | 类型 | 缺省 | 说明 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关。 |
| `model_path` | String | `models/decision/laya_multilingual.int8.onnx` | laya ONNX 检查点（int8 量化版推荐）。 |
| `tokenizer_path` | String | `models/decision/tokenizer.json` | 检查点配套的 HuggingFace tokenizer。 |
| `config_path` | String | `models/decision/laya_config.json` | 携带 `max_len`/`head_max_len`/温度校准的 json；缺失则用安全缺省。 |
| `min_confidence` | f32 | `0.35` | 低于该置信度的决策不采纳（fail-open 维持旧行为）。 |
| `num_threads` | u16 | `1` | 推理线程。 |

### [meeting] - 会议模式（按需录音 + 说话人分离 + 分段转写）

本地会议纪要：显式 start → stop 的录音会话，停止后离线跑说话人分离
（pyannote 分割 + CAM++ 嵌入 + 快速聚类）→ 逐段三语转写 →（启用时）标点
恢复 → 每说话人经声纹档案投票打名，入库为文字纪要。**隐私姿态**：待机不
录任何音频的承诺不变——只有显式会话窗口内落盘；`keep_audio=false`（缺省）
时处理完成（含失败）即删音频，只留文字；到 `max_duration_secs` 自动停止
并走同一管线。依赖 `[voice]` 的 ASR 模型与说话人嵌入模型在场（会议复用
同一套文件）；处理需要 `voice` feature 构建。模型资产见 `models/README.md`。

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

| 键 | 类型 | 缺省 | 说明 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关（缺省关——录音必须显式开启）。 |
| `segmentation_model` | String | `models/voice/diarization/pyannote.onnx` | pyannote segmentation-3.0（sherpa-onnx 转换，int8 ~1.5MB，MIT）。 |
| `punctuation_model` | String | `models/voice/punct/model.onnx` | ct-transformer zh-en 标点（int8 ~75MB）；空串禁用标点恢复。 |
| `clustering_threshold` | f32 | `0.5` | 快速聚类距离阈值（调高=更少说话人；实测四说话人样本在 0.5 检出 5——按现场校准）。 |
| `min_duration_on` | f32 | `0.3` | 最短发声段（秒）。 |
| `min_duration_off` | f32 | `0.5` | 最短静默段（秒）；同说话人相邻段间隔 ≤ 该值时合并为一段转写。 |
| `keep_audio` | bool | `false` | 处理完成后保留 WAV（缺省删除——隐私优先）。 |
| `max_duration_secs` | u64 | `7200` | 安全上限：到点自动停止并处理（防遗忘录音）。 |
| `audio_dir` | String | `meetings` | 会话 WAV 目录（相对工作目录）。 |
| `num_threads` | i32 | `1` | 推理线程。 |

**诚实边界**：声学聚类对同嗓音家人可能合并为一个说话人，对独特嗓音可能
过分裂（可调 `clustering_threshold`）；转写质量同 `[voice]` ASR；说话人
命名依赖声纹档案命中（未命中显示"说话人 N"）——均为便利性能力，非精确
标注。

### [tts] - 语音播报

通过 `sherpa-onnx-offline-tts` **CLI 子进程**合成语音（GPL espeak-ng 依赖
隔离在子进程内，不进入本二进制），经 `aplay` 播放。TTS 开启时 LLM 回复
文本会被读出。

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
# 分语言模型（#30-D）：按回复语言选音色（粤语特征字→yue、纯 ASCII→en、
# 否则主模型）；未设置的键回落主模型。
yue_model = ""
yue_lexicon = ""
en_model = ""
en_lexicon = ""
# 合成子进程线程数（默认 2；4 核以上主机设 4 可把 5 秒语音约 16 秒的
# 合成时间砍半）。
num_threads = 4
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关。无需 cargo feature。 |
| `binary` | String | `"tmp/sherpa-libs/tools-bin/bin/sherpa-onnx-offline-tts"` | sherpa-onnx-offline-tts 二进制路径（另行从 k2-fsa release 下载）。 |
| `model` | String | `"models/voice/melo/model.onnx"` | vits-melo-tts-zh_en 语音模型（普通话主音色） |
| `lexicon` | String | `"models/voice/melo/lexicon.txt"` | 发音词典 |
| `tokens` | String | `"models/voice/melo/tokens.txt"` | TTS tokens 文件 |
| `dict_dir` | String | `"models/voice/melo/dict"` | jieba 词典目录 |
| `num_threads` | u32 | `2` | TTS 合成线程数 |
| `voice_change` | String | `"off"` | 变声（M4-b 首发）：`"high"`/`"low"` 经重采样升降音高与语速，`"off"` 原声（拼错值按 off 处理） |
| `yue_model` | String | `""` | 粤语语音模型（空 = 粤语不可用） |
| `yue_lexicon` | String | `""` | 粤语词典 |
| `yue_dict_dir` | String | `""` | 粤语词典目录 |
| `en_model` | String | `""` | 英语语音模型（空 = 英语不可用） |
| `en_lexicon` | String | `""` | 英语词典 |
| `rule_fsts` | String | `"…/number.fst,…/date.fst"` | 数字/日期归一化 FST（逗号连接）。 |
| `player` | String | `"aplay -q"` | 播放命令；留空 = 仅合成（不外放）。 |

### [tools] - 对话任务工具

接地系统回合的可选注入块（#30-A）。当前为天气：用户话语含天气意图且
`weather_enabled` 开启时，拉取 `weather_city` 当前天气进【联网】块（wttr.in，
`timeout_secs` 限时）。这些键可经 `PUT /api/config` 的 `scene.tools.*` 热调（#31）。

```toml
[tools]
weather_enabled = false
weather_city = ""
timeout_secs = 5
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `weather_enabled` | bool | `false` | 天气工具总开关 |
| `weather_city` | String | `""` | 查询城市（空 = 工具无数据） |
| `timeout_secs` | u64 | `5` | 拉取超时（秒） |

### [vlm] - 告警画面图像描述

事件触发的告警画面"发生了什么"描述，使用视觉-语言模型（Qwen3-VL，经
llama.cpp mtmd）。视觉告警边沿被接受后，触发帧 JPEG 异步送描述，结果以
`alarm_description` SSE 事件送出——告警本身绝不延迟。需要 `llm` 构建
特性（AVX2 档 CPU）。

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

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关（需 `llm` 构建特性 + AVX2）。 |
| `model_path` | String | `"models/vlm/qwen3-vl-2b-instruct-q4_k_m.gguf"` | 文本模型 GGUF。 |
| `mmproj_path` | String | `"models/vlm/mmproj-qwen3-vl-2b-instruct-q8_0.gguf"` | 视觉投影器（mmproj）GGUF。 |
| `n_ctx` | u32 | `2048` | 上下文窗口（仅图像就占约 1k 位置）。 |
| `n_threads` | u32 | `2` | CPU 线程数。 |
| `max_tokens` | u32 | `100` | 每次描述的生成上限。 |
| `repeat_penalty` | f32 | `1.1` | 最近 64 个生成 token 的重复惩罚（llama.cpp 语义）。没有它贪心解码会在困难帧的列举项上循环；`1.0` 关闭。 |
| `prompt` | String | 中文安防措辞指令 | 提供给模型的指令；要求一句话作答。 |

**注意事项：**

- **单飞调度**：同时只跑一个描述，两次描述起点之间有 120 秒下限——检测
  抖动不会把 VLM 负载堆到小主机上。
- 与 `[llm]` 共享同一 llama.cpp 后端实例与内存 guardrail。

### [face] - 人脸识别

YuNet 检测 + SFace 128 维嵌入（登记、按事件归属姓名、离家看家的已知访客
问候）。默认关——老机器上 CPU 成本可观。`detect_input` 遵循模型作者参考
实现；随包 zoo 模型接受动态输入。

```toml
[face]
enabled = false
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 总开关 |
| `detect_model` | String | `"models/face/face_detection_yunet_2023mar.onnx"` | YuNet 检测模型（OpenCV zoo） |
| `recog_model` | String | `"models/face/face_recognition_sface_2021dec.onnx"` | SFace 识别模型（128 维嵌入） |
| `detect_input` | u32 | `320` | 检测输入边长；更大可检出更小的脸，CPU 二次方增长 |
| `match_threshold` | f32 | `0.363` | 姓名匹配的余弦相似度阈值（OpenCV SFace 参考值） |
| `enroll_frames` | usize | `8` | 一次登记接受的帧数（更多 = 模板更稳） |
| `ttl_ms` | u64 | `10_000` | 帧龄超过此值即跳过检测 |

### [models] - 模型资产目录

可下载模型资产的根目录（SPEC §4.6 模型管理器）。

```toml
[models]
dir = "models"
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `dir` | String | `"models"` | 模型资产根目录（上传、注册表下载） |

### [ocr] - 文字识别

端侧 OCR（PP-OCR v4 检测 + v5 识别，中英），经 `POST /api/ocr` 暴露
（JPEG body → `{"items":[{text,score,bbox}]}`），能力位 `ocr`。

```toml
[ocr]
enabled = false
det_path = "models/ocr/ch_PP-OCRv4_det_infer.onnx"
rec_path = "models/ocr/ppocrv5_mobile_rec.onnx"
dict_path = "models/ocr/ppocrv5_dict.txt"
max_side = 960
det_threshold = 0.3
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `enabled` | bool | `false` | 主开关。 |
| `det_path` | String | `"models/ocr/ch_PP-OCRv4_det_infer.onnx"` | DBNet 文本检测模型。 |
| `rec_path` | String | `"models/ocr/ppocrv5_mobile_rec.onnx"` | CRNN 文本识别模型。 |
| `dict_path` | String | `"models/ocr/ppocrv5_dict.txt"` | 识别字典（随 git 分发）。 |
| `max_side` | u32 | `960` | 送检测器的图像最长边（32 的倍数）。 |
| `det_threshold` | f32 | `0.3` | DBNet 二值化阈值。 |

### 每相机流配置键（Web UI / API）

部分相机选项是每相机的 JSON 配置（在"相机"视图或 `PUT /api/cameras/{id}`
设置），不在 TOML——典型是 `substream`（`{enabled, width, height, fps,
bitrate}`，SPEC 附录 A #20）：低分辨率 H.264 副码流，经 `stream.sub.mse`、
RTSP `/live/{id}/sub` 挂载点与 ONVIF `sub` profile 暴露。在下次流
（重）启动时生效。

### [plugin] - 插件触发器框架（SPEC 附录 A #47）

插件 = 按 id 声明的清单（`[plugin.<id>]`）：对设备自身事件源的触发规则，调
用已注册工具（内置 + MCP stdio 服务器）。每次执行经 SSE `plugin_invocation`
透明上报（outcome ok | error | skipped）。

```toml
[plugin.lamp-link]
version = 1
tools = ["mcp:home"]
triggers = [
  { on = "away_state", when = "armed", cooldown_secs = 60, run = [
    { tool = "lamp.switch", args = { room = "客厅", on = true } } ] }
]
[plugin.lamp-link.permissions]
events = ["away_state"]   # 最小事件集——规则源不在白名单内即 skipped
egress = []               # 外发白名单（知情原则，沿用 #44）
```

**字段参考：**本节是任意键清单映射 + 嵌套触发表——完整语法见 SPEC 附录
A #47。四个触发源全部实接：`alarm`（`when` = ai/audio/zone 之一，空=任意）、
`away_state`（`when` = armed/disarmed，空=两者）、`schedule`（`when` =
"HH:MM" 本地每日边界分，20s 对时、经冷却每边界分只发一次）、
`audio_level`（`when` = 数值阈值，level ≥ 阈值即触发，10Hz 流由冷却收
口）。动作三类：`tool`（15s 超时）| `speak`（TTS 播报带播放自哑；TTS 不
可用报 `skipped:tts-inactive`）| `notify`（桌面通知；无桌面会话=诚实 ok
no-op）。冷却缺省 60s。触发源不在 `permissions.events` 内或
schedule/audio_level 的 `when` 格式不合法的清单装载时告警跳过。

### [database] - SQLite 数据库

配置用于摄像头设置、协议配置、会话和用户的 SQLite 数据库路径。

```toml
[database]
path = "~/.local/share/mibee-eye/mibee_eye.db"
```

**字段参考：**

| 字段 | 类型 | 默认值 | 描述 |
|------|------|---------|------|
| `path` | String | `~/.local/share/mibee-eye/mibee_eye.db` | SQLite 数据库文件路径（XDG 兼容默认值） |

**注意事项：**

- 使用 XDG 数据目录作为默认路径：`~/.local/share/mibee-eye/mibee_eye.db`
- 如果 XDG 数据目录不可用，则回退到 `/tmp/mibee-eye/mibee_eye.db`
- 存储摄像头配置、协议配置、会话、用户和流会话数据

## 配置验证

在启动时验证配置。以下规则适用：

- **端口约束**：所有端口必须 > 1024（web.port、rtsp.server_port、gb28181.platform_sip_port）
- **端口冲突**：web.port 不得等于 rtsp.server_port
- **速率限制**：security.rate_limit_max 必须 > 0
- **GB28181 间隔**：gb28181.register_interval_secs 必须 > 0（如果启用）
- **RTMP 推送**：rtmp_push.reconnect_interval_secs 必须 > 0（如果启用）
- **RTMP 推送**：rtmp_push.max_reconnect_attempts 必须 > 0（如果启用）
- **日志级别**：observability.log_level 必须是以下之一：trace、debug、info、warn、error
- **录制路径**：recording.path 不得为空
- **录制段**：recording.segment_duration_secs 必须 > 0
- **声音事件**：audio_ai.threshold 必须在 (0, 1] 内，classes 不得为空，
  cooldown_secs 必须 > 0（如果启用）

## 示例

### 启用所有协议的生产配置

```toml
# 启用所有出站协议的生产配置
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
alarm_notify_enabled = true
alarm_cooldown_secs = 30
position_longitude = ""
position_latitude = ""

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

### 本地开发配置

```toml
# 本地开发，最小安全限制
[web]
host = "127.0.0.1"
advertised_host = "127.0.0.1"

[security]
rate_limit_max = 100

[observability]
log_level = "debug"
```

### 最小配置

```toml
# 最小配置 — 大多数字段使用默认值
[web]
port = 8443

[capture]
video_device = "/dev/video0"
audio_device = "default"

[recording]
enabled = true
path = "./recordings"
```

### 资源受限环境

```toml
# 为低资源环境优化
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

### 网络特定配置

```toml
# 不同网络环境的配置
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

### TLS 证书管理

```toml
# 生产环境，使用自定义 TLS 证书
[web]
port = 8443
# 在 tls/cert.pem 和 tls/key.pem 提供您自己的 CA 签名证书
# 证书在文件 mtime 更改时自动重载
```

**TLS 证书注意事项：**

- 首次运行时在 `tls/cert.pem` + `tls/key.pem` 自动生成自签名证书
- 证书支持文件 mtime 更改时的热重载
- 生产环境应提供 CA 签名的证书
- 通用名称：CN=mibee-eye，SAN：mibee-eye.local
- 自动生成的开发证书的有效期：约 30 天

## 配置最佳实践

1. **切勿提交机密信息**：将敏感配置存储在 `config.local.toml` 中，该文件已被 gitignored
2. **使用描述性文件名**：`config.prod.toml`、`config.test.toml` 用于不同环境
3. **验证配置**：部署前测试配置文件（使用 --config 标志运行应用程序）
4. **记录变更**：使用新选项保持配置文档更新
5. **监控性能**：使用可观察性跟踪不同配置的资源使用情况
6. **协议默认值**：所有出站协议（RTMP 推送、ONVIF、GB28181）默认关闭以确保安全
7. **端口约束**：使用端口 > 1024 以避免特权要求
8. **通告主机**：为多宿主网络或 NAT 后设置 `web.advertised_host`

## 故障排除

### 常见问题

1. **端口已被占用**：尝试不同的端口或确保之前的实例已停止。检查验证中的端口冲突。
2. **设备未找到**：验证设备路径和权限（Linux：`video` 组成员身份）。这是仅本地捕获 — 不会发现远程摄像头。
3. **速率限制问题**：检查 `security` 部分配置。确保 `rate_limit_max` > 0。
4. **TLS 错误**：验证证书配置和端点。检查 `tls/cert.pem` 和 `tls/key.pem` 是否存在。
5. **OpenTelemetry 失败**：系统在没有收集器的情况下继续运行，但可能会丢失指标。验证通过。
6. **端口冲突**：web.port 不得等于 rtsp.server_port。验证将拒绝。
7. **GB28181 注册失败**：检查 platform_sip_address、platform_sip_port、username、password。平台必须可达。
8. **RTMP 推送失败**：验证 push_url、app_name、stream_name。外部推流服务器必须正在接受连接。
9. **录制路径不可写**：确保 recording.path 存在且可写。自动修剪需要读/写访问权限。

### 验证

通过使用 `--config` 启动应用程序来验证配置文件：

```bash
# 通过启动应用程序测试配置
cargo run -- --config test-config.toml

# 如果配置无效，启动失败并显示描述性错误
# 示例错误："web.port: must be > 1024, got 80"
# 示例错误："rtsp.server_port: must not equal web.port"
```

### 获取帮助

对于配置问题：
1. 查看本参考文档
2. 查看源代码 `src/config.rs` 和 `crates/web/src/config.rs` 以获取权威默认值
3. 检查默认的 `config.toml` 文件
4. 查看 GitHub 问题讨论
5. 通过启动验证：`cargo run -- --config your-config.toml`