# Rust BTC Trading Challenge

Write a trading strategy in Rust. We replay real BTC order book data through a simulator, and your strategy trades against it.

**The biggest balance at the end wins.**

**The challenge is over.** [RESULTS.md](RESULTS.md) has the ranking and the evaluation data, so you can reproduce your score.

Deadline: **October 16, 2026, 12:00** local time. Results at 17:30 the same day, at our booth. Submit through [the submission form](https://docs.google.com/forms/d/e/1FAIpQLScRaEQ4Gh9ORZOS3dJiwltF3MDNxnjOSj69e_qIRUx9LS8aog/viewform).

## Start

Install Rust with [rustup](https://rustup.rs). The pinned toolchain (Rust 1.96) installs itself on the first build. Clone this repository, then:

```sh
cargo test
```

It fails: your strategy is still a `todo!()`.

1. Write your strategy in `strategy/src/strategy.rs`, and fill in `info()` there with your nickname, your email and a name for your strategy.
2. Run `cargo test` until it passes. It checks the rules that disqualify.
3. Run `cargo run --release` to see your balance on the sample data.
4. Submit `strategy/src/strategy.rs`.

## The rules in one minute

- Every 100 ms your strategy gets the order book (up to 50 levels on each side) and returns the orders it wants.
- You start with **10,000 USD** and no BTC. Your position must stay within **±10 BTC**, even if all your orders fill. Shorting is allowed and there is no cash check.
- The list you return is **every limit order you want resting**. Orders you leave out are cancelled. Market orders execute once. At most 20 orders per call.
- Orders take effect at the **next** snapshot, 100 ms later.
- Market orders, and limit orders that cross the book, pay **1 bps**. A resting order fills only when the market trades **through** its price, and earns a **0.1 bps** rebate.
- Liquidity you take is gone for the rest of that snapshot. Otherwise the market does not react to you.
- Trade at least **10 BTC per day of data**, buys plus sells, or you are disqualified.
- At the end your orders are cancelled and your position is closed at market. The USD you have then is your final balance.
- The winner is found on **at least 4 days** of hidden data, replayed as one run or several, with the dates shifted by a secret number of days. The biggest final balance wins, summed if there are several runs. On a tie, the earlier submission wins.

Your code:

- One file, `strategy.rs`, at most 200 KB, standard library only.
- No `unsafe`, threads, files, network, processes, environment variables or clock. Your own seeded random numbers are fine.
- Deterministic: the same input gives the same output. `HashMap` iterates in a random order; use `BTreeMap`.
- No historical market data, and no trying to find out which days you are trading on.
- At most 100 µs per snapshot on average, and 1 GiB of memory.

Failing to build, panicking or breaking a limit disqualifies. The full rules are in [RULES.md](RULES.md); if this page and RULES.md disagree, RULES.md wins.

## Your strategy

Prices are integers in ticks of 0.01 USD. Quantities are integers in lots of 0.00001 BTC, so 1 BTC is 100,000 lots. This strategy quotes at the best bid and ask:

```rust
use sim::{Account, Book, Info, Order, Side, Strategy};

#[derive(Default)]
pub struct MyStrategy;

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info {
            nickname: "your-nickname",
            email: "you@example.com",
            strategy: "Touch quoter",
        }
    }

    fn call(
        &mut self,
        _ts_ms: u64,
        book: &Book,
        account: &Account,
    ) -> Vec<Order> {
        let qty = 1_000; // 0.01 BTC
        let bid = book.best_bid().price;
        let ask = book.best_ask().price;
        let mut orders = Vec::new();
        // Hold at most 0.1 BTC either way.
        if account.position < 10_000 {
            let side = Side::Buy;
            orders.push(Order::Limit { side, price: bid, qty });
        }
        if account.position > -10_000 {
            let side = Side::Sell;
            orders.push(Order::Limit { side, price: ask, qty });
        }
        orders
    }
}
```

It passes `cargo test` and loses about 250 USD on the sample. Making money is up to you.

## Commands

- `cargo test` checks your strategy against the rules.
- `cargo run --release` runs it on the sample and prints your balance, fees, fills, volume and time per call.
- `cargo run --release -- --equity-csv pnl.csv` also writes your PnL once per minute.
- `cargo doc -p sim --open` opens the API reference.

## What's here

```
strategy/src/strategy.rs  your strategy
strategy/src/tests.rs     cargo test checks
crates/sim/               the simulator
data/sample/              48 min of data
RULES.md                  the full rules
```

The simulator is exactly the one used for final scoring. The sample data (2026-09-28) is not used for scoring.

## Submit

Upload `strategy/src/strategy.rs` itself, not a zip, through [the submission form](https://docs.google.com/forms/d/e/1FAIpQLScRaEQ4Gh9ORZOS3dJiwltF3MDNxnjOSj69e_qIRUx9LS8aog/viewform) before **October 16, 2026, 12:00** local time. Results are announced at 17:30 the same day, at our booth. Submissions are not checked when they arrive, so run `cargo test` before you send yours. Its `info()` says who you are: the ranking shows your nickname and strategy name, and your email is only used to contact you, never published. You can resubmit until the deadline, and your last submission counts. One entry per person.

Questions: ask in [our Discord channel](https://discord.com/channels/1422894279683997836/1559615257414992033).
