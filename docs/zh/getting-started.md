# 快速入门

mibee-eye（MiBee Eye）快速入门指南 — 基于 Rust 构建的专业笔记本监控代理。

## 前置条件

**Rust：** 需要 Rust 1.85+ 和 Cargo。

**Linux 系统依赖：**

```bash
# 安装必需的系统包
sudo apt install libv4l-dev libasound2-dev libclang-dev

# 将用户添加到 video 组以获取摄像头权限
sudo usermod -aG video $USER

# 注销后重新登录以使组变更生效
```

**Windows：** 次要支持。通过 vcpkg 安装（当可用时）。

## 构建

使用 Cargo 构建项目：

```bash
# 调试模式构建（开发）
cargo build

# 发布模式构建（生产）
cargo build --release
```

发布版二进制文件将位于 `target/release/`：`mibee-eye`（全功能）、
`mibee-cam`（视觉 AI + 推流）、`mibee-mic`（拾音）、`mibee-talk`（对讲）。
按主机角色选择形态；所有形态共享同一 Web 界面与 API。

默认端口：
- Web UI：8443（HTTPS 使用自签名 TLS）
- RTSP 服务端：8554
- RTMP 推送：1935（推送到外部推流服务器的出站）

## 首次运行

**在首次执行时**，服务器以设置模式运行。您必须创建管理员用户才能访问 Web 界面。

启动服务器：

```bash
cargo run -- --config config.toml
```

服务器会检测这是首次运行，并允许无需认证即可访问设置端点。

使用 curl 创建管理员用户：

```bash
curl -X POST https://localhost:8443/api/auth/setup \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"yourpass123"}'
```

**要求：**
- 用户名不能为空
- 密码必须至少 8 个字符

成功后，服务器：
- 在 SQLite 数据库中创建管理员用户
- 为 HTTPS 生成自签名 TLS 证书
- 在 8443 端口启动 Web 服务器

## 访问 Web 界面

打开浏览器并导航到：

```
https://localhost:8443
```

**重要：** 您会看到关于自签名证书的安全警告。这在开发中是预期的。点击「高级」和「继续前往 localhost」。

设置完成后，Web 界面需要使用基于会话的 cookie 进行身份认证。

## Web 界面功能

Web 界面提供：

- **双语支持**：zh-CN / en-US 语言切换（保存到用户设置）
- **日间/夜间主题**：系统偏好自动检测，手动切换（保存到用户设置）
- **摄像头管理**：添加、删除和配置本地摄像头捕获
- **流控制**：启动/停止流，监控状态，主/子码流画质切换
- **实时 AI 叠加**：检测框（NanoDet）与已保存区域直接画在画面上
- **区域编辑器**：在冻结帧上直接绘制入侵 / 越线区域
- **对话面板**：与端侧 LLM 对话（能力位 `chat`）
- **告警 toast**：视觉 / 声音 / 区域告警、VLM 描述与语音转写实时提示
- **协议配置**：RTSP、RTMP 推送、ONVIF、GB28181（均默认关闭，UI 热切换）
- **本地录制**：MP4 段存档，自动修剪
- **设置**：统一配置编辑器、设备枚举等

界面按能力位渲染：显示的正是当前构建与模型文件支持的功能全集。
各面板与 AI 功能的使用方法见[用户手册](user-guide.md)。

## 产品范围

**mibee-eye 是仅本地捕获代理：**

- 仅从此机器捕获物理连接的设备（USB 摄像头、内置/USB 麦克风）
- 不会发现或连接到远程网络摄像头
- 不充当 NVR 或视频管理服务器
- 出站流媒体（RTSP 服务器、RTMP 推送、ONVIF 设备、GB28181 设备）可用但默认关闭

有关权威产品定位，请参阅 [POSITIONING.md](../POSITIONING.md)。

## 下一步

继续使用以下资源：

- [用户手册](user-guide.md) - Web 界面与 AI 功能的日常使用
- [安装指南](installation.md) - 详细的安装说明
- [配置说明](configuration.md) - 高级配置选项
- [API 参考](api.md) - 完整的 API 文档

用于开发和测试：
- `cargo test` - 运行所有测试
- `cargo ci-clippy` - 运行 clippy 代码检查
- `cargo fmt-check` - 检查格式

祝监控愉快！

---
*MiBee Eye（MiBee Eye）— 基于 Rust 构建的专业笔记本监控代理。*