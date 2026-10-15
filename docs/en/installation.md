# Installation Guide

This guide covers installation and deployment for mibee-eye (MiBee Eye), a professional laptop surveillance agent built in Rust.

## System Requirements

### Operating Systems
- **Linux (first-class)** - Full support with V4L2/ALSA, optimized for resource usage
- **Windows (secondary-class)** - Limited support with MSMF/WASAPI, not feature-complete
- **macOS** - Not in scope

### Software Requirements
- **Rust 1.88+** - MSRV for Rust 2024 edition
- **RAM**: Target <200MB RAM during operation
- **CPU**: Target <5% CPU idle usage
- **Disk**: ~50MB for binary + dependencies

### Hardware Requirements
- **Webcam**: V4L2-compatible (Linux) or Media Foundation (Windows)
- **Microphone**: ALSA-compatible (Linux) or WASAPI (Windows)
- **Network**: TCP/UDP for streaming protocols

## Linux Installation

### System Dependencies

Install required system packages:

```bash
sudo apt install libv4l-dev libasound2-dev libclang-dev
```

### User Permissions

Add your user to the video group for webcam access:

```bash
sudo usermod -aG video $USER
```

**Important**: Log out and back in for the group change to take effect.

### Privileged Ports

By default, mibee-eye uses:

- Web UI: 8443 (TLS)

- RTSP: 8554  

- RTMP Push: 1935 (outbound to external ingest server; no RTMP ingest server exists)

- Web UI: 8443 (TLS)
- RTSP: 8554  
- RTMP: 1935

These ports avoid the privileged range (<1024). If you need to use lower ports, set capability binding:

```bash
setcap 'cap_net_bind_service=+ep' ./target/release/mibee-eye
```

## Windows Installation

Windows support is secondary-class and not feature-complete. This installation is for development/testing purposes only.

### System Dependencies

Install required components:

1. **Visual Studio Build Tools** - C++ build tools for nokhwa compilation
2. **Media Foundation Runtime** - Built into Windows 10/11
3. **Windows SDK** - Required for WASAPI audio support

### Installation Steps

1. Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
2. Enable "C++ build tools" during installation
3. Install Rust via [rustup](https://rustup.rs/)

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install Windows dependencies (developer setup)
choco install visualstudio2022buildtools visualstudio2022-workload-vctools
```

## Building from Source

### Prerequisites

Ensure you have Rust 1.88+ installed:

```bash
rustc --version  # Should be >= 1.88
```

### Build Commands

```bash
# Clone and enter the repository
git clone https://github.com/xiqing85/mibee-eye-notebook.git
cd mibee-eye

# Debug build (development)
cargo build

# Release build (production)
cargo build --release

# Run tests
cargo test

# Lint code
cargo ci-clippy

# Format check
cargo fmt-check
```

### Build Artifacts

The binary will be at:
- Debug: `target/debug/mibee-eye`
- Release: `target/release/mibee-eye`

## Running

### CLI Arguments

The binary accepts these arguments:

```bash
# Run with default config
cargo run -- --config config.toml

# Specify custom config and database path
cargo run --release -- --config config.local.toml --db-path /path/to/database.db

# Reset password (does not start server)
cargo run -- --reset-password
```

### Command Line Options

- `--config, -c`: Path to config file (default: `config.toml`)
- `--db-path, -d`: Path to SQLite database (default: `mibee_eye.db`)
- `--reset-password`: Reset password for a user (prompts for credentials)
- `--selftest-audio <wav>` / `--selftest-ocr <image>` / `--selftest-voice <wav>` / `--selftest-llm <prompt>` / `--selftest-tts <text>` / `--selftest-vlm <jpeg>`: offline one-shot self-tests — run the corresponding engine's full pipeline once and print a JSON result (see the [user guide](user-guide.md#verifying-a-feature-offline-self-tests))

## AI Model Assets

The AI engines load their models from `models/` at startup. These files are
**not tracked in git** — download them at deploy time. Without them the
service runs fine with the corresponding capabilities simply off (fail-open).

[`models/README.md`](../../models/README.md) is the authoritative table of
sources, sizes and licenses. Summary:

| Path | Purpose | Size |
|------|---------|------|
| `models/nanodet-m.onnx` | visual object detection | ~10 MB |
| `models/audio/yamnet.onnx`, `models/audio/silero_vad.onnx` | sound events, voice presence | ~18 MB |
| `models/voice/kws/*`, `models/voice/paraformer/*` | wake word + ASR (sherpa-onnx releases) | ~250 MB |
| `models/voice/melo/*` | TTS voice (k2-fsa `vits-melo-tts-zh_en`) | ~165 MB |
| `models/llm/qwen3-0.6b-q8_0.gguf` | local chat LLM (ModelScope `Qwen/Qwen3-0.6B-GGUF`) | ~640 MB |
| `models/vlm/*.gguf` (text + mmproj) | alarm-frame descriptions (ModelScope `Qwen/Qwen3-VL-2B-Instruct-GGUF`) | ~1.5 GB |
| `models/ocr/*.onnx` | PP-OCR detector + recognizer | ~21 MB |

Build-time notes:

- The `voice` feature needs the sherpa-onnx **static libraries** — download
  the k2-fsa release `sherpa-onnx-v1.13.8-linux-x64-static-lib.tar.bz2` and
  build with `SHERPA_ONNX_LIB_DIR=<extracted>/lib`.
- The `llm`/`vlm` features require an **AVX2-class CPU**; Sandy Bridge and
  older hosts must build voice-only (`cargo build --release --features voice`).

## Configuration File

### Local Configuration

Copy and customize the default configuration:

```bash
cp config.toml config.local.toml
```

Edit `config.local.toml` for local overrides. This file is gitignored and won't be committed.

### Default Configuration

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

### Configuration Options

- **web**: Web UI settings (port, host, advertised_host)
- **rtsp**: RTSP server configuration (outbound server mode only)
- **rtmp_push**: RTMP push client (outbound push to external ingest, NOT ingest server)
- **capture**: Video/audio device paths (local-only; no remote camera discovery)
- **security**: Rate limiting configuration (non-poisoning Mutex)
- **observability**: Logging and metrics settings (OTLP tracing, optional Loki remote log shipping)
- **onvif**: ONVIF device endpoint configuration (optional)
- **gb28181**: GB/T 28181 device registration (optional)
- **recording**: Local MP4 segment recording with auto-prune
- **database**: SQLite database path (XDG-compliant default)

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

### Configuration Options

- **web**: Web UI settings (port, host)
- **rtsp**: RTSP server configuration
- **rtmp**: RTMP ingest settings
- **capture**: Video/audio device paths
- **security**: Rate limiting configuration
- **observability**: Logging and metrics settings

## TLS Certificates

### Development Setup

For development, mibee-eye automatically generates self-signed TLS certificates on first run:

```bash
# First run generates certificates
./target/release/mibee-eye --config config.local.toml

# Certificates are saved to:
# - tls/cert.pem
# - tls/key.pem
```

The certificates support hot-reload on file mtime change (update cert.pem/key.pem files and server reloads automatically).

Development certificates use:

- Subject: CN=mibee-eye

- SAN: mibee-eye.local

- Validity: ~30 days

For development, mibee-eye automatically generates self-signed TLS certificates on first run:

```bash
# First run generates certificates
./target/release/mibee-eye --config config.local.toml

# Certificates are saved to:
# - tls/cert.pem
# - tls/key.pem
```

The certificates use:
- Subject: CN=mibee-eye
- SAN: mibee-eye.local
- Validity: ~30 days

### Production Setup

For production, use certificates from a trusted CA:

```bash
# Using Let's Encrypt with certbot
sudo apt install certbot
sudo certbot certonly --standalone -d your-domain.com

# Copy certificates to appropriate location
sudo cp /etc/letsencrypt/live/your-domain.com/fullchain.pem ./tls/cert.pem
sudo cp /etc/letsencrypt/live/your-domain.com/privkey.pem ./tls/key.pem

# Set appropriate permissions
sudo chown $USER:$USER ./tls/cert.pem ./tls/key.pem
chmod 600 ./tls/cert.pem ./tls/key.pem
```

### Certificate Management

- **Auto-renewal**: Set up cron job for Let's Encrypt certificates
- **Rotation**: Replace certificates and restart the service
- **Backup**: Keep certificate backups in secure location

## Deployment

### Systemd Service

Create a systemd service file at `/etc/systemd/system/mibee-eye.service`:

```ini
[Unit]
Description=MiBee Eye Surveillance Agent
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

# Security settings
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true

# Resource limits
LimitNOFILE=65536
MemoryMax=256M

[Install]
WantedBy=multi-user.target
```

### Service Management

```bash
# Enable and start the service
sudo systemctl enable mibee-eye
sudo systemctl start mibee-eye

# Check status
sudo systemctl status mibee-eye

# View logs
sudo journalctl -u mibee-eye -f

# Restart service
sudo systemctl restart mibee-eye
```

### User Setup

Create dedicated user for the service:

```bash
sudo useradd -r -s /bin/false mibee
sudo mkdir -p /opt/mibee-eye
sudo chown mibee:mibee /opt/mibee-eye
```

### Reverse Proxy Configuration

#### Nginx with TLS Termination

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

    # Security headers
    add_header X-Frame-Options DENY;
    add_header X-Content-Type-Options nosniff;
    add_header X-XSS-Protection "1; mode=block";
    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;

    # Proxy to mibee-eye
    location / {
        proxy_pass https://localhost:8443;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        
        # Timeouts
        proxy_connect_timeout 30s;
        proxy_send_timeout 30s;
        proxy_read_timeout 30s;
    }
}
```

### Podman/Docker Considerations

For containerized deployment:

```bash
# Build with Podman
podman build -t mibee-eye .

# Run with volume mounts
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
# Note: RTMP push is outbound; no port mapping needed unless you're running an external RTMP ingest server
```
  -p 1935:1935 \
  mibee-eye
```

## Troubleshooting

### Common Issues

#### Permission Denied (Webcam Access)

**Symptom**: "Permission denied" when accessing webcam

**Solution**: Verify user is in video group

```bash
# Check group membership
groups $USER

# If not in video group, add and re-login
sudo usermod -aG video $USER
# Log out and back in
```

#### Device Not Found

**Symptom**: Camera or microphone device not found

**Solution**: Check device paths and permissions

```bash
# List video devices
ls /dev/video*

# List audio devices
arecord -l

# Check device permissions
ls -la /dev/video0
```

#### Port in Use

**Symptom**: Port already in use error

**Solution**: Check port usage and adjust configuration

```bash
# Check port usage
sudo netstat -tulpn | grep 8443
sudo netstat -tulpn | grep 8554

# Kill conflicting processes
sudo fuser -k 8443/tcp
```

#### TLS Certificate Issues

**Symptom**: TLS handshake failures or certificate errors

**Solution**: Regenerate or replace certificates

```bash
# Remove existing certificates
rm -f tls/cert.pem tls/key.pem

# Restart service to regenerate
sudo systemctl restart mibee-eye
```

#### Database Connection Errors

**Symptom**: SQLite connection failed errors

**Solution**: Check database path and permissions

```bash
# Check database file
ls -la mibee_eye.db

# Check permissions
chmod 640 mibee_eye.db
```

#### Resource Limits Hit

**Symptom**: "Too many streams" or memory limit errors

**Solution**: Adjust resource limits or reduce concurrent streams

```bash
# Check current resource usage
systemctl show mibee-eye --property=MemoryCurrent,LimitNOFILE

# Adjust systemd service limits if needed
```

### Performance Issues

#### High CPU Usage

**Solution**: Check streaming configuration and resource limits

```bash
# Monitor resource usage
top -p $(pidof mibee-eye)
htop -p $(pidof mibee-eye)

# Check streaming logs for errors
sudo journalctl -u mibee-eye | grep -i error
```

#### Memory Usage High

**Solution**: Check concurrent streams and buffer configuration

```bash
# Check memory usage
ps aux | grep mibee-eye

# Check streaming configuration
grep -i buffer config.local.toml
```

### Debug Mode

Enable debug logging for troubleshooting:

```bash
# Set RUST_LOG environment variable
RUST_LOG=debug ./target/release/mibee-eye --config config.local.toml

# Or set in systemd service file
Environment="RUST_LOG=debug"
```

### System Information

For bug reports, collect system information:

```bash
# System and Rust version
rustc --version
cargo --version
uname -a

# Package versions (Ubuntu/Debian)
dpkg -l | grep -E "(libv4l|libasound|libclang)"

# Kernel modules
lsmod | grep -E "(v4l2|snd)"

# Network information
ip a
ss -tulpn | grep -E "(8443|8554|1935)"
```

### Getting Help

If you continue to have issues:

1. Check the [GitHub Issues](https://github.com/xiqing85/mibee-eye-notebook/issues)
2. Review [full documentation](https://github.com/xiqing85/mibee-eye-notebook/docs/en/)
3. Create a detailed issue with:
   - System information (OS, version)
   - Exact error messages
   - Configuration files (redact sensitive data)
   - Steps to reproduce
   - Expected vs actual behavior

> **Migration Note (Rebrand)**: The dev TLS certificate's CommonName and SubjectAltName have changed from `notebook-cam`/`notebook-cam.local` to `mibee-eye`/`mibee-eye.local`. Delete the old `tls/cert.pem` and `tls/key.pem` files before starting the server so a new self-signed certificate is generated with the correct identity.