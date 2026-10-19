//! mibee-mic — audio-pickup form (design §2 basic form 2): microphone, sound events, speech-to-text records. Skeleton phase — assembly shared with the full form.
use clap::Parser;

use mibee_eye::runtime::full::{Args, run_full};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_full("mic", Args::parse()).await
}
