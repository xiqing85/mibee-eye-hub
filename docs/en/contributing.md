# Contributing to mibee-eye

Thank you for your interest in contributing to mibee-eye! This guide covers everything you need to know to get started and make meaningful contributions to this Rust-based laptop surveillance agent.

## Getting Started (Development)

### Prerequisites

- **Rust 1.85+** (MSRV) - Install via [rustup](https://rustup.rs/)
- **System dependencies** (Linux):

```bash
# Install required packages
sudo apt install libv4l-dev libasound2-dev libclang-dev

# Add user to video group for webcam access
sudo usermod -aG video $USER
# Log out and back in for group change to take effect
```

### Clone and Build

```bash
# Clone the repository
git clone https://github.com/xiqing85/mibee-eye-notebook.git
cd mibee-eye

# Build the project
cargo build

# Run tests
cargo test

# Build release version
cargo build --release
```

### Development Workflow

1. **Configure your development environment**:

```bash
# Copy the default config for local development
cp config.toml config.local.toml

# Edit config.local.toml for your development setup
# This file is gitignored and won't be committed
```

2. **Run the application**:

```bash
# Run with your local config
cargo run -- --config config.local.toml

# Run with release build for performance testing
cargo run --release -- --config config.local.toml

# Reset password (CLI utility)
cargo run -- --reset-password
```

3. **Use workspace aliases for development**:

```bash
# Lint with strict clippy rules
cargo ci-clippy

# Run tests with verbose output
cargo test-verbose

# Check formatting
cargo fmt-check

# Format code
cargo fmt
```

## Project Structure

```
mibee-eye/
├─ src/                    # Binary entry point, config, types
│  ├─ main.rs             # Application entry point
│  ├─ config.rs           # Configuration management with TOML
│  └─ types.rs            # Core domain types (CameraId, StreamId, CameraType)
├─ crates/                # Workspace crates
│  ├─ protocols/         # Media plane: RTSP server, RTMP push, RTP, H.264, RTCP (signaling lives in the shared protocol libraries)
│  ├─ streaming/         # StreamHub fan-out, CaptureSource, Output adapters
│  ├─ web/              # Axum REST API + embedded SPA + TLS + i18n + ProtocolRuntime
│  ├─ security/         # Auth, TLS, rate limiting, CSRF, password hashing
│  ├─ capture/           # Video (nokhwa) + Audio (cpal) + hot-plug (udev)
│  └─ observability/    # tracing + OTel + Prometheus + Loki log shipping
├─ migrations/           # SQLite schema (4 migrations: cameras, settings, protocol_configs, users/sessions)
└─ tls/                  # Development TLS certificates
```

### Core Concepts

- **Camera Types**: `Usb`, `Rtsp`, `Onvif`, `Gb28181`, `Rtmp`
- **Media Pipeline**: Sources → BufferPool → StreamHub → Outputs
- **Resource Management**: Semaphore-guarded concurrency (max 16 streams)
- **Security**: TLS everywhere, session-based authentication
- **Observability**: Structured logging, metrics, distributed tracing

For detailed architecture documentation, see [docs/en/architecture.md](architecture.md).

## Code Conventions

### Rust Edition and Version

- **Rust 2024 edition** with MSRV 1.85
- Prefer `async fn` over `fn -> impl Future`
- Use `tokio` for all async operations
- Never use blocking network I/O

### Testing Conventions

#### Unit Tests

Place unit tests at the bottom of each file using the standard pattern:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_example_logic() {
        // Test implementation
        assert_eq!(2 + 2, 4);
    }

    #[tokio::test]
    async fn test_async_behavior() {
        // Async test implementation
        let result = some_async_function().await;
        assert!(result.is_ok());
    }
}
```

#### Mock Patterns

Use `MockSource` and `MockOutput` for testing protocol adapters:

```rust
// From crates/streaming/src/source.rs
pub(crate) struct MockSource {
    frames: Vec<MediaFrame>,
    started: bool,
    index: usize,
}

