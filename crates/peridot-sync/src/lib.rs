//! Peridot's sync engine. Your Omarchy settings are published as encrypted
//! events that only your own devices can read; this crate decides what
//! syncs, packs and unpacks it, applies it safely, pairs new computers and
//! makes recovery kits. The daemon (`peridotd`) drives it.

pub mod apply;
pub mod crypto;
pub mod envelope;
pub mod identity;
pub mod manifest;
pub mod pairing;
pub mod recovery;
pub mod scan;
pub mod signer;
pub mod store;
pub mod sync;

/// Kind for everything Peridot stores (NIP-78 application data).
pub const DATA_KIND: u16 = 30078;
/// Ephemeral kind for pairing messages (relays forward, never store).
pub const PAIR_KIND: u16 = 21078;
