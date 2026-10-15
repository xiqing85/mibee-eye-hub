# 安装指南

本指南涵盖 mibee-eye（MiBee Eye）的安装和部署，这是一个用 Rust 构建的专业笔记本监控代理。

## 系统要求

### 操作系统

- **Linux（一等支持）** - 完整支持 V4L2/ALSA，针对资源使用进行了优化
- **Windows（二等支持）** - 有限的 MSMF/WASAPI 支持，功能不完整
- **macOS** - 不在支持范围内

### 软件要求

- **Rust 1.88+** - Rust 2024 版本的 MSRV
- **RAM**：运行时目标 <200MB 内存
- **CPU**：目标 <5% CPU 空闲占用
- **磁盘**：二进制文件 + 依赖项约 50MB

### 硬件要求

- **摄像头**：V4L2 兼容（Linux）或 Media Foundation（Windows）
- **麦克风**：ALSA 兼容（Linux）或 WASAPI（Windows）
- **网络**：流媒体协议的 TCP/UDP

## Linux 安装

### 系统依赖

安装所需的系统包：

```bash
sudo apt install libv4l-dev libasound2-dev libclang-dev
```

### 用户权限

将用户添加到 video 组以获取摄像头访问权限：

```bash
sudo usermod -aG video $USER
```

**重要**：注销并重新登录使组变更生效。

### 特权端口

默认情况下，mibee-eye 使用：
- Web UI：8443（TLS）
- RTSP：8554  
- RTMP：1935

这些端口避免了特权范围（<1024）。如果需要使用更低的端口，设置功能绑定：

```bash
setcap 'cap_net_bind_service=+ep' ./target/release/mibee-eye
```

## Windows 安装

Windows 支持是二等的，功能不完整。此安装仅用于开发/测试目的。

### 系统依赖

安装所需的组件：

1. **Visual Studio Build Tools** - nokhwa 编译的 C++ 构建工具
2. **Media Foundation Runtime** - 内置于 Windows 10/11
3. **Windows SDK** - WASAPI 音频支持所必需

### 安装步骤

