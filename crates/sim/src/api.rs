//! The participant-facing API: implement [`Strategy`] for `MyStrategy`.
//!
//! [`Strategy::info`] says who you are and names your strategy, for the ranking. The
//! simulator calls [`Strategy::call`] once per snapshot. The returned list is **the
//! complete set of limit orders you want resting**, plus any market orders to send. It takes
//! effect at the next snapshot:
//!
//! - a limit order at a side and price where you already rest keeps that order and sets its
//!   remaining quantity to the new `qty`;
//! - a limit order at a new side and price is placed; whatever part of it crosses the book
//!   executes immediately as a taker, and the rest rests;
//! - every resting order you did not return is cancelled, so returning an empty list cancels
//!   everything;
//! - market orders execute once and never rest.
//!
//! The book is market data only: your own orders never appear in it. Your position, cash and
//! resting orders are in the [`Account`] passed to each call.
//!
//! ```
//! use sim::{Account, Book, Order, Info, Side, Strategy};
//!
//! /// Quotes 0.01 BTC one tick outside the best prices, and stops buying when long.
//! #[derive(Default)]
//! pub struct MyStrategy;
//!
//! impl Strategy for MyStrategy {
//!     fn info(&self) -> Info {
//!         Info { nickname: "jane", email: "jane@example.com", strategy: "One tick out" }
//!     }
//!
//!     fn call(&mut self, _ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
//!         let mut orders = vec![Order::Limit { side: Side::Sell, price: book.best_ask().price + 1, qty: 1_000 }];
//!         if account.position <= 0 {
//!             orders.push(Order::Limit { side: Side::Buy, price: book.best_bid().price - 1, qty: 1_000 });
//!         }
//!         orders
//!     }
//! }
//! ```

use crate::types::{Cash, Level, Price, Qty};

/// Maximum absolute position, in lots (10 BTC).
pub const POSITION_LIMIT: Qty = 1_000_000;

/// Maximum number of entries in one returned order list. Entries beyond it are rejected.
pub const MAX_ORDERS: usize = 20;

/// Taker fee in parts per million of notional: 1 bps (0.01%).
pub const TAKER_FEE_PPM: Cash = 100;

/// Maker fee in parts per million of notional: a rebate of 0.1 bps (0.001%). Negative fees are
/// paid to you.
pub const MAKER_FEE_PPM: Cash = -10;

/// Cash units per USD.
pub const CASH_PER_USD: Cash = 10_000_000;

/// Starting cash (10,000 USD).
pub const STARTING_CASH: Cash = 10_000 * CASH_PER_USD;

/// Minimum trading volume per day of data, in lots (10 BTC): everything you buy plus everything
/// you sell, not counting the end-of-run liquidation. Pro rata to the length of the run, 10 BTC
/// per 864,000 snapshots. A submission that trades less is disqualified (RULES.md §3).
pub const MIN_DAILY_VOLUME: Qty = 1_000_000;

/// Order side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Buy,
    Sell,
}

/// One order book snapshot. Both sides are non-empty, strictly ordered best first, and the book
/// is never crossed.
#[derive(Clone, Copy, Debug)]
pub struct Book<'a> {
    /// Bid levels, highest price first. At most 50.
    pub bids: &'a [Level],
    /// Ask levels, lowest price first. At most 50.
    pub asks: &'a [Level],
}

impl Book<'_> {
    /// Highest bid.
    pub fn best_bid(&self) -> Level {
        self.bids[0]
    }

    /// Lowest ask.
    pub fn best_ask(&self) -> Level {
        self.asks[0]
    }

    /// Twice the mid price, in ticks. Doubling keeps it an integer.
    pub fn mid_x2(&self) -> Price {
        self.bids[0].price + self.asks[0].price
    }

    /// Best ask minus best bid, in ticks. Always positive.
    pub fn spread(&self) -> Price {
        self.asks[0].price - self.bids[0].price
    }
}

/// An order returned from [`Strategy::call`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// Rest `qty` lots at `price` ticks. Several entries with the same side and price are
    /// combined into one order with the summed quantity.
    Limit { side: Side, price: Price, qty: Qty },
    /// Take `qty` lots from the book now, best price first. Any unfilled remainder is dropped.
    Market { side: Side, qty: Qty },
}

/// One of your limit orders currently resting in the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resting {
    pub side: Side,
    pub price: Price,
    /// Quantity still unfilled, in lots.
    pub qty: Qty,
}

/// Your account as of the snapshot being handled. Compare with the previous call to see what
/// executed: fills change `position` and `cash` (fees included) and shrink or remove `resting`
/// orders. An order you returned that is neither resting nor filled was rejected (RULES.md §4);
/// the runner reports rejections by reason.
#[derive(Clone, Copy, Debug)]
pub struct Account<'a> {
    /// Position in lots. Positive is long.
    pub position: Qty,
    /// Cash in units of 1e-7 USD (see [`CASH_PER_USD`]). Starts at [`STARTING_CASH`].
    pub cash: Cash,
    /// Your resting limit orders after this snapshot's executions.
    pub resting: &'a [Resting],
}

/// Who wrote a strategy, for the ranking (RULES.md §10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Your nickname, shown in the ranking.
    pub nickname: &'static str,
    /// Your email. It is only used to contact you and is never published.
    pub email: &'static str,
    /// Your strategy's name, shown in the ranking next to your nickname.
    pub strategy: &'static str,
}

/// A trading strategy. The submitted `MyStrategy` must implement this and `Default`.
pub trait Strategy {
    /// Who you are and what your strategy is called. Called before the first snapshot.
    fn info(&self) -> Info;

    /// Called once per snapshot with its time (`ts_ms`, milliseconds since the Unix epoch,
    /// UTC, a multiple of 100). Returns the full set of orders you want; see the module docs.
    /// The orders returned for the last snapshot are discarded.
    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order>;
}
