# Getting Started

Quick start guide for mibee-eye (MiBee Eye) — laptop surveillance agent built in Rust.

## Prerequisites

**Rust:** Requires Rust 1.85+ with Cargo.

**Linux System Dependencies:**

```bash
# Install required system packages
sudo apt install libv4l-dev libasound2-dev libclang-dev

# Add user to video group for webcam access
sudo usermod -aG video $USER

# Log out and back in for group change to take effect
```

**Windows:** Secondary support. Install via vcpkg when available.

## Build

Build the project using Cargo:

```bash
# Build in debug mode (development)
cargo build

# Build in release mode (production)
cargo build --release
```

The release binaries will be available at `target/release/`:
`mibee-eye` (full form), `mibee-cam` (visual AI + streaming),
`mibee-mic` (audio pickup), `mibee-talk` (intercom). Pick the form that
matches the host's role; every form serves the same Web UI and API.

Default ports:
- Web UI: 8443 (HTTPS with self-signed TLS)
- RTSP Server: 8554
- RTMP Push: 1935 (outbound to external ingest)

## First Run

**On first execution**, the server runs in setup mode. You must create an admin user before accessing the web UI.

Start the server:

```bash
cargo run -- --config config.toml
```

The server will detect this is the first run and allow access to the setup endpoint without authentication.

Create your admin user using curl:

```bash
curl -X POST https://localhost:8443/api/auth/setup \
  -H "Content-Type: application/json" \
  -d '{"username":"admin","password":"yourpass123"}'
```

**Requirements:**
- Username cannot be empty
- Password must be at least 8 characters

On success, the server:
- Creates the admin user in the SQLite database
- Generates a self-signed TLS certificate for HTTPS
- Starts the web server on port 8443

## Access Web UI

Open your browser and navigate to:

```
https://localhost:8443
```

**Important:** You will see a security warning about the self-signed certificate. This is expected in development. Click "Advanced" and "Proceed to localhost" to continue.

After setup, the web UI requires authentication using session-based cookies.

## Web UI Features

The web UI provides:

- **Bilingual support**: zh-CN / en-US language toggle (persisted to user settings)
- **Day/night theme**: System-preference auto-detect, manual toggle (persisted to user settings)
- **Camera management**: Add, remove, and configure local webcam capture
- **Stream controls**: Start/stop streams, monitor status, main/sub stream quality
- **Live AI overlays**: detection boxes (NanoDet) and your saved zones drawn over the picture
- **Zones editor**: draw intrusion / tripwire regions directly on the frozen frame
- **Chat panel**: talk to the on-device LLM (capability `chat`)
- **Alarm toasts**: visual / sound / zone alarms, VLM descriptions and voice transcripts in real time
- **Protocol configuration**: RTSP, RTMP push, ONVIF, GB28181 (all default-OFF, hot-toggled via UI)
- **Local recording**: MP4 segment archive with auto-prune
- **Settings**: Unified config editor, device enumeration, and more

The UI is capability-gated: it shows exactly what this build and its model
files support. See the [User Guide](user-guide.md) for a walkthrough of
every panel and the AI features.

## Product Scope

**mibee-eye is a LOCAL-ONLY capture agent:**

- Captures physically-attached devices (USB webcam, built-in/USB mic) from THIS machine only
- Does NOT discover or connect to remote network cameras
- Does NOT act as an NVR or video management server
- Outbound streaming (RTSP server, RTMP push, ONVIF device, GB28181 device) is available but default-OFF

For authoritative product positioning, see [POSITIONING.md](../POSITIONING.md).

## Next Steps

Continue with these resources:

- [User Guide](user-guide.md) - Using the web UI and the AI features day to day
- [Installation Guide](installation.md) - Detailed installation instructions
- [Configuration Guide](configuration.md) - Advanced configuration options
- [API Reference](api.md) - Complete API documentation

For development and testing:
- `cargo test` - Run all tests
- `cargo ci-clippy` - Run clippy linter
- `cargo fmt-check` - Check formatting

Happy monitoring!

---
*MiBee Eye (MiBee Eye) — Professional laptop surveillance agent built in Rust.*