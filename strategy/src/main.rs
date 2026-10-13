//! Runner for your strategy. Do not edit: only `strategy.rs` is submitted.
//!
//! `cargo run --release -- [--data PATH] [--equity-csv PATH]`

mod strategy;
#[cfg(test)]
mod tests;

fn main() -> std::process::ExitCode {
    sim::runner::main::<strategy::MyStrategy>()
}
