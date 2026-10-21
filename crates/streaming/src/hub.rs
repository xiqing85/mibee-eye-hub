//! Stream hub — fan-out from one source to many outputs.
//!
//! [`StreamHub`] is the central orchestrator of the media pipeline. It
//! reads frames from a single [`Source`] and distributes them to one or
//! more [`Output`]s via a `tokio::sync::broadcast` channel, providing
//! concurrent delivery with bounded backpressure.
//!
//! # Architecture
//!
//! ```text
//!                     ┌──────────────┐
//!   Source ──pull──▶  │ StreamHub    │──broadcast──▶ Output task 1
//!                     │ (source task)│──broadcast──▶ Output task 2
//!                     │              │──broadcast──▶ Output task N
//!                     └──────────────┘
//! ```
//!
//! The source task reads from [`Source::next_frame`] and sends each frame
//! into a `broadcast` channel. Each output runs its own tokio task that
//! receives frames from the broadcast and calls [`Output::send_frame`].
//! This decouples source frame rate from output processing speed — a slow
//! output will lag (and potentially drop frames) without blocking the
//! source or other outputs.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::Result;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::buffer::BufferPool;
use crate::output::Output;
use crate::resource::{ResourceController, StreamBudget, StreamLifecycle, StreamState};
use crate::source::{MediaFrame, Source};

/// Type used to identify an output within a hub.
pub type OutputId = Uuid;

// ---------------------------------------------------------------------------
// StreamHub
// ---------------------------------------------------------------------------

/// Shared mutable state for the hub.
struct HubInner {
    /// Registered outputs, keyed by ID.
    outputs: HashMap<OutputId, Box<dyn Output>>,
    /// Stop signals for individual output tasks.
    output_stop: HashMap<OutputId, watch::Sender<bool>>,
    /// Whether the source task has been spawned.
    running: bool,
}

/// Central stream hub that fans out frames from one source to many outputs.
///
/// # Lifecycle
///
/// 1. Create with [`StreamHub::new`], providing a [`Source`].
/// 2. Add outputs with [`add_output`](StreamHub::add_output).
/// 3. Start the pipeline with [`run`](StreamHub::run).
/// 4. Stop gracefully with [`stop`](StreamHub::stop).
///
/// Outputs can be added and removed dynamically while the hub is running.
pub struct StreamHub {
    /// The media source.
    source: Option<Box<dyn Source>>,
    /// Broadcast channel sender (source task → output tasks).
    broadcast_tx: broadcast::Sender<Arc<MediaFrame>>,
    /// Global stop signal.
    stop_tx: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
    /// Shared output state.
    inner: Arc<Mutex<HubInner>>,
    /// Buffer pool for frame memory management.
    buffer_pool: BufferPool,
    /// Resource controller for stream concurrency.
    resource: ResourceController,
    /// Stream lifecycle manager.
    lifecycle: StreamLifecycle,
    /// Per-stream memory budget tracker.
    budget: StreamBudget,
    /// Unique identifier for this hub instance.
    stream_id: Uuid,
    /// Maximum number of outputs allowed.
    max_outputs: usize,
}

impl StreamHub {
    /// Create a new hub with the given source.
    ///
    /// The source is moved into the hub and will be started when [`run`](Self::run)
    /// is called.
    #[tracing::instrument(skip_all)]
    pub fn new(source: Box<dyn Source>, resource_controller: ResourceController) -> Self {
        let (stop_tx, stop_rx) = watch::channel(false);
        // Broadcast channel capacity: 64 frames. If outputs are slow they'll
        // lag and drop old frames rather than blocking the source.
        let (broadcast_tx, _) = broadcast::channel(64);

        Self {
            source: Some(source),
            broadcast_tx,
            stop_tx,
            stop_rx,
            inner: Arc::new(Mutex::new(HubInner {
                outputs: HashMap::new(),
                output_stop: HashMap::new(),
                running: false,
            })),
            buffer_pool: BufferPool::new(10 * 1024 * 1024), // 10 MB per stream
            resource: resource_controller,
            lifecycle: StreamLifecycle::new(),
            budget: StreamBudget::new(),
            stream_id: Uuid::new_v4(),
            max_outputs: 16,
        }
    }