1. 安装 [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
2. 安装时启用 "C++ build tools"
3. 通过 [rustup](https://rustup.rs/) 安装 Rust

```bash
# 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 安装 Windows 依赖项（开发者设置）
choco install visualstudio2022buildtools visualstudio2022-workload-vctools
```

## 从源代码构建

### 前置条件

确保安装了 Rust 1.88+：

```bash
rustc --version  # 应该 >= 1.88
```

### 构建命令

```bash
# 克隆并进入仓库
git clone https://github.com/xiqing85/mibee-eye-notebook.git
cd mibee-eye

# 调试构建（开发）
cargo build

# 发布构建（生产）
cargo build --release

# 运行测试
cargo test

# 代码检查
cargo ci-clippy

# 格式检查
cargo fmt-check
```

### 构建产物

二进制文件位置：
- 调试版本：`target/debug/mibee-eye`
- 发布版本：`target/release/mibee-eye`

## 运行

### 命令行参数

二进制文件接受这些参数：

```bash
# 使用默认配置运行
cargo run -- --config config.toml

# 指定自定义配置和数据库路径
cargo run --release -- --config config.local.toml --db-path /path/to/database.db

# 重置密码（不启动服务器）
cargo run -- --reset-password
```

### 命令行选项

- `--config, -c`：配置文件路径（默认：`config.toml`）
- `--db-path, -d`：SQLite 数据库路径（默认：`mibee_eye.db`）
- `--reset-password`：重置用户密码（提示输入凭据）
- `--selftest-audio <wav>` / `--selftest-ocr <image>` / `--selftest-voice <wav>` / `--selftest-llm <prompt>` / `--selftest-tts <text>` / `--selftest-vlm <jpeg>`：离线一次性自检——完整跑一遍对应引擎管线并输出 JSON 结果（见[用户手册](user-guide.md#离线自检)）

## AI 模型资产

AI 引擎在启动时从 `models/` 目录加载模型。这些文件**不进 git**——部署时
下载。缺失时服务照常运行，对应能力位只是保持关闭（fail-open）。

[`models/README.md`](../../models/README.md) 是来源、体积与许可的权威
表格。概要：

| 路径 | 用途 | 体积 |
|------|------|------|
| `models/nanodet-m.onnx` | 视觉目标检测 | ~10 MB |
| `models/audio/yamnet.onnx`、`models/audio/silero_vad.onnx` | 声音事件、语音存在 | ~18 MB |
| `models/voice/kws/*`、`models/voice/paraformer/*` | 唤醒词 + 转写（sherpa-onnx releases） | ~250 MB |
| `models/voice/melo/*` | TTS 音色（k2-fsa `vits-melo-tts-zh_en`） | ~165 MB |
| `models/llm/qwen3-0.6b-q8_0.gguf` | 本地对话 LLM（ModelScope `Qwen/Qwen3-0.6B-GGUF`） | ~640 MB |
| `models/vlm/*.gguf`（文本 + mmproj） | 告警画面描述（ModelScope `Qwen/Qwen3-VL-2B-Instruct-GGUF`） | ~1.5 GB |
| `models/ocr/*.onnx` | PP-OCR 检测 + 识别 | ~21 MB |

构建期注意：

- `voice` feature 需要 sherpa-onnx **静态库**——下载 k2-fsa release
  `sherpa-onnx-v1.13.8-linux-x64-static-lib.tar.bz2`，以
  `SHERPA_ONNX_LIB_DIR=<解压目录>/lib` 构建。
- `llm`/`vlm` feature 需要 **AVX2 档 CPU**；Sandy Bridge 及更老主机只能
  构建 voice-only（`cargo build --release --features voice`）。

## 配置文件

### 本地配置

复制并自定义默认配置：

```bash
cp config.toml config.local.toml
```

编辑 `config.local.toml` 进行本地覆盖配置。此文件已被 .gitignore 忽略，不会被提交。

### 默认配置
```toml
[web]
port = 8443
host = "0.0.0.0"
advertised_host = "192.168.1.100"

[rtsp]
server_port = 8554

[rtmp_push]
enabled = false
push_url = "rtmp://192.168.1.100:1935/live"
app_name = "live"
stream_name = "stream1"
reconnect_interval_secs = 5
max_reconnect_attempts = 10

[capture]
video_device = "/dev/video0"
audio_device = "default"

[security]
rate_limit_max = 20
rate_limit_window_secs = 60

[observability]
otel_endpoint = "http://localhost:4317"
log_level = "info"

[recording]
enabled = false
path = "./recordings"
segment_duration_secs = 900
max_capacity_mb = 10240

[database]
path = "~/.local/share/mibee-eye/mibee_eye.db"
```

### 配置选项

- **web**：Web UI 设置（端口、主机、通告主机）
- **rtsp**：RTSP 服务器配置（仅出站服务器模式）
- **rtmp_push**：RTMP 推送客户端（推送到外部推流服务器的出站，不是接收服务器）
- **capture**：视频/音频设备路径（仅本地；不发现远程摄像头）
- **security**：速率限制配置（不可中毒 Mutex）
- **observability**：日志和指标设置（OTLP 跟踪、可选 Loki 远程日志推送）
- **onvif**：ONVIF 设备端点配置（可选）
- **gb28181**：GB/T 28181 设备注册（可选）
- **recording**：本地 MP4 段录制，自动修剪
- **database**：SQLite 数据库路径（XDG 兼容默认值）

```toml
[web]
port = 8443
host = "0.0.0.0"

[rtsp]
server_port = 8554

[rtmp_push]
enabled = false

[capture]
video_device = "/dev/video0"
audio_device = "default"

[security]
rate_limit_max = 20
rate_limit_window_secs = 60

[observability]
otel_endpoint = "http://localhost:4317"
log_level = "info"
```

### 配置选项

- **web**：Web UI 设置（端口、主机）
- **rtsp**：RTSP 服务器配置
- **rtmp**：RTMP 接收设置
- **capture**：视频/音频设备路径
- **security**：速率限制配置
- **observability**：日志和指标设置

## TLS 证书

### 开发环境

在开发环境中，mibee-eye 在首次运行时自动生成自签名 TLS 证书：

```bash
# 首次运行生成证书
./target/release/mibee-eye --config config.local.toml

# 证书保存到：
# - tls/cert.pem
# - tls/key.pem
```

证书使用：
- 主题：CN=mibee-eye
- SAN：mibee-eye.local
- 有效期：约 30 天

### 生产环境

在生产环境中，使用来自可信 CA 的证书：

```bash
# 使用 Let's Encrypt 和 certbot
sudo apt install certbot
sudo certbot certonly --standalone -d your-domain.com

# 将证书复制到适当位置
sudo cp /etc/letsencrypt/live/your-domain.com/fullchain.pem ./tls/cert.pem
sudo cp /etc/letsencrypt/live/your-domain.com/privkey.pem ./tls/key.pem

# 设置适当的权限
sudo chown $USER:$USER ./tls/cert.pem ./tls/key.pem
chmod 600 ./tls/cert.pem ./tls/key.pem
```

### 证书管理

- **自动续订**：为 Let's Encrypt 证书设置 cron 作业
- **轮换**：替换证书并重新启动服务
- **备份**：在安全位置保留证书备份

## 部署

### Systemd 服务

在 `/etc/systemd/system/mibee-eye.service` 创建 systemd 服务文件：

```ini
[Unit]
Description=MiBee Eye 监控代理
After=network.target
Wants=network.target

[Service]
Type=simple
User=mibee
Group=mibee
WorkingDirectory=/opt/mibee-eye
ExecStart=/opt/mibee-eye/target/release/mibee-eye --config /opt/mibee-eye/config.local.toml
Restart=always
RestartSec=10
Environment=RUST_LOG=info

# 安全设置
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true

# 资源限制
LimitNOFILE=65536
MemoryMax=256M

[Install]
WantedBy=multi-user.target
```

### 服务管理

```bash
# 启用并启动服务
sudo systemctl enable mibee-eye
sudo systemctl start mibee-eye

# 检查状态
sudo systemctl status mibee-eye

# 查看日志
sudo journalctl -u mibee-eye -f

# 重启服务
sudo systemctl restart mibee-eye
```

### 用户设置

为服务创建专用用户：

```bash
sudo useradd -r -s /bin/false mibee
sudo mkdir -p /opt/mibee-eye
sudo chown mibee:mibee /opt/mibee-eye
```

### 反向代理配置

#### 使用 TLS 终端的 Nginx

```nginx
server {
    listen 80;
    server_name your-domain.com;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl http2;
    server_name your-domain.com;

    ssl_certificate /path/to/your/cert.pem;
    ssl_certificate_key /path/to/your/key.pem;

    # 安全头
    add_header X-Frame-Options DENY;
    add_header X-Content-Type-Options nosniff;
    add_header X-XSS-Protection "1; mode=block";
    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;

    # 代理到 mibee-eye
    location / {
        proxy_pass https://localhost:8443;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # 超时设置
        proxy_connect_timeout 30s;
        proxy_send_timeout 30s;
        proxy_read_timeout 30s;
    }
}
```

### Podman/Docker 考虑因素

对于容器化部署：

```bash
# 使用 Podman 构建
podman build -t mibee-eye .

# 使用卷挂载运行
podman run -d \
  --name mibee-eye \
  --restart unless-stopped \
  --cap-add=NET_BIND_SERVICE \
  -v /opt/mibee-eye/config.local.toml:/config.toml:ro \
  -v /opt/mibee-eye/tls:/tls:ro \
  -p 8443:8443 \
  -p 8443:8443 \
  -p 8554:8554 \
  mibee-eye
# 注意：RTMP 推送是出站的；除非您正在运行外部 RTMP 推流服务器，否则不需要端口映射
```
  -p 1935:1935 \
  mibee-eye
```

## 故障排除

### 常见问题

#### 权限被拒绝（摄像头访问）

**症状**：访问摄像头时出现"权限被拒绝"

**解决方案**：验证用户是否在 video 组中

```bash
# 检查组成员身份
groups $USER

# 如果不在 video 组，添加并重新登录
sudo usermod -aG video $USER
# 注销后重新登录
```

#### 设备未找到

**症状**：找不到摄像头或麦克风设备

**解决方案**：检查设备路径和权限

```bash
# 列出视频设备
ls /dev/video*

# 列出音频设备
arecord -l

# 检查设备权限
ls -la /dev/video0
```

#### 端口被占用

**症状**：端口已在使用中的错误

**解决方案**：检查端口使用情况并调整配置

```bash
# 检查端口使用情况
sudo netstat -tulpn | grep 8443
sudo netstat -tulpn | grep 8554

# 终止冲突进程
sudo fuser -k 8443/tcp
```

#### TLS 证书问题

**症状**：TLS 握手失败或证书错误

**解决方案**：重新生成或替换证书

```bash
# 删除现有证书
rm -f tls/cert.pem tls/key.pem

# 重启服务以重新生成
sudo systemctl restart mibee-eye
```

#### 数据库连接错误

**症状**：SQLite 连接失败错误

**解决方案**：检查数据库路径和权限

```bash
# 检查数据库文件
ls -la mibee_eye.db

# 检查权限
chmod 640 mibee_eye.db
```

#### 资源限制已达到

**症状**："流太多"或内存限制错误

**解决方案**：调整资源限制或减少并发流数

```bash
# 检查当前资源使用情况
systemctl show mibee-eye --property=MemoryCurrent,LimitNOFILE

# 如需要，调整 systemd 服务限制
```

### 性能问题

#### CPU 使用率高

**解决方案**：检查流媒体配置和资源限制

```bash
# 监控资源使用情况
top -p $(pidof mibee-eye)
htop -p $(pidof mibee-eye)

# 检查流媒体日志中的错误
sudo journalctl -u mibee-eye | grep -i error
```

#### 内存使用率高

**解决方案**：检查并发流数和缓冲区配置

```bash
# 检查内存使用情况
ps aux | grep mibee-eye

# 检查流媒体配置
grep -i buffer config.local.toml
```

### 调试模式

启用调试日志进行故障排除：

```bash
# 设置 RUST_LOG 环境变量
RUST_LOG=debug ./target/release/mibee-eye --config config.local.toml

# 或在 systemd 服务文件中设置
Environment="RUST_LOG=debug"
```

### 系统信息

为错误报告收集系统信息：

```bash
# 系统和 Rust 版本
rustc --version
cargo --version
uname -a

# 包版本（Ubuntu/Debian）
dpkg -l | grep -E "(libv4l|libasound|libclang)"

# 内核模块
lsmod | grep -E "(v4l2|snd)"

# 网络信息
ip a
ss -tulpn | grep -E "(8443|8554|1935)"
```

### 获取帮助

如果继续遇到问题：

1. 查看 [GitHub Issues](https://github.com/xiqing85/mibee-eye-notebook/issues)
2. 阅读[完整文档](https://github.com/xiqing85/mibee-eye-notebook/docs/zh/)
3. 创建详细的问题报告，包括：
   - 系统信息（操作系统、版本）
   - 精确的错误消息
   - 配置文件（删除敏感数据）
   - 复现步骤
   - 预期与实际行为

> **迁移说明（品牌改名）**：开发用 TLS 证书的 CommonName 和 SubjectAltName 已从 `notebook-cam`/`notebook-cam.local` 改为 `mibee-eye`/`mibee-eye.local`。请在启动服务器前删除旧的 `tls/cert.pem` 和 `tls/key.pem` 文件，以便使用正确的身份重新生成自签名证书。