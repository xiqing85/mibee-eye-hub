//! mibee-talk — intercom form (design §2 basic form 3): talkback plus PTZ. Form-gated to audio in+out (runtime/forms.rs).
use clap::Parser;

use mibee_eye::runtime::full::{Args, run_full};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_full("talk", Args::parse()).await
}