    /// Add an output to the hub.
    ///
    /// Returns an [`OutputId`] that can be used to remove the output later.
    ///
    /// If the hub is already running (i.e., [`run`](Self::run) was called),
    /// this spawns a new tokio task for the output immediately.
    #[tracing::instrument(skip_all)]
    pub async fn add_output(&mut self, output: Box<dyn Output>) -> OutputId {
        let id = Uuid::new_v4();
        let (stop_tx, stop_rx) = watch::channel(false);
        let running: bool;

        {
            let mut inner = self.inner.lock().await;
            inner.outputs.insert(id, output);
            inner.output_stop.insert(id, stop_tx);
            running = inner.running;
        }

        // If the hub is already running, spawn a task for this output.
        if running {
            spawn_output_task(
                self.broadcast_tx.clone(),
                self.inner.clone(),
                self.stop_rx.clone(),
                id,
                stop_rx,
            );
        }

        info!("Output added: {id}");
        id
    }

    /// Try to add an output, returning an error if resource limits are exceeded.
    ///
    /// Returns `Err` with a 503-style message when:
    /// - The maximum number of outputs has been reached.
    /// - The resource controller has no available permits (all stream slots full).
    #[tracing::instrument(skip_all)]
    pub async fn try_add_output(&mut self, output: Box<dyn Output>) -> Result<OutputId> {
        {
            let inner = self.inner.lock().await;
            if inner.outputs.len() >= self.max_outputs {
                return Err(anyhow::anyhow!(
                    "503: Maximum outputs ({}) reached for this stream",
                    self.max_outputs
                ));
            }
        }
        if self.resource.available_permits() == 0 {
            return Err(anyhow::anyhow!(
                "503: All stream slots are exhausted. Try again later."
            ));
        }
        Ok(self.add_output(output).await)
    }

    /// Remove an output from the hub.
    ///
    /// The output's task will be stopped gracefully.
    #[tracing::instrument(skip_all)]
    pub async fn remove_output(&mut self, id: OutputId) -> bool {
        let mut inner = self.inner.lock().await;
        let removed = inner.outputs.remove(&id).is_some();
        // Signal the output task to stop.
        if let Some(tx) = inner.output_stop.remove(&id) {
            let _ = tx.send(true);
        }
        if removed {
            info!("Output removed: {id}");
        }
        removed
    }

    /// Start the pipeline.
    ///
    /// Consumes the source (if not already started) and spawns the main
    /// source task. For each registered output, a separate task is spawned.
    ///
    /// Returns a `JoinHandle` that resolves when the pipeline stops
    /// (either due to source exhaustion, an error, or [`stop`](Self::stop)).
    #[tracing::instrument(skip_all)]
    #[allow(clippy::async_yields_async)]
    pub async fn run(&mut self) -> JoinHandle<()> {
        let mut source = self
            .source
            .take()
            .expect("StreamHub::run called twice or no source provided");

        let broadcast_tx = self.broadcast_tx.clone();
        let mut global_stop_rx = self.stop_rx.clone();
        let resource = self.resource.clone();
        let inner = self.inner.clone();
        let lifecycle = self.lifecycle.clone();
        let budget = self.budget.clone();
        let stream_id = self.stream_id;

        // Mark as running so add_output knows to spawn tasks.
        {
            let mut g = inner.lock().await;
            g.running = true;
        }

        // Collect output IDs and spawn output tasks FIRST so they can subscribe
        // to the broadcast channel before the source starts sending frames.
        let ids: Vec<OutputId> = {
            let guard = inner.lock().await;
            guard.outputs.keys().copied().collect()
        };

        #[allow(clippy::unnecessary_to_owned)]
        for id in ids.iter().copied() {
            let inner = inner.clone();
            let bt = self.broadcast_tx.clone();
            let global_stop = self.stop_rx.clone();

            // Create a per-output stop receiver.
            let output_stop_rx: watch::Receiver<bool> = {
                let guard = inner.lock().await;
                guard
                    .output_stop
                    .get(&id)
                    .map(|tx| tx.subscribe())
                    .unwrap_or_else(|| {
                        let (_tx, rx) = watch::channel(false);
                        rx
                    })
            };

            tokio::spawn(async move {
                // Remove the output from shared state and take ownership.
                let output = {
                    let mut guard = inner.lock().await;
                    guard.outputs.remove(&id)
                };

                let mut output = match output {
                    Some(o) => o,
                    None => return,
                };

                if let Err(e) = output.start().await {
                    error!("Output {id} start failed: {e}");
                    return;
                }
                info!("Output {id} started");

                let mut rx = bt.subscribe();
                let mut global_stop = global_stop;
                let mut output_stop = output_stop_rx;

                loop {
                    tokio::select! {
                        _ = global_stop.changed() => {
                            info!("Output {id} stopping (global stop)");
                            break;
                        }
                        _ = output_stop.changed() => {
                            info!("Output {id} stopping (removed)");
                            break;
                        }
                        frame = rx.recv() => {
                            match frame {
                                Ok(frame) => {
                                    if let Err(e) = output.send_frame(&frame).await {
                                        error!("Output {id} send error: {e}");
                                        break;
                                    }
                                }
                                Err(broadcast::error::RecvError::Closed) => {
                                    debug!("Output {id} broadcast closed");
                                    break;
                                }
                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                    warn!("Output {id} lagged by {n} frames");
                                }
                            }
                        }
                    }
                }

                if let Err(e) = output.stop().await {
                    error!("Output {id} stop error: {e}");
                }
                info!("Output {id} stopped");
            });
        }

