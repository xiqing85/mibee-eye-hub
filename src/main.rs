use clap::Parser;

use mibee_eye::runtime::full::{Args, run_full};

/// mibee-eye — the full form (all capabilities).
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_full("eye", Args::parse()).await
}
