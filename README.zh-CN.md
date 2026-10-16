# mibee-eye-notebook

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/Rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)
[![Platform: Linux Tier 1](https://img.shields.io/badge/Platform-Linux%20Tier%201-green.svg)](../docs/POSITIONING.md)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](docs/zh/contributing.md)

[English](README.md) · [文档](docs/zh/)

基于 Rust 构建的专业本地采集代理。

采集本机摄像头和麦克风，编码为 H.264/AAC，通过 RTSP 服务端 / RTMP 推流 / ONVIF 设备端 / GB/T 28181 设备端 向外部 NVR 提供流媒体服务。

**MiBee Eye** 摄像头家族成员：[mibee-eye-rs](https://github.com/xiqing85/mibee-eye-rs) · [mibee-eye-go](https://github.com/xiqing85/mibee-eye-go) · [mibee-eye-webui](https://github.com/xiqing85/mibee-eye-webui)（共享前端 + API 规范）。

> 二进制与 systemd 服务保留历史名称 `mibee-eye`。

## 功能特性

- **本地采集** — 摄像头通过 V4L2（Linux）/ MSMF（Windows），麦克风通过 ALSA / WASAPI
- **对外协议** — RTSP 服务端（客户端拉流）、RTMP 推流、ONVIF 设备端点、GB/T 28181 设备注册（全部默认关闭，通过 Web 界面启用）
- **GB/T 28181-2022 设备面** — 告警事件（SSE `alarm` + Alarm NOTIFY，AI 上升沿 + 冷却）、DeviceControl（IFrameCmd 强制关键帧、RecordCmd 录像门）、优雅注销（REGISTER Expires: 0）、静态 MobilePosition 上报、DeviceConfig FrameMirror 运行时镜像、SIP-Date 校时观察——全部随共享库 gb28181-rs 提供
- **端侧智能（全部故障开放、主动开启）** — 视觉目标检测（NanoDet）实时叠加框；声音事件检测（YAMNet）发出 `source: "audio"` 告警（投票平滑 + 逐类冷却）；用户绘制入侵/越线区域 + ByteTrack 子集跟踪（`zone_event` SSE）；OCR（PP-OCR v4/v5，`POST /api/ocr`）——SPEC 附录 A #21
- **语音交互** — 唤醒词小蜜蜂（sherpa-onnx zipformer KWS）→ 离线 paraformer 三语转写 → 本地 LLM 回复（llama.cpp + Qwen3 GGUF）→ 按回复语言选音色的 TTS 播报（粤/英/普通话；sherpa-onnx CLI 子进程，GPL 隔离）；跟问窗口内免唤醒连续对话（`follow_up_window_secs`）；对话面板实时麦克风波形条（`audio_level` SSE）——SPEC 附录 A #22/#30/#36
- **接地对话（SPEC 附录 A #29/#35）** — 每轮回答都知道相机当前看到什么（实时检测 + VLM 告警描述）、本地时钟与系统状态，可选注入天气；`vision:true` 把新鲜帧交给端侧 VLM 看图直答；配置云端后对话云优先、失败回落本地（`engine` 披露实际路径）
- **告警智能** — 告警画面的视觉-语言描述（Qwen3-VL 经 llama.cpp mtmd），异步 `alarm_description` SSE，绝不延迟告警本身；单飞调度 + 描述间隔下限
- **H.264 / H.265** — 手写 NAL 单元解析器、关键帧检测、SPS/PPS 提取
- **Web 界面** — Axum REST API + 嵌入式 SPA、TLS 通过 rustls、基于会话的身份认证、双语（zh-CN / en-US）、日/夜间主题
- **本地录制** — MP4 分段归档，自动清理，可按摄像头配置
- **浏览器预览** — MJPEG 多部分实时流、JPEG 快照端点、fMP4/MSE H.264 直播（`stream.mse`），可选低分辨率省流子码流（`stream.sub.mse`、RTSP `/live/{id}/sub`、ONVIF `sub` Profile —— 每相机 `config.substream`，SPEC 附录 A #20）
- **资源约束** — 信号量控制的并发（最多 16 路流）、每流内存预算
- **对话记录（规范 §3.4）** — 每轮对话（语音或网页）落持久人读日志：听到/输入文本、逐模型调用的内部"思考"条目（决策/云端/VLM/本地 LLM/TTS，含失败回落腿）与 AI 回复及引擎；`GET /api/conversations` + 实时 SSE `conversation` 事件驱动助手页"对话记录"卡；无回复轮诚实落库
- **桌面集成（规范附录 A #42）** — 有桌面会话的主机上提供托盘图标 + 桌面通知（ksni StatusNotifierItem + org.freedesktop.Notifications，纯 Rust D-Bus）；告警上升沿通知、左键打开 Web 界面；headless 服务器完全 fail-open
- **可观测性** — 结构化日志、OpenTelemetry 追踪（132+ 仪器化 span）、Prometheus 指标（14+ 计数器/仪表）、可选的 Loki 远程日志发送
- **资源自适应** — 启动时按可用内存对 AI 功能做预算制准入（`[resources] feature_gate`），小内存主机自动裁掉重模型尾部；准入表经状态页"资源档位"卡与 `capabilities.resource` 暴露
- **安全性** — 速率限制与指数退避、CSRF（双提交 cookie）、CSP 头、TLS 仅
- **动态管理** — 通过 Web 界面协议热切换（无需重启）、热插拔摄像头检测（udev）、SSE 实时事件
- **低资源占用** — 目标 <5% CPU 空闲占用、<200 MB 内存；尽可能零拷贝

### Crate 职责

代码行数为当前代码树的实测值（`grep -v '^\s*\(//\|/\*\|\*\)' | grep -v '^\s*$'`，
排除空行与注释）——此前的数字最多偏差 8 倍。

| Crate | 代码行数 | 作用 |
|-------|---------|------|
| `web` | ~21k | Axum REST API、约 40 个路由模块、嵌入式 SPA、TLS、国际化、主题 |
| `streaming` | ~18k | StreamHub 扇出编排器（到 Web 预览、文件输出、RTSP、RTMP、ONVIF、GB28181）、源/输出适配器、进程内 OpenH264/AAC 编解码栈，以及整个 AI 面（检测、声音事件、ASR、LLM、VLM、TTS、人脸、OCR、决策辅助） |
| `protocols` | ~6.5k | 媒体面协议实现：RTSP、RTMP、RTP、H.264（信令协议来自共享协议库） |
| 根包（`src/`） | ~5.1k | 二进制入口：配置、语音/对话桥、离家监控、桌面集成 |
| `security` | ~1.8k | 基于会话的身份认证、速率限制、CSRF 保护、加密 |
| `observability` | ~1.4k | 结构化追踪、OpenTelemetry 导出、Prometheus 指标、Loki 远程日志发送 |
| `capture` | ~1.4k | 视频（nokhwa）+ 音频（cpal）设备包装器 |

## 架构

```mermaid
flowchart TB
    subgraph L1["Web UI（Axum + SPA）"]
        direction LR
        L1a["REST API · TLS · 会话认证"]
        L1b["双语（zh-CN/en-US）· 主题 · SSE 事件"]
    end
    subgraph L2["Streaming Hub"]
        direction LR
        L2a["Source → BufferPool → fan-out"]
        L2b["ResourceController（最多 16 路流）"]
    end
    subgraph L3["输出面"]
        direction LR
        L3a["Web 预览 · MP4 录制"]
        L3b["RTSP · RTMP 推流"]
        L3c["ONVIF · GB28181 设备端"]
    end
    subgraph L4["协议层"]
        direction LR
        L4a["RTP/RTSP/RTMP · H.264 NAL（仓内自实现，媒体面）"]
        L4b["信令：onvif-device-rs · gb28181-rs"]
    end
    subgraph L5["采集层"]
        direction LR
        L5a["视频（nokhwa）· 音频（cpal）"]
        L5b["OpenH264 进程内编码 · udev 热插拔"]
    end
    SEC["安全 · 可观测性<br/>认证 · TLS · CSRF · CSP<br/>追踪 · 指标 · Loki 日志"]
    L5 -->|"H.264 AU + G.711/AAC"| L2
    L1 -->|"配置 / 控制"| L2
    L2 --> L3
    L3 --- L4
    SEC -.->|包裹| L1
```

## 工作空间布局

```
mibee-eye/
├─ src/                # 二进制入口、配置、类型、错误处理
├─ crates/
│  ├─ protocols/       # RTSP、RTMP、ONVIF、GB28181、RTP、H.264
│  ├─ streaming/       # StreamHub 扇出、源/输出适配器、MiBee 客户端
│  ├─ web/             # Axum REST API + 嵌入式 SPA + TLS
│  ├─ security/        # 身份认证、TLS、加密、速率限制
│  ├─ capture/         # 视频 + 音频捕获包装器
│  └─ observability/   # tracing + OTel + Prometheus
├─ migrations/         # SQLite 模式
└─ config.toml         # 默认运行时配置
```

## 协议支持状态

| 协议 | 组件 | 实现方式 | 状态 |
|------|------|---------|------|
| **身份认证（登录/注销/设置/重置）** | 基于会话 | bcrypt + 24小时会话 + 速率限制 + 指数退避锁定 | ✅ 已实现并连接 |
| **TLS (rustls)** | 仅 HTTPS，无 HTTP | 自动生成自签名开发证书，热重载 | ✅ 已实现并连接 |
| **RTSP 服务端** | RFC 2326 + Digest 认证 + RTP 交错 | 手写（`RtspServer`） | ✅ 已实现并连接 |
| **RTMP 推流** | 握手 + 连接 + 发布 | 手写（`RtmpOutput`，当 `rtmp_push.enabled=true` 时通过 StreamHub 自动连接） | ✅ 已实现并连接 |
| **ONVIF 设备** | WS-Discovery + SOAP 设备服务 | [`onvif-device-rs`](https://github.com/mickeyzzc/onvif-rs)（当 `onvif.enabled=true` 时启动） | ✅ 已实现并连接 |
| **GB/T 28181 设备** | SIP REGISTER (Digest) + INVITE + RTP 推送 | [`gb28181-rs`](https://github.com/mickeyzzc/gb28181-rs)（含 GB35114 认证；`Gb28181Output` 在 INVITE 时动态连接，BYE 时断开） | ✅ 已实现并连接 |
| **GB28181 告警管线** | AI 检测上升沿 → `alarm` SSE + Alarm NOTIFY | `AlarmBridge`（冷却门控）+ gb28181-rs `notifier()`；优先级 4 / 方法 5 / 类型 2（2022 标准表） | ✅ 已实现并连接 |
| **GB28181 DeviceControl** | IFrameCmd / RecordCmd / GuardCmd / TeleBoot / PTZ | gb28181-rs 控制接缝：OpenH264 强制关键帧、RecordCmd 门控本地录制、无执行器命令 ack-only | ✅ 已实现并连接 |
| **GB28181 优雅注销** | SIGTERM / 协议停止时发 REGISTER `Expires: 0` | gb28181-rs `shutdown_with_deregister`（401 全套舞步、2s 超时、超时兜底 abort） | ✅ 已实现并连接 |
| **GB28181 DeviceConfig FrameMirror** | 运行时镜像模式 0-3（A.2.1.22） | 共享原子翻转标志，与静态安装翻转 XOR 复合，烧入编码/快照/AI（BasicParam 维持拒绝，与树莓派版一致） | ✅ 已实现并连接 |
| **GB28181 SIP-Date 校时观察** | REGISTER `Date` 头的平台时钟漂移（§9.10.2） | 漂移>5s WARN（每次再偏 5s 才重复告警）；绝不改系统时钟 | ✅ 已实现并连接 |
| **GB28181 MobilePosition** | 订阅周期上报静态坐标 | gb28181-rs `with_position_source`（配置为空则不上报） | ✅ 已实现并连接 |
| **H.264** | NAL 单元解析器、SPS/PPS、关键帧检测 | 手写（`H264Parser`） | ✅ 用于所有视频输出 |
| **H.265 解码** | 浏览器回退到 H.264 | — | ⚠️ 浏览器不支持通用；v1 仅 H.264 |
| **浏览器实时预览** | 通过 `<img>` 的 MJPEG 多部分流 | `/api/cameras/{id}/live` 路由（进程内 OpenH264，无 ffmpeg 子进程） | ✅ 已实现并连接 |
| **本地录制** | 带自动清理的 MP4 分段归档 | 根据录制配置自动连接每摄像头的 `FileOutput` | ✅ 已实现并连接 |
| **国际化（zh-CN / en-US）** | 翻译层 | `app.js` 中的 `t()` 字典，语言切换持久化到用户设置 | ✅ 已实现并连接 |
| **日/夜间主题** | 主题切换 | 系统偏好自动检测，手动覆盖持久化 | ✅ 已实现并连接 |
| **CSRF / CSP** | 双提交 cookie + 严格头 | 登录时颁发 CSRF token，通过 `X-CSRF-Token` 头验证；严格 CSP 头 | ✅ 已实现并连接 |
| **远程日志发送** | Loki / OTLP 日志 | `tracing-loki` 层，带批处理 + 刷新间隔，失败开放 | ✅ 已实现并连接 |
| **OTel 追踪** | OTLP gRPC 导出器 | 管道已连接 + 132 个 `#[tracing::instrument]` span 覆盖所有处理程序和关键路径 | ✅ 已实现并连接 |
| **Prometheus 指标** | 计数器/仪表 | 14+ 自定义指标，位于 `/metrics` 端点 | ✅ 已实现并连接 |
| **速率限制** | 每 IP 固定窗口 | `parking_lot::Mutex` 保护，成功登录后重置，5次失败后指数退避 | ✅ 已实现并连接 |
| **协议热切换** | 启动/停止无需重启 | `ProtocolRuntime` 通过 Web 界面启动/停止 ONVIF/GB28181/RTMP | ✅ 已实现并连接 |
| **热插拔监控** | 摄像头添加/移除 | udev netlink ADD/REMOVE 自动发现插入的摄像头，标记拔出的为离线 | ✅ 已实现并连接 |
| **SSE 事件总线** | 实时事件 | `/api/events` 向浏览器推送摄像头添加/离线事件 | ✅ 已实现并连接 |
| **跨平台：Windows** | MSMF + WASAPI | — | ❌ 不编译（计划 Tier 2，阻碍：`libc::getifaddrs` 仅 POSIX） |
| **跨平台：macOS** | AVFoundation + CoreAudio | — | ❌ 计划 Tier 2（可编译但 `/dev/videoN` 路径不存在） |

**图例**: ✅ 已实现 · ⚠️ 有限/回退 · ❌ 缺失/不支持

## 资源目标

| 指标 | 目标 | 机制 |
|------|------|------|
| CPU（空闲） | <5% | 零拷贝 I/O、全异步、无忙循环 |
| 内存 | <200 MB | `BufferPool`（10 MB 池）、每流预算、`ResourceController` |
| 并发流数 | ≤16 | `ResourceController` 中通过 `tokio::sync::Semaphore` 控制 |
| 首帧延迟 | <500 ms | 最小缓冲、急切关键帧检测 |

## 快速开始

```bash
# 克隆并进入
git clone https://github.com/xiqing85/mibee-eye-notebook.git
cd mibee-eye-notebook

# 安装系统依赖（Linux）
sudo apt install libv4l-dev libasound2-dev libclang-dev
sudo usermod -aG video $USER
# 注销后重新登录以使组变更生效

# 构建
cargo build --release

# 配置
cp config.toml config.local.toml
# 根据需要编辑 config.local.toml

# 运行
cargo run --release -- --config config.local.toml
```

## 构建

**前置条件（Linux）：**

```bash
# 安装系统依赖
sudo apt install libv4l-dev libasound2-dev libclang-dev

# 将用户添加到 video 组以获取摄像头权限
sudo usermod -aG video $USER
# 注销后重新登录以使组变更生效
```

**构建和运行：**

```bash
cargo build                                # 调试构建
cargo build --release                      # 发布构建
cargo run -- --config config.toml          # 使用配置运行
cargo run -- --reset-password              # 密码重置 CLI
```

**开发：**

```bash
cargo test                                 # 运行所有测试
cargo test -p protocols                    # 测试单个 crate
cargo ci-clippy                            # 代码检查（clippy -D warnings）
cargo fmt-check                            # 格式检查
```

## 配置

复制并编辑默认配置：

```bash
cp config.toml config.local.toml
```

`config.local.toml` 已被 .gitignore 忽略——请在此放置本地覆盖配置。

默认端口：Web UI `8443`（TLS）、RTSP `8554`、RTMP `1935`。

## 文档

完整文档请参阅 [docs/zh/](docs/zh/)：

- [用户手册](docs/zh/user-guide.md) — Web 界面与 AI 功能的日常使用
- [快速入门](docs/zh/getting-started.md)
- [安装指南](docs/zh/installation.md)
- [配置说明](docs/zh/configuration.md)
- [API 参考](docs/zh/api.md)
- [架构说明](docs/zh/architecture.md)
- [贡献指南](docs/zh/contributing.md)

英文文档：[README.md](README.md) | [docs/en/](docs/en/)

---

## 许可证

基于 [Apache-2.0](LICENSE) 许可证开源。
