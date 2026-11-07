//! The `lantern` binary: dispatches `daemon` | `gui` | CLI commands (docs/01-architecture.md §3).

fn main() {
    println!("lantern {}", env!("CARGO_PKG_VERSION"));
}
