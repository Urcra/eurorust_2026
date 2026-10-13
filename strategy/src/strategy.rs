//! Your strategy. This is the only file you submit.
//!
//! The simulator calls `MyStrategy::call` once per order book snapshot (every 100 ms of market
//! time). You return the complete set of orders you want; they take effect at the next snapshot.
//! Rules: RULES.md. API: `cargo doc -p sim --open`, module `sim::api`. README.md has a short
//! example.
//!
//! `cargo test` checks your strategy against the evaluation rules (`tests.rs`); the checks fail
//! until you implement `call`. `cargo run --release` runs it on the sample and prints your
//! balance. Your own tests can go in a `#[cfg(test)]` module at the end of this file.

use sim::{Account, Book, Info, Order, Strategy};

#[derive(Default)]
pub struct MyStrategy {
    // Your state goes here. It lives for the whole run, which can span several days of data.
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        // Your nickname, shown in the ranking; your email, only used to contact you and never
        // published; and a name for your strategy. For example:
        // Info { nickname: "jane", email: "jane@example.com", strategy: "Wall watcher" }
        todo!("your nickname, email and strategy name in strategy/src/strategy.rs")
    }

    fn call(&mut self, _ts_ms: u64, _book: &Book, _account: &Account) -> Vec<Order> {
        // `book` has the top 50 bid and ask levels (price in ticks of 0.01 USD, quantity in lots
        // of 0.00001 BTC); `account` has your position, cash and resting orders.
        //
        // Return every limit order you want resting, plus any market orders. Resting orders you
        // leave out are cancelled, so an empty list means "no orders at all". You must trade at
        // least 10 BTC per day of data (RULES.md §3).
        todo!("MyStrategy::call in strategy/src/strategy.rs; README.md has a short example")
    }
}
