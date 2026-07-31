//! The `stax-parity` binary — the differ's entry point.
//!
//! A stub this wave: wave 0's parity check is run by hand (`stax-rs status`
//! against the Python reader on the live store, both outputs recorded in
//! `PERF.md`'s sibling notes). Wave 1 turns that into fixture-driven runs here.

#![forbid(unsafe_code)]

fn main() {
    eprintln!(
        "stax-parity: not implemented yet — wave 0 ships the crate shell only.\n\
         See docs/specs/rust-port.md §3 (parity/) and §5 (parity is the definition of done)."
    );
    std::process::exit(2);
}