        // Spawn source task AFTER output tasks so outputs can subscribe to the
        // broadcast channel before the source starts sending frames.
        let handle: JoinHandle<()> = tokio::spawn(async move {
            // Register stream in lifecycle.
            lifecycle.start(stream_id);

            // Acquire a stream permit from the resource controller.
            match resource.acquire().await {
                Ok(_permit) => {
                    debug!("Stream permit acquired");
                    let _ = lifecycle.transition(stream_id, StreamState::Running);
                }
                Err(e) => {
                    error!("Failed to acquire stream permit: {e}");
                    let _ = lifecycle.transition(stream_id, StreamState::Error);
                    lifecycle.remove(stream_id);
                    return;
                }
            }

            // Start the source.
            if let Err(e) = source.start().await {
                error!("Source start failed: {e}");
                let _ = lifecycle.transition(stream_id, StreamState::Error);
                lifecycle.remove(stream_id);
                return;
            }
            info!("Source started, pipeline running");

            // Main loop: read frames and broadcast.
            loop {
                tokio::select! {
                    _ = global_stop_rx.changed() => {
                        info!("Pipeline stop signal received");
                        break;
                    }
                    result = source.next_frame() => {
                        let frame = match result {
                            Ok(f) => f,
                            Err(e) => {
                                error!("Source error: {e}");
                                let _ = lifecycle.transition(stream_id, StreamState::Error);
                                budget.remove_stream(stream_id);
                                break;
                            }
                        };

                        let frame = Arc::new(frame);

                        // Broadcast to all outputs.
                        // If the channel is full, `send` blocks (backpressure).
                        // `SendError` means all receivers have been dropped —
                        // which is fine; we just continue.
                        if broadcast_tx.receiver_count() > 0
                            && let Err(broadcast::error::SendError(_)) = broadcast_tx.send(frame) {
                                // All receivers dropped — nothing to track.
                            }
                    }
                }
            }

            // Cleanup: transition lifecycle and release resources.
            let _ = lifecycle.transition(stream_id, StreamState::Stopping);
            if let Err(e) = source.stop().await {
                error!("Source stop error: {e}");
            }
            let _ = lifecycle.transition(stream_id, StreamState::Stopped);
            budget.remove_stream(stream_id);
            info!("Source stopped");
        });