impl Source for MockSource {
    fn start(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.started = true;
            Ok(())
        })
    }
    
    fn next_frame(&mut self) -> Pin<Box<dyn Future<Output = Result<MediaFrame>> + Send + '_>> {
        Box::pin(async move {
            if !self.started {
                anyhow::bail!("MockSource not started");
            }
            if self.index >= self.frames.len() {
                anyhow::bail!("MockSource exhausted");
            }
            let frame = self.frames[self.index].clone();
            self.index += 1;
            Ok(frame)
        })
    }
    
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.started = false;
            Ok(())
        })
    }
}
```

#### Database Tests

For database-related code, use in-memory SQLite with embedded migrations:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Apply migrations manually for testing
        conn.execute_batch(include_str!("../../../migrations/001_initial.sql")).unwrap();
        conn
    }

    #[test]
    fn test_camera_crud() {
        let conn = test_db();
        // Test database operations
    }
}
```

### Serialization Conventions

All public types must implement `Serialize`/`Deserialize` with roundtrip tests:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CameraId(pub String);

#[test]
fn test_camera_id_serde_roundtrip() {
    let id = CameraId::new();
    let json = serde_json::to_string(&id).unwrap();
    let back: CameraId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, id);
}
```

### Formatting and Linting

Always run these commands before committing:

```bash
# Format code
cargo fmt

# Check formatting
cargo fmt-check

# Run strict clippy linter
cargo ci-clippy
```

## Adding a Protocol

### Overview

To add a new protocol support (e.g., WebRTC, SRT, or a custom protocol), follow these steps:

### Step 1: Add Protocol Module

1. Create a new module in `crates/protocols/src/`:

```rust
// crates/protocols/src/new_protocol.rs
pub struct NewProtocolClient {
    // Client implementation
}

impl NewProtocolClient {
    pub fn new(config: &NewProtocolConfig) -> Result<Self> {
        // Constructor logic
    }
    
    pub async fn connect(&mut self) -> Result<()> {
        // Connection logic
    }
    
    pub async fn receive_frame(&mut self) -> Result<MediaFrame> {
        // Frame reception logic
    }
}
```

2. Export the module in `crates/protocols/src/lib.rs`:

```rust
// crates/protocols/src/lib.rs
pub mod new_protocol;
```

### Step 2: Create Source and Output Adapters

Add source and output adapters in `crates/streaming/src/`:

```rust
// crates/streaming/src/source.rs - add NewProtocolSource
pub struct NewProtocolSource {
    client: protocols::new_protocol::NewProtocolClient,
    started: bool,
}

impl Source for NewProtocolSource {
    fn start(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.client.connect().await?;
            self.started = true;
            Ok(())
        })
    }
    
    fn next_frame(&mut self) -> Pin<Box<dyn Future<Output = Result<MediaFrame>> + Send + '_>> {
        Box::pin(async move {
            if !self.started {
                anyhow::bail!("NewProtocolSource not started");
            }
            self.client.receive_frame().await
        })
    }
    
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            // Cleanup logic
            self.started = false;
            Ok(())
        })
    }
}
```

### Step 3: Add CameraType Variant

Update `src/types.rs` to include your new camera type:

```rust
// src/types.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraType {
    Usb,
    Rtsp,
    Onvif,
    Gb28181,
    Rtmp,
    NewProtocol,  // Add your new type here
}
```

Don't forget to add a corresponding test:

```rust
#[test]
fn test_camera_type_new_protocol() {
    let json = serde_json::to_string(&CameraType::NewProtocol).unwrap();
    assert_eq!(json, "\"new_protocol\"");
    let back: CameraType = serde_json::from_str(&json).unwrap();
    assert_eq!(back, CameraType::NewProtocol);
}
```

### Step 4: Wire Up API Routes

Add endpoints in `crates/web/src/routes/`:

```rust
// crates/web/src/routes/new_protocol.rs
use axum::{extract::Query, Json};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct DiscoverQuery {
    timeout: Option<u64>,
}

