//! A throwaway local relay for trying Peridot without touching real
//! servers: `cargo run -p peridotd --example dev_relay`, then point a
//! scratch daemon's `relays` at the printed URL.

use nostr_sdk::prelude::MockRelay;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let relay = MockRelay::run().await?;
    println!("{}", relay.url().await);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