        handle
    }

    /// Signal the hub to stop gracefully.
    ///
    /// All tasks (source and outputs) will see the stop signal and exit.
    #[tracing::instrument(skip_all)]
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    /// Check if the hub has been signalled to stop.
    #[tracing::instrument(skip_all)]
    pub fn is_stopped(&self) -> bool {
        *self.stop_rx.borrow()
    }

    /// Create a lightweight handle that can add outputs at runtime,
    /// even after [`run`](Self::run) has moved the hub into its spawned task.
    ///
    /// The handle holds cloned Arc/Sender references to the hub's shared
    /// state, so it stays valid as long as the hub is running.
    #[tracing::instrument(skip_all)]
    pub fn handle(&self) -> HubHandle {
        HubHandle {
            broadcast_tx: self.broadcast_tx.clone(),
            inner: self.inner.clone(),
            global_stop_rx: self.stop_rx.clone(),
        }
    }

    /// Access the buffer pool.
    #[tracing::instrument(skip_all)]
    pub fn buffer_pool(&self) -> &BufferPool {
        &self.buffer_pool
    }

    /// Access the resource controller.
    #[tracing::instrument(skip_all)]
    pub fn resource_controller(&self) -> &ResourceController {
        &self.resource
    }

    /// Access the stream lifecycle manager.
    #[tracing::instrument(skip_all)]
    pub fn lifecycle(&self) -> &StreamLifecycle {
        &self.lifecycle
    }

    /// Access the per-stream memory budget tracker.
    #[tracing::instrument(skip_all)]
    pub fn budget(&self) -> &StreamBudget {
        &self.budget
    }

    /// The hub's unique stream identifier.
    #[tracing::instrument(skip_all)]
    pub fn stream_id(&self) -> Uuid {
        self.stream_id
    }
}

// ---------------------------------------------------------------------------
// HubHandle — lightweight handle for runtime output management
// ---------------------------------------------------------------------------

/**
 * Lightweight handle to a running [`StreamHub`] that can add outputs at
 * runtime, even after the hub has been moved into its spawned task.
 *
 * Obtain via [`StreamHub::handle`] BEFORE calling [`StreamHub::run`], then
 * store it (e.g., in a `StreamManager` entry) for later use.
 *
 * Clone-able and Send-safe. The handle stays valid as long as the hub is
 * running; once the hub stops, calls to [`add_output_at_runtime`](Self::add_output_at_runtime)
 * will still insert the output into shared state but the spawn will exit
 * immediately when it observes the global stop signal.
 */
#[derive(Clone)]
pub struct HubHandle {
    /// Broadcast channel from source task → output tasks.
    broadcast_tx: broadcast::Sender<Arc<MediaFrame>>,
    /// Shared output registry.
    inner: Arc<Mutex<HubInner>>,
    /// Global stop signal.
    global_stop_rx: watch::Receiver<bool>,
}

impl HubHandle {
    /// Attach a new output to the running hub.
    ///
    /// The output is started in its own tokio task and receives frames from
    /// the hub's broadcast channel. Returns the new [`OutputId`] so the
    /// caller can remove it later via [`remove_output`](Self::remove_output).
    #[tracing::instrument(skip_all)]
    pub async fn add_output_at_runtime(&self, output: Box<dyn Output>) -> OutputId {
        let id = Uuid::new_v4();
        let (stop_tx, stop_rx) = watch::channel(false);

        {
            let mut inner = self.inner.lock().await;
            inner.outputs.insert(id, output);
            inner.output_stop.insert(id, stop_tx);
        }

        // Always spawn — this handle is only useful after run() has started.
        spawn_output_task(
            self.broadcast_tx.clone(),
            self.inner.clone(),
            self.global_stop_rx.clone(),
            id,
            stop_rx,
        );

        info!("Runtime output attached: {id}");
        id
    }

    /// Remove an output from the hub by ID.
    ///
    /// Signals the output's task to stop and removes it from the registry.
    #[tracing::instrument(skip_all)]
    pub async fn remove_output(&self, id: OutputId) {
        let mut inner = self.inner.lock().await;
        // Signal the output task to stop
        if let Some(stop_tx) = inner.output_stop.remove(&id) {
            let _ = stop_tx.send(true);
        }
        // Remove the output from the map
        inner.outputs.remove(&id);
        info!("Runtime output removed: {id}");
    }

    /// Check if the hub has been signalled to stop.
    #[tracing::instrument(skip_all)]
    pub fn is_stopped(&self) -> bool {
        *self.global_stop_rx.borrow()
    }

    /// Subscribe to the hub's frame broadcast.
    ///
    /// Returns a fresh [`broadcast::Receiver`] that yields every
    /// [`MediaFrame`] the source produces. Used by pull-style consumers
    /// (e.g. the MSE/fMP4 HTTP endpoint) that want their own independent view
    /// of the stream rather than attaching a push-style [`Output`].
    ///
    /// The receiver is bound to the same capacity (64 frames) as the hub's
    /// broadcast channel; slow consumers will see `Lagged` errors and skip
    /// frames rather than blocking the source.
    pub fn subscribe_frames(&self) -> broadcast::Receiver<Arc<MediaFrame>> {
        self.broadcast_tx.subscribe()
    }
}