#[derive(Serialize)]
pub struct DiscoveryResult {
    devices: Vec<NewProtocolDeviceInfo>,
}

pub async fn discover_new_protocol(
    Query(params): Query<DiscoverQuery>,
) -> Result<Json<DiscoveryResult>, AppError> {
    // Discovery logic
    let devices = protocols::new_protocol::discover_devices(
        params.timeout.unwrap_or(5)
    ).await?;
    
    Ok(Json(DiscoveryResult { devices }))
}
```

Register the route in `crates/web/src/routes/mod.rs`:

```rust
// crates/web/src/routes/mod.rs
pub mod new_protocol;

// In all_routes() function
pub fn all_routes() -> Router<AppState> {
    Router::new()
        // ... existing routes
        .route("/api/new_protocol/discover", get(new_protocol::discover_new_protocol))
}
```

### Step 5: Update Configuration

Add protocol-specific configuration options to your config structure and update the default config.

### Reference Implementations

Look at existing protocols for reference:

- **ONVIF**: `onvif-device-rs` crate (signaling via the shared protocol library; runtime wiring in `crates/web/src/protocol_runtime.rs`)
- **GB28181**: `gb28181-rs` crate (signaling via the shared protocol library; the `Gb28181Output` adapter in `crates/streaming` bridges it to the StreamHub)
- **RTSP**: `crates/protocols/src/rtsp_server/` - hand-written RTSP server (RFC 2326, Digest auth, RTP interleaved)
- **RTMP**: `crates/protocols/src/rtmp/` - hand-written RTMP push client (handshake + connect + publish)
- **H.264**: `crates/protocols/src/h264.rs` - hand-written NAL unit parser
- **RTP**: `crates/protocols/src/rtp.rs` - hand-written RTP packetizer/parser

## Adding a Camera Type

### CameraType Enum

Camera types are defined in `src/types.rs` and represent the different sources mibee-eye can handle:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraType {
    Usb,           // Local webcam via V4L2/MSMF
    Rtsp,          // Network RTSP camera
    Onvif,         // ONVIF-compatible IP camera
    Gb28181,       // GB/T 28181 compliant camera
    Rtmp,          // RTMP push source
    // Add new types here
}
```

### Adding a New Camera Type

1. **Add variant to CameraType enum** (as shown above)

2. **Create Source adapter** implementing the `Source` trait:

```rust
// crates/streaming/src/source.rs
pub struct YourCameraSource {
    // Camera-specific configuration
    config: YourCameraConfig,
    // Camera connection handle
    connection: Option<YourCameraConnection>,
    started: bool,
}

impl Source for YourCameraSource {
    fn start(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            // Connect to camera
            let conn = YourCameraConnection::connect(&self.config).await?;
            self.connection = Some(conn);
            self.started = true;
            Ok(())
        })
    }
    
    fn next_frame(&mut self) -> Pin<Box<dyn Future<Output = Result<MediaFrame>> + Send + '_>> {
        Box::pin(async move {
            if !self.started {
                anyhow::bail!("YourCameraSource not started");
            }
            let conn = self.connection.as_mut()
                .ok_or_else(|| anyhow::anyhow!("No connection"))?;
            conn.receive_frame().await
        })
    }
    
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            self.connection = None;
            self.started = false;
            Ok(())
        })
    }
}
```

3. **Register in API routes**:

Add CRUD endpoints for your camera type in `crates/web/src/routes/cameras.rs`.

4. **Update configuration schema**:

Add your camera-specific configuration options to the appropriate config struct.

## Testing Strategy

### Unit Tests

