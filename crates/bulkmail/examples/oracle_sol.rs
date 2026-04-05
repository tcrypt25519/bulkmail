//! Solana oracle example (placeholder).
//!
//! Enable with `--features solana` once the Solana adapter is implemented.

#[cfg(feature = "solana")]
fn main() {
    // TODO: wire up Solana adapter once implemented.
    println!("Solana adapter not implemented yet.");
}

#[cfg(not(feature = "solana"))]
fn main() {
    println!("Enable the `solana` feature to build this example.");
}