/// Spawn a task for an output to consume frames from the broadcast channel.
fn spawn_output_task(
    broadcast_tx: broadcast::Sender<Arc<MediaFrame>>,
    inner: Arc<Mutex<HubInner>>,
    global_stop_rx: watch::Receiver<bool>,
    id: OutputId,
    output_stop_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        // Remove the output from shared state.
        let output = {
            let mut guard = inner.lock().await;
            guard.outputs.remove(&id)
        };

        let mut output = match output {
            Some(o) => o,
            None => return,
        };

        if let Err(e) = output.start().await {
            error!("Output {id} start failed: {e}");
            return;
        }
        info!("Output {id} started");

        let mut rx = broadcast_tx.subscribe();
        let mut global_stop = global_stop_rx;
        let mut output_stop = output_stop_rx;

        loop {
            tokio::select! {
                _ = global_stop.changed() => break,
                _ = output_stop.changed() => break,
                frame = rx.recv() => {
                    match frame {
                        Ok(frame) => {
                            if let Err(e) = output.send_frame(&frame).await {
                                error!("Output {id} send error: {e}");
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("Output {id} lagged by {n} frames");
                        }
                    }
                }
            }
        }

        if let Err(e) = output.stop().await {
            error!("Output {id} stop error: {e}");
        }
        info!("Output {id} stopped");
    });
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::RtspOutput;
    use crate::output::tests::MockOutput;
    use crate::source::testing::MockSource;
    use std::time::Duration;
    use tokio::sync::broadcast;

    fn test_frame(ts: u64) -> MediaFrame {
        MediaFrame::Video {
            keyframe: ts == 0,
            data: vec![0x67, 0x42, 0x80],
            timestamp: ts,
        }
    }

    // ── Hub lifecycle ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_hub_new() {
        let source = MockSource::new(vec![test_frame(0)]);
        let hub = StreamHub::new(Box::new(source), ResourceController::new(16));
        assert!(!hub.is_stopped());
        assert!(hub.source.is_some());
    }

    #[tokio::test]
    async fn test_hub_add_remove_output() {
        let source = MockSource::new(vec![test_frame(0)]);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));

        let out = MockOutput::new();
        let id = hub.add_output(Box::new(out)).await;

        // Output should be registered
        {
            let inner = hub.inner.lock().await;
            assert!(inner.outputs.contains_key(&id));
        }

        let removed = hub.remove_output(id).await;
        assert!(removed);

        {
            let inner = hub.inner.lock().await;
            assert!(!inner.outputs.contains_key(&id));
        }
    }

    #[tokio::test]
    async fn test_hub_remove_nonexistent_output() {
        let source = MockSource::new(vec![test_frame(0)]);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));
        let removed = hub.remove_output(Uuid::new_v4()).await;
        assert!(!removed);
    }

    // ── Fan-out: 1 source → 3 outputs ──────────────────────────────────────────

    #[tokio::test]
    async fn test_hub_fan_out_to_three_outputs() {
        let frames = vec![test_frame(0), test_frame(33), test_frame(66)];
        let source = MockSource::new(frames);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));

        let out1 = MockOutput::new();
        let out2 = MockOutput::new();
        let out3 = MockOutput::new();

        let recv1 = out1.receiver();
        let recv2 = out2.receiver();
        let recv3 = out3.receiver();

        let _id1 = hub.add_output(Box::new(out1)).await;
        let _id2 = hub.add_output(Box::new(out2)).await;
        let _id3 = hub.add_output(Box::new(out3)).await;

        let _handle = hub.run().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        for (i, recv) in [&recv1, &recv2, &recv3].iter().enumerate() {
            let frames = recv.lock().await;
            assert_eq!(
                frames.len(),
                3,
                "Output {i} should have received 3 frames, got {}",
                frames.len()
            );
            assert_eq!(frames[0].timestamp(), 0);
            assert_eq!(frames[1].timestamp(), 33);
            assert_eq!(frames[2].timestamp(), 66);
        }

        hub.stop();
    }

    #[tokio::test]
    async fn test_hub_fan_out_single_frame() {
        let source = MockSource::new(vec![test_frame(42)]);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));

        let out = MockOutput::new();
        let recv = out.receiver();
        let _id = hub.add_output(Box::new(out)).await;

        let _handle = hub.run().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let frames = recv.lock().await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp(), 42);

        hub.stop();
    }

    // ── Hub lifecycle: start, run, stop ────────────────────────────────────────

    #[tokio::test]
    async fn test_hub_lifecycle() {
        let source = MockSource::new(vec![test_frame(0), test_frame(1)]);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));

        let out = MockOutput::new();
        let _id = hub.add_output(Box::new(out)).await;

        assert!(!hub.is_stopped());

        let _handle = hub.run().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        hub.stop();
        assert!(hub.is_stopped());
    }

    #[tokio::test]
    async fn test_hub_stop_before_run() {
        let source = MockSource::new(vec![test_frame(0)]);
        let hub = StreamHub::new(Box::new(source), ResourceController::new(16));
        hub.stop();
        assert!(hub.is_stopped());
    }

    // ── Graceful degradation ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_hub_source_exhaustion_cleans_up() {
        let source = MockSource::new(vec![test_frame(99)]);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));

        let out = MockOutput::new();
        let recv = out.receiver();
        let _id = hub.add_output(Box::new(out)).await;

        let handle = hub.run().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let frames = recv.lock().await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp(), 99);
    }

    // ── ResourceController integration ─────────────────────────────────────────

    #[tokio::test]
    async fn test_hub_resource_controller() {
        let ctrl = ResourceController::new(2);
        let p1 = ctrl.acquire().await.unwrap();
        let p2 = ctrl.acquire().await.unwrap();
        assert_eq!(ctrl.available_permits(), 0);

        drop(p1);
        assert_eq!(ctrl.available_permits(), 1);

        drop(p2);
        assert_eq!(ctrl.available_permits(), 2);
    }

    // ── BufferPool integration ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_hub_buffer_pool_access() {
        let source = MockSource::new(vec![test_frame(0)]);
        let hub = StreamHub::new(Box::new(source), ResourceController::new(16));
        let pool = hub.buffer_pool();
        let mut buf = pool.acquire(100).await.unwrap();
        buf.data().extend_from_slice(&[0x42; 50]);
        assert_eq!(buf.len(), 50);
    }

    // ── Hub integration: concrete output types ──────────────────────────────

    #[tokio::test]
    async fn test_hub_with_rtsp_output() {
        let frames = vec![test_frame(0), test_frame(33), test_frame(66)];
        let source = MockSource::new(frames);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));

        let (tx, mut rx) = broadcast::channel(64);
        let rtsp_out = RtspOutput::with_channel("test".to_string(), "s=Test".to_string(), 1, tx);
        let _id = hub.add_output(Box::new(rtsp_out)).await;

        let _handle = hub.run().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify frames arrived via the channel
        let mut count = 0;
        while let Ok(Ok(data)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            count += 1;
            // Data is an RTP packet (12-byte header + NAL payload)
            assert_eq!(&data[12..], &[0x67, 0x42, 0x80]);
        }
        // All 3 frames should arrive (same data since test_frame uses static data)
        assert_eq!(count, 3, "RtspOutput should receive all 3 frames");

        hub.stop();
    }

    #[tokio::test]
    async fn test_hub_mixed_outputs() {
        let frames = vec![test_frame(0), test_frame(33)];
        let source = MockSource::new(frames);
        let mut hub = StreamHub::new(Box::new(source), ResourceController::new(16));

        // RtspOutput (verified via channel, but we only check MockOutput here)
        let (tx, _rx) = broadcast::channel(64);
        let rtsp_out = RtspOutput::with_channel("test".to_string(), "s=Test".to_string(), 1, tx);
        let mock_out = MockOutput::new();
        let mock_recv = mock_out.receiver();

        let _rtsp_id = hub.add_output(Box::new(rtsp_out)).await;
        let _mock_id = hub.add_output(Box::new(mock_out)).await;

        let _handle = hub.run().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify MockOutput received all frames through the hub
        let mock_frames = mock_recv.lock().await;
        assert_eq!(mock_frames.len(), 2, "MockOutput should receive 2 frames");
        assert_eq!(mock_frames[0].timestamp(), 0);
        assert_eq!(mock_frames[1].timestamp(), 33);

        hub.stop();
    }
}