Every module should have comprehensive unit tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_function_logic() {
        // Test core logic with known inputs
    }

    #[tokio::test]
    async fn test_async_behavior() {
        // Test async operations
    }
}
```

### Integration Tests

#### Protocol Adapter Testing

Test protocol adapters with both real cameras (when available) and mock implementations:

```rust
#[tokio::test]
async fn test_rtsp_integration() {
    let mut source = RtspSource::new("rtsp://test-camera:554/stream", "user", "pass");
    source.start().await.unwrap();
    
    let frame = source.next_frame().await.unwrap();
    assert!(matches!(frame, MediaFrame::Video { .. }));
    
    source.stop().await.unwrap();
}
```

#### Resource Exhaustion Tests

Test the resource controller limits:

```rust
#[tokio::test]
async fn test_max_concurrent_streams() {
    // Test that the semaphore correctly limits concurrent streams
    // Try to start more than 16 streams and verify failure
}
```

### Test Running

- **All tests**: `cargo test`
- **Specific crate**: `cargo test -p protocols`
- **Verbose output**: `cargo test-verbose` (shows detailed test output)
- **Filter tests**: `cargo test test_name`
- **Run failing tests only**: `cargo test --lib -- --test-threads=1`

### Test Coverage

Aim for high test coverage, especially for:
- Protocol parsing logic
- Error handling paths
- Resource management
- Database operations
- Authentication flows

## Commit Conventions

### Commit Message Format

Use the conventional commit format:

```
type(scope): description
```

### Commit Types

- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `refactor`: Code changes that neither fix bugs nor add features
- `test`: Adding or fixing tests
- `chore`: Build process or auxiliary tool changes

### Examples

```
feat(protocols): add WebRTC camera support
fix(streaming): handle connection timeouts properly
docs(contributing): update development setup instructions
refactor(config): simplify configuration loading logic
test(rtsp): add integration tests for error scenarios
chore(ci): update GitHub Actions workflow
```

### Commit Guidelines

1. **Keep commits focused**: Each commit should address a single logical change
2. **Write clear messages**: Describe what changed and why
3. **Include breaking change notes**: Use `BREAKING CHANGE:` footer for incompatible changes
4. **Reference issues**: Use `Closes #123` to link to GitHub issues
5. **Test before committing**: Ensure all tests pass and `cargo ci-clippy` passes

## Pull Request Process

### Before Opening a PR

1. **Create a feature branch** from the appropriate base branch
2. **Run the full test suite**:
   ```bash
   cargo test
   cargo ci-clippy
   cargo fmt-check
   ```
3. **Update documentation** if your changes affect APIs or user-facing features
4. **Add tests** for any new functionality or bug fixes

### PR Description Template

Use this template for your pull request:

```markdown
## Description
Brief description of the changes made.

## Changes Made
- [ ] Added new protocol support
- [ ] Fixed bug in X
- [ ] Updated documentation
- [ ] Added tests for Y

## Testing
- [ ] Unit tests pass
- [ ] Integration tests pass
- [ ] Manual testing completed
- [ ] Performance impact assessed (if applicable)

## Breaking Changes
- [ ] None
- [ ] List breaking changes here

## Related Issues
Closes #123
Related to #456
```

### PR Review Process

1. **Automated checks**: CI will run tests and linting
2. **Code review**: Wait for at least one maintainer review
3. **Requested changes**: Address all review comments
4. **Final approval**: Get approval from a maintainer
5. **Merge**: Maintainer will merge after CI passes

### After Merging

- Delete your feature branch
- Update your local main branch
- Continue working on the next feature

## Key Gotchas

### 1. nokhwa on Linux

**Issue**: nokhwa requires specific system dependencies and user permissions.

**Solution**:
```bash
# Install required packages
sudo apt install libv4l-dev input-native

# Add user to video group
sudo usermod -aG video $USER
# Log out and back in for group change to take effect
```

**Never assume root/sudo** - output exact commands for privileged operations.

### 2. cpal ALSA Audio Callbacks

**Issue**: Audio callbacks must never block - use non-blocking operations.

**Anti-pattern** (DO NOT DO):
```rust
// WRONG: Blocking in callback
fn audio_callback(data: &mut [f32]) {
    let result = some_blocking_operation(); // This will cause audio glitches
    // ...
}
```

