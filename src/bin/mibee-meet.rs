//! mibee-meet — dictation form (design §2 extension): on-demand meeting
//! recording → speaker diarization → per-segment transcription minutes.
//! Form-gated to the audio + transcribe families (runtime/forms.rs); the
//! ASR models ride `[voice]`, so build with `--features voice`.
use clap::Parser;

use mibee_eye::runtime::full::{Args, run_full};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_full("meet", Args::parse()).await
}
