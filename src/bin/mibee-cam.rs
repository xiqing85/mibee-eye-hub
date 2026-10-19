//! mibee-cam — camera form (design §2 basic form 1): capture, streaming and the camera protocols. Skeleton phase — assembly shared with the full form; the capability subset lands with the M2 form milestones.
use clap::Parser;

use mibee_eye::runtime::full::{Args, run_full};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_full("cam", Args::parse()).await
}