**Correct pattern**:
```rust
// CORRECT: Use channels for non-blocking communication
let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

fn audio_callback(data: &mut [f32]) {
    if let Err(e) = tx.try_send(data.to_vec()) {
        tracing::warn!("Audio channel full: {}", e);
    }
}
```

**Issue**: GB28181 device mode registers WITH the platform via SIP REGISTER; the platform sends INVITE, and this device pushes RTP (encapsulated in MPEG-PS) back to the platform.

**Solution**: The `gb28181-rs` protocol library handles REGISTER/INVITE/BYE signaling and pushes H.264 NAL units encapsulated in MPEG-PS over RTP/UDP. The `Gb28181Output` adapter is dynamically attached to the camera's `StreamHub` on INVITE and detached on BYE.

**Browser compatibility**: H.265 is not universally supported in browsers - always fallback to H.264.

**Browser compatibility**: H.265 is not universally supported in browsers - always fallback to H.264.

**Issue**: ONVIF discovery uses UDP multicast port 3702 and may require raw socket access.

**Requirements**:
```bash
# May need CAP_NET_RAW capability or root access
# The WS-Discovery server from the onvif-device-rs library
# handles this automatically
```

**Note**: ONVIF is entirely hand-written (no external wrapper library). The device endpoint exposes device information via SOAP so external NVRs can discover this machine via WS-Discovery.

### 5. WebRTC Rust Ecosystem

**Issue**: Rust WebRTC ecosystem is immature for production use.

**Solution**: Proxy through MiBee NVR WHEP endpoint:

```rust
// Instead of direct WebRTC implementation
let whep_endpoint = "https://mibee-nvr.example.com/whep";
// Use WHEP protocol via MiBee NVR
```

### 6. H.265 Browser Compatibility

**Issue**: H.265 (HEVC) is not universally supported in web browsers.

**Solution**: Always provide H.264 fallback:

```rust
// In streaming endpoints, check client support
if client_supports_h265 && source_has_h265 {
    stream_h265();
} else {
    // Convert/transcode to H.264
    stream_h264();
}
```

### 7. Privileged Ports (< 1024)

**Issue**: Binding to ports < 1024 requires root privileges or special capabilities.

**Solutions**:
- **Use high ports**: Web UI on 8443, RTSP on 8554, RTMP on 1935
- **Or use setcap**: `sudo setcap 'cap_net_bind_service=+ep' /path/to/binary`

## Development Commands Reference

### Build Commands

```bash
cargo build                                # Debug build
cargo build --release                      # Release build
cargo run -- --config config.toml          # Run with config
cargo run -- --reset-password              # Password reset CLI
```

### Testing Commands

```bash
cargo test                                 # Run all tests
cargo test -p protocols                    # Test specific crate
cargo test-verbose                         # Tests with verbose output
cargo test test_name                      # Run specific test
```

### Code Quality Commands

```bash
cargo ci-clippy                            # Strict clippy linting (-D warnings)
cargo fmt-check                            # Check formatting
cargo fmt                                  # Format code
```

### Workspace Aliases

The workspace defines these convenient aliases:

- `cargo ci-clippy` - Runs clippy with `-D warnings` (strict linting)
- `cargo fmt-check` - Checks if code is properly formatted without making changes

**Note**: `cargo test-verbose` is a standard cargo command for running tests with verbose output.

## Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Tokio Async Runtime](https://tokio.rs/docs/)
- [Axum Web Framework](https://docs.rs/axum)
- [SQLite with Rust](https://docs.rs/rusqlite)
- [Serde Serialization](https://serde.rs/)

## Getting Help

- [GitHub Issues](https://github.com/xiqing85/mibee-eye-notebook/issues)
- [Documentation](https://github.com/xiqing85/mibee-eye-notebook/docs)
- [Project Discord/Community] (if available)

Happy coding! 🚀