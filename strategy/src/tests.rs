//! Checks `MyStrategy` against the evaluation rules: `cargo test`. Do not edit; put your own
//! tests in `strategy.rs`.
//!
//! They fail with "not yet implemented" until you implement `MyStrategy::call`. Passing them
//! does not mean your strategy makes money: `cargo run --release` prints your balance.

use crate::strategy::MyStrategy;
use sim::runner::{DEFAULT_DATA, Options, Run, run};
use sim::{Account, Book, Level, MAX_ORDERS, Order, STARTING_CASH, Strategy, fmt};
use std::sync::OnceLock;

/// A fresh strategy, created the way the evaluator creates it.
fn fresh<S: Strategy + Default + 'static>() -> Box<dyn Strategy> {
    Box::new(S::default())
}

/// Two fresh copies of `MyStrategy`, each with its own account, side by side over the sample.
/// Runs once and is shared by the tests below.
fn sample_run() -> &'static Run {
    static RUN: OnceLock<Run> = OnceLock::new();
    RUN.get_or_init(|| {
        let strategies =
            vec![("first".to_string(), fresh::<MyStrategy>()), ("second".to_string(), fresh::<MyStrategy>())];
        run(strategies, &Options { data: DEFAULT_DATA.into(), equity_csv: None }).unwrap()
    })
}

/// The file says who you are (RULES.md §10).
#[test]
fn says_who_you_are() {
    let info = fresh::<MyStrategy>().info();
    assert!(!info.nickname.trim().is_empty(), "info(): the nickname is empty");
    assert!(!info.strategy.trim().is_empty(), "info(): the strategy name is empty");
    let (user, domain) = info.email.split_once('@').unwrap_or_default();
    assert!(!user.is_empty() && domain.contains('.'), "info(): {:?} is not an email address", info.email);
}

/// One call on a hand-built book. To test your own logic, build books and accounts like this.
#[test]
fn returns_valid_orders_for_one_snapshot() {
    let bids = [Level { price: 8_303_800, qty: 50_000 }, Level { price: 8_303_700, qty: 120_000 }];
    let asks = [Level { price: 8_303_801, qty: 30_000 }, Level { price: 8_303_900, qty: 80_000 }];
    let book = Book { bids: &bids, asks: &asks };
    let account = Account { position: 0, cash: STARTING_CASH, resting: &[] };
    let orders = fresh::<MyStrategy>().call(1_790_607_733_600, &book, &account);
    assert!(orders.len() <= MAX_ORDERS, "{} orders: entries after the {MAX_ORDERS}th are rejected", orders.len());
    for order in &orders {
        let valid = match *order {
            Order::Limit { price, qty, .. } => price > 0 && qty > 0,
            Order::Market { qty, .. } => qty > 0,
        };
        assert!(valid, "{order:?} would be rejected: quantities and limit prices must be positive");
    }
}

/// The whole sample runs without a panic. A panic during evaluation disqualifies (RULES.md §9).
#[test]
fn runs_on_the_sample() {
    let run = sample_run();
    assert_eq!(run.summaries[0].report.snapshots, run.snapshots);
}

/// Trading less than the minimum volume disqualifies (RULES.md §3). On the sample the minimum is
/// pro rata.
#[test]
fn meets_the_minimum_volume() {
    let report = &sample_run().summaries[0].report;
    assert!(
        report.meets_volume(),
        "traded {} BTC, the minimum for the sample is {} BTC",
        fmt::btc(report.volume()),
        fmt::btc(report.required_volume())
    );
}

/// The same input must produce the same output (RULES.md §9): two fresh copies trade alike.
#[test]
fn is_deterministic() {
    let [first, second] = &sample_run().summaries[..] else { unreachable!() };
    assert_eq!(first.report, second.report, "two fresh copies of MyStrategy traded differently");
}
