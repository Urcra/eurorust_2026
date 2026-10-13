//! Matching engine and accounting: RULES.md §4–7, in the order of processing of RULES.md §5.
//!
//! The engine calls the strategy once per snapshot and executes the returned list at the next
//! snapshot. Everything is integer arithmetic, and nothing depends on time or randomness, so a
//! run is fully determined by the data and the strategy.
//!
//! Fills and rejects are recorded here for reports only; strategies never see them.

use crate::api::{
    Account, Book, MAKER_FEE_PPM, MAX_ORDERS, MIN_DAILY_VOLUME, Order, POSITION_LIMIT, Resting, STARTING_CASH, Side,
    Strategy, TAKER_FEE_PPM,
};
use crate::data::Snapshot;
use crate::types::{Cash, Price, Qty};
use std::fmt;

/// Whether a fill added liquidity (resting order) or took it (market or crossing limit order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liquidity {
    Maker,
    Taker,
}

/// One execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    /// Time of the snapshot the fill happened at.
    pub ts_ms: u64,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    pub liquidity: Liquidity,
    /// Fee charged, in cash units.
    pub fee: Cash,
}

/// Why an order from a strategy's list was not accepted (RULES.md §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RejectReason {
    /// Quantity is zero or negative.
    InvalidQty,
    /// Limit price is zero or negative.
    InvalidPrice,
    /// Accepting it could take the position beyond [`POSITION_LIMIT`].
    PositionLimit,
    /// Beyond the first [`MAX_ORDERS`] entries of the list.
    TooManyOrders,
}

impl RejectReason {
    /// All reasons, in the order of [`Report::rejects`].
    pub const ALL: [RejectReason; 4] = [
        RejectReason::InvalidQty,
        RejectReason::InvalidPrice,
        RejectReason::PositionLimit,
        RejectReason::TooManyOrders,
    ];
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RejectReason::InvalidQty => "invalid quantity",
            RejectReason::InvalidPrice => "invalid price",
            RejectReason::PositionLimit => "position limit",
            RejectReason::TooManyOrders => "too many orders",
        })
    }
}

/// Result of a run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Snapshots processed.
    pub snapshots: u64,
    /// Final balance minus starting cash.
    pub pnl: Cash,
    /// Maker fees, negative for a rebate received.
    pub maker_fees: Cash,
    /// Taker fees, liquidation included.
    pub taker_fees: Cash,
    pub maker_fills: u64,
    pub maker_qty: Qty,
    pub taker_fills: u64,
    pub taker_qty: Qty,
    /// Rejected orders, indexed by `RejectReason as usize`.
    pub rejects: [u64; 4],
    /// Largest absolute position reached.
    pub max_abs_position: Qty,
    /// Absolute position closed by the end-of-run liquidation.
    pub liquidation_qty: Qty,
    /// Part of `liquidation_qty` closed at the fallback price because the book was too thin.
    pub liquidation_fallback_qty: Qty,
    /// Every fill in execution order, if enabled with [`Engine::recording_fills`].
    pub fills: Vec<Fill>,
}

/// Snapshots in a full day of data (one per 100 ms).
pub const SNAPSHOTS_PER_DAY: u64 = 864_000;

impl Report {
    /// Cash after the end-of-run liquidation, when no BTC and no orders are left. This is the
    /// run's score; with several runs, the scores add up (RULES.md §8).
    pub fn final_balance(&self) -> Cash {
        STARTING_CASH + self.pnl
    }

    /// Quantity the strategy traded, buys plus sells, without the end-of-run liquidation.
    pub fn volume(&self) -> Qty {
        self.maker_qty + self.taker_qty - self.liquidation_qty
    }

    /// Minimum volume for this run: [`MIN_DAILY_VOLUME`] pro rata to the snapshots processed.
    pub fn required_volume(&self) -> Qty {
        (MIN_DAILY_VOLUME as i128 * self.snapshots as i128 / SNAPSHOTS_PER_DAY as i128) as Qty
    }

    /// Whether the run meets the minimum volume.
    pub fn meets_volume(&self) -> bool {
        self.volume() >= self.required_volume()
    }

    /// Net fees: taker fees plus maker fees (rebates are negative).
    pub fn fees(&self) -> Cash {
        self.maker_fees + self.taker_fees
    }
}

/// Fee for a fill, in cash units, rounded against the participant: a fee is rounded up, a
/// rebate (negative fee) towards zero.
pub fn fee(price: Price, qty: Qty, liquidity: Liquidity) -> Cash {
    let rate = match liquidity {
        Liquidity::Maker => MAKER_FEE_PPM,
        Liquidity::Taker => TAKER_FEE_PPM,
    };
    let scaled = price as Cash * qty as Cash * rate;
    let (quotient, remainder) = (scaled.div_euclid(1_000_000), scaled.rem_euclid(1_000_000));
    quotient + (remainder != 0) as Cash
}

/// An accepted entry of the strategy's list, after merging.
struct Accepted {
    side: Side,
    /// `None` for market orders.
    price: Option<Price>,
    qty: Qty,
    /// Matches an order that was already resting, so it is not placed again.
    kept: bool,
}

struct Ledger {
    ts_ms: u64,
    position: Qty,
    cash: Cash,
    record: bool,
    report: Report,
}

impl Ledger {
    fn fill(&mut self, side: Side, price: Price, qty: Qty, liquidity: Liquidity) {
        debug_assert!(qty > 0);
        let notional = price as Cash * qty as Cash;
        let fee = fee(price, qty, liquidity);
        match side {
            Side::Buy => {
                self.cash -= notional + fee;
                self.position += qty;
            }
            Side::Sell => {
                self.cash += notional - fee;
                self.position -= qty;
            }
        }
        let r = &mut self.report;
        match liquidity {
            Liquidity::Maker => {
                (r.maker_fills, r.maker_qty, r.maker_fees) = (r.maker_fills + 1, r.maker_qty + qty, r.maker_fees + fee)
            }
            Liquidity::Taker => {
                (r.taker_fills, r.taker_qty, r.taker_fees) = (r.taker_fills + 1, r.taker_qty + qty, r.taker_fees + fee)
            }
        }
        r.max_abs_position = r.max_abs_position.max(self.position.abs());
        if self.record {
            r.fills.push(Fill { ts_ms: self.ts_ms, side, price, qty, liquidity, fee });
        }
    }

    fn reject(&mut self, reason: RejectReason, n: u64) {
        self.report.rejects[reason as usize] += n;
    }
}

/// Replays snapshots against one strategy.
pub struct Engine {
    ledger: Ledger,
    /// Resting orders, oldest first. `(side, price)` is unique.
    resting: Vec<Resting>,
    /// List returned by the last call, executed at the next snapshot.
    pending: Vec<Order>,
    accepted: Vec<Accepted>,
    /// The current snapshot with quantities reduced by what was consumed at it.
    liquidity: Snapshot,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            ledger: Ledger { ts_ms: 0, position: 0, cash: STARTING_CASH, record: false, report: Report::default() },
            resting: Vec::new(),
            pending: Vec::new(),
            accepted: Vec::new(),
            liquidity: Snapshot::default(),
        }
    }

    /// Records every fill in [`Report::fills`].
    pub fn recording_fills(mut self) -> Self {
        self.ledger.record = true;
        self
    }

    pub fn position(&self) -> Qty {
        self.ledger.position
    }

    pub fn cash(&self) -> Cash {
        self.ledger.cash
    }

    pub fn resting(&self) -> &[Resting] {
        &self.resting
    }

    /// Processes one snapshot: executes the previous list, then calls the strategy.
    pub fn step<S: Strategy + ?Sized>(&mut self, snapshot: &Snapshot, strategy: &mut S) {
        debug_assert!(snapshot.validate(usize::MAX).is_ok());
        self.liquidity.clone_from(snapshot);
        self.ledger.ts_ms = snapshot.ts_ms;
        self.resolve();
        self.fill_resting();
        self.place_new();
        self.ledger.report.snapshots += 1;
        let book = Book { bids: &snapshot.bids, asks: &snapshot.asks };
        let account = Account { position: self.ledger.position, cash: self.ledger.cash, resting: &self.resting };
        self.pending = strategy.call(snapshot.ts_ms, &book, &account);
    }

    /// Ends the run: discards the last list, cancels resting orders and liquidates the position
    /// against what is left of the last snapshot (RULES.md §7).
    pub fn finish(mut self) -> Report {
        self.pending.clear();
        self.resting.clear();
        let Self { ledger, liquidity, .. } = &mut self;
        let position = ledger.position;
        if position != 0 {
            let side = if position > 0 { Side::Sell } else { Side::Buy };
            let levels = match side {
                Side::Buy => &mut liquidity.asks,
                Side::Sell => &mut liquidity.bids,
            };
            let mut qty = position.abs();
            for level in levels.iter_mut() {
                let take = qty.min(level.qty);
                if take > 0 {
                    level.qty -= take;
                    qty -= take;
                    ledger.fill(side, level.price, take, Liquidity::Taker);
                }
            }
            if qty > 0
                && let Some(worst) = levels.last().map(|l| l.price)
            {
                // Worst visible price ∓ 1%, rounded against the participant.
                let price = match side {
                    Side::Sell => worst * 99 / 100,
                    Side::Buy => (worst * 101 + 99) / 100,
                };
                ledger.fill(side, price, qty, Liquidity::Taker);
                ledger.report.liquidation_fallback_qty = qty;
            }
            ledger.report.liquidation_qty = position.abs();
        }
        ledger.report.pnl = ledger.cash - STARTING_CASH;
        self.ledger.report
    }

    /// Step 2: validate the list, merge it into the desired set, cancel or resize resting orders.
    fn resolve(&mut self) {
        let Self { ledger, resting, pending, accepted, .. } = self;
        accepted.clear();
        if pending.len() > MAX_ORDERS {
            ledger.reject(RejectReason::TooManyOrders, (pending.len() - MAX_ORDERS) as u64);
        }
        // Worst case: every accepted buy (or sell) fills in full. i128, because quantities are
        // arbitrary until checked.
        let (position, limit) = (ledger.position as i128, POSITION_LIMIT as i128);
        let (mut buys, mut sells) = (0i128, 0i128);
        for order in pending.iter().take(MAX_ORDERS) {
            let (side, price, qty) = match *order {
                Order::Limit { side, price, qty } => (side, Some(price), qty),
                Order::Market { side, qty } => (side, None, qty),
            };
            let reason = if qty <= 0 {
                Some(RejectReason::InvalidQty)
            } else if price.is_some_and(|p| p <= 0) {
                Some(RejectReason::InvalidPrice)
            } else {
                match side {
                    Side::Buy if position + buys + qty as i128 <= limit => {
                        buys += qty as i128;
                        None
                    }
                    Side::Sell if position - sells - qty as i128 >= -limit => {
                        sells += qty as i128;
                        None
                    }
                    _ => Some(RejectReason::PositionLimit),
                }
            };
            if let Some(reason) = reason {
                ledger.reject(reason, 1);
                continue;
            }
            match price.and_then(|p| accepted.iter_mut().find(|a| a.side == side && a.price == Some(p))) {
                Some(a) => a.qty += qty,
                None => accepted.push(Accepted { side, price, qty, kept: false }),
            }
        }
        pending.clear();
        resting.retain_mut(|r| match accepted.iter_mut().find(|a| a.side == r.side && a.price == Some(r.price)) {
            Some(a) => {
                a.kept = true;
                r.qty = a.qty;
                true
            }
            None => false,
        });
    }

    /// Step 3: resting orders fill when the market trades strictly through them, at their price.
    fn fill_resting(&mut self) {
        let Self { ledger, resting, liquidity, .. } = self;
        for r in resting.iter_mut() {
            let levels = match r.side {
                Side::Buy => &mut liquidity.asks,
                Side::Sell => &mut liquidity.bids,
            };
            for level in levels.iter_mut() {
                let through = match r.side {
                    Side::Buy => level.price < r.price,
                    Side::Sell => level.price > r.price,
                };
                if !through || r.qty == 0 {
                    break;
                }
                let take = r.qty.min(level.qty);
                if take > 0 {
                    level.qty -= take;
                    r.qty -= take;
                    ledger.fill(r.side, r.price, take, Liquidity::Maker);
                }
            }
        }
        resting.retain(|r| r.qty > 0);
    }

    /// Step 4: market orders and new limit orders take liquidity at level prices; limit
    /// remainders rest.
    fn place_new(&mut self) {
        let Self { ledger, resting, liquidity, accepted, .. } = self;
        for a in accepted.iter().filter(|a| !a.kept) {
            let levels = match a.side {
                Side::Buy => &mut liquidity.asks,
                Side::Sell => &mut liquidity.bids,
            };
            let mut qty = a.qty;
            for level in levels.iter_mut() {
                let marketable = match (a.side, a.price) {
                    (_, None) => true,
                    (Side::Buy, Some(p)) => level.price <= p,
                    (Side::Sell, Some(p)) => level.price >= p,
                };
                if !marketable || qty == 0 {
                    break;
                }
                let take = qty.min(level.qty);
                if take > 0 {
                    level.qty -= take;
                    qty -= take;
                    ledger.fill(a.side, level.price, take, Liquidity::Taker);
                }
            }
            if let Some(price) = a.price
                && qty > 0
            {
                resting.push(Resting { side: a.side, price, qty });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Info;
    use crate::types::Level;
    use std::collections::VecDeque;

    /// A realistic price, in ticks (83,000 USD).
    const P: Price = 8_300_000;

    fn snap(ts_ms: u64, bids: &[(Price, Qty)], asks: &[(Price, Qty)]) -> Snapshot {
        let levels = |side: &[(Price, Qty)]| side.iter().map(|&(price, qty)| Level { price, qty }).collect();
        Snapshot { ts_ms, bids: levels(bids), asks: levels(asks) }
    }

    /// A quiet book far from `P`, so nothing near `P` executes.
    fn quiet(ts_ms: u64) -> Snapshot {
        snap(ts_ms, &[(P - 1_000, 1_000_000)], &[(P + 1_000, 1_000_000)])
    }

    fn buy(price: Price, qty: Qty) -> Order {
        Order::Limit { side: Side::Buy, price, qty }
    }

    fn sell(price: Price, qty: Qty) -> Order {
        Order::Limit { side: Side::Sell, price, qty }
    }

    fn mbuy(qty: Qty) -> Order {
        Order::Market { side: Side::Buy, qty }
    }

    fn msell(qty: Qty) -> Order {
        Order::Market { side: Side::Sell, qty }
    }

    fn resting(side: Side, price: Price, qty: Qty) -> Resting {
        Resting { side, price, qty }
    }

    fn fill(ts_ms: u64, side: Side, price: Price, qty: Qty, liquidity: Liquidity) -> Fill {
        Fill { ts_ms, side, price, qty, liquidity, fee: fee(price, qty, liquidity) }
    }

    /// What a strategy saw at one call.
    #[derive(Debug, PartialEq)]
    struct Seen {
        position: Qty,
        cash: Cash,
        resting: Vec<Resting>,
    }

    /// Returns a fixed list per call, then empty lists; records every account it sees.
    struct Script {
        lists: VecDeque<Vec<Order>>,
        seen: Vec<Seen>,
    }

    impl Strategy for Script {
        fn info(&self) -> Info {
            Info { nickname: "test", email: "test@example.com", strategy: "script" }
        }

        fn call(&mut self, _ts_ms: u64, _book: &Book, account: &Account) -> Vec<Order> {
            self.seen.push(Seen { position: account.position, cash: account.cash, resting: account.resting.to_vec() });
            self.lists.pop_front().unwrap_or_default()
        }
    }

    fn run(snaps: &[Snapshot], lists: Vec<Vec<Order>>) -> (Report, Vec<Seen>) {
        let mut script = Script { lists: lists.into(), seen: Vec::new() };
        let mut engine = Engine::new().recording_fills();
        for s in snaps {
            engine.step(s, &mut script);
        }
        (engine.finish(), script.seen)
    }

    #[test]
    fn orders_execute_at_the_next_snapshot() {
        let snaps = [snap(0, &[(P - 1, 10)], &[(P, 10)]), snap(100, &[(P + 99, 10)], &[(P + 100, 10)])];
        let (report, seen) = run(&snaps, vec![vec![mbuy(5)]]);
        assert_eq!(seen[0].position, 0);
        assert_eq!(seen[1].position, 5);
        // Executed against the second snapshot's price, not the one the strategy looked at.
        assert_eq!(report.fills[0], fill(100, Side::Buy, P + 100, 5, Liquidity::Taker));
    }

    #[test]
    fn market_buy_walks_levels_and_drops_the_remainder() {
        let snaps = [quiet(0), snap(100, &[(P - 1, 10)], &[(P, 1_000), (P + 1, 1_000)])];
        let (report, seen) = run(&snaps, vec![vec![mbuy(1_500)]]);
        let fills = &report.fills[..2];
        assert_eq!(
            fills,
            [fill(100, Side::Buy, P, 1_000, Liquidity::Taker), fill(100, Side::Buy, P + 1, 500, Liquidity::Taker)]
        );
        let cost = P as Cash * 1_000 + (P + 1) as Cash * 500;
        assert_eq!(seen[1].cash, STARTING_CASH - cost - fills[0].fee - fills[1].fee);
        assert_eq!(seen[1].position, 1_500);

        let (_, seen) = run(&snaps, vec![vec![mbuy(5_000)]]);
        assert_eq!(seen[1].position, 2_000);
        assert!(seen[1].resting.is_empty());
    }

    #[test]
    fn consumed_liquidity_is_gone_for_the_rest_of_the_snapshot() {
        let snaps = [quiet(0), snap(100, &[(P - 1, 10)], &[(P, 5), (P + 1, 5)]), snap(200, &[(P - 1, 10)], &[(P, 5)])];
        let (report, _) = run(&snaps, vec![vec![mbuy(5), mbuy(5)], vec![mbuy(5)]]);
        let got: Vec<_> = report.fills.iter().take(3).map(|f| (f.ts_ms, f.price, f.qty)).collect();
        // The second order gets the next level; the next snapshot is fully refreshed.
        assert_eq!(got, [(100, P, 5), (100, P + 1, 5), (200, P, 5)]);
    }

    #[test]
    fn marketable_limit_takes_then_rests_the_remainder() {
        let snaps =
            [quiet(0), snap(100, &[(P - 1, 10)], &[(P, 5), (P + 1, 5)]), snap(200, &[(P - 2, 10)], &[(P - 1, 10)])];
        let (report, seen) = run(&snaps, vec![vec![buy(P, 8)], vec![buy(P, 3)]]);
        assert_eq!(seen[1].resting, [resting(Side::Buy, P, 3)]);
        assert_eq!(
            report.fills[..2],
            [fill(100, Side::Buy, P, 5, Liquidity::Taker), fill(200, Side::Buy, P, 3, Liquidity::Maker)]
        );
    }

    #[test]
    fn resting_orders_fill_only_when_traded_through() {
        let snaps = [
            quiet(0),
            quiet(100),
            snap(200, &[(P - 5, 10)], &[(P, 50)]),
            snap(300, &[(P - 5, 10)], &[(P - 2, 3), (P - 1, 4), (P, 50)]),
        ];
        let keep = vec![buy(P, 10)];
        let (report, seen) = run(&snaps, vec![keep.clone(), keep.clone(), keep, vec![buy(P, 3)]]);
        assert_eq!(seen[2].resting, [resting(Side::Buy, P, 10)], "touching the price is not enough");
        // Fills at the order's price, up to the quantity strictly below it, as maker.
        assert_eq!(seen[3].resting, [resting(Side::Buy, P, 3)]);
        assert_eq!(
            report.fills[..2],
            [fill(300, Side::Buy, P, 3, Liquidity::Maker), fill(300, Side::Buy, P, 4, Liquidity::Maker)]
        );
        let maker_fee = fee(P, 3, Liquidity::Maker) + fee(P, 4, Liquidity::Maker);
        assert!(maker_fee < 0, "makers earn a rebate");
        assert_eq!(seen[3].cash, STARTING_CASH - P as Cash * 7 - maker_fee);
    }

    #[test]
    fn resting_sell_mirrors_resting_buy() {
        let snaps = [quiet(0), quiet(100), snap(200, &[(P + 2, 4), (P, 50)], &[(P + 5, 10)])];
        let (report, seen) = run(&snaps, vec![vec![sell(P, 10)], vec![sell(P, 10)]]);
        assert_eq!(report.fills[0], fill(200, Side::Sell, P, 4, Liquidity::Maker));
        assert_eq!(seen[2].position, -4);
    }

    #[test]
    fn resting_orders_fill_before_new_orders_share_the_book() {
        let snaps = [quiet(0), quiet(100), snap(200, &[(P - 5, 10)], &[(P - 1, 5)])];
        let (report, seen) = run(&snaps, vec![vec![buy(P, 5)], vec![buy(P, 5), mbuy(5)]]);
        assert_eq!(report.fills[0], fill(200, Side::Buy, P, 5, Liquidity::Maker));
        assert_eq!(seen[2].position, 5, "the market order found nothing left");
    }

    #[test]
    fn omitted_orders_are_cancelled_before_passive_fills() {
        let snaps = [quiet(0), quiet(100), snap(200, &[(P - 5, 10)], &[(P - 1, 5)])];
        let (report, seen) = run(&snaps, vec![vec![buy(P, 5), sell(P + 500, 5)], vec![sell(P + 500, 5)]]);
        assert_eq!(seen[1].resting, [resting(Side::Buy, P, 5), resting(Side::Sell, P + 500, 5)]);
        assert_eq!(seen[2].resting, [resting(Side::Sell, P + 500, 5)]);
        assert_eq!(report.maker_fills, 0);
        // An empty list cancels everything.
        let (_, seen) = run(&[quiet(0), quiet(100), quiet(200)], vec![vec![buy(P, 5), sell(P + 500, 5)], vec![]]);
        assert!(seen[2].resting.is_empty());
    }

    #[test]
    fn kept_orders_keep_their_age_after_a_resize() {
        let snaps = [quiet(0), quiet(100), snap(200, &[(P - 5, 10)], &[(P - 2, 6)])];
        let (report, seen) = run(&snaps, vec![vec![buy(P, 5), buy(P - 1, 5)], vec![buy(P - 1, 5), buy(P, 4)]]);
        let got: Vec<_> = report.fills.iter().take(2).map(|f| (f.price, f.qty)).collect();
        assert_eq!(got, [(P, 4), (P - 1, 2)], "older order first, with its new size");
        assert_eq!(seen[2].resting, [resting(Side::Buy, P - 1, 3)]);
    }

    #[test]
    fn fully_filled_kept_order_is_not_placed_again() {
        let snaps = [quiet(0), quiet(100), snap(200, &[(P - 5, 10)], &[(P - 1, 10)])];
        let (report, seen) = run(&snaps, vec![vec![buy(P, 5)], vec![buy(P, 5)]]);
        assert_eq!(report.maker_fills, 1);
        assert_eq!(report.taker_fills, 1, "only the end-of-run liquidation");
        assert_eq!((seen[2].position, seen[2].resting.len()), (5, 0));
    }

    #[test]
    fn duplicates_are_merged_and_the_list_is_capped() {
        let (_, seen) = run(&[quiet(0), quiet(100)], vec![vec![buy(P, 3), sell(P + 500, 1), buy(P, 4)]]);
        assert_eq!(seen[1].resting, [resting(Side::Buy, P, 7), resting(Side::Sell, P + 500, 1)]);

        let many: Vec<Order> = (0..25).map(|i| buy(P - i, 1)).collect();
        let (report, seen) = run(&[quiet(0), quiet(100)], vec![many]);
        assert_eq!(seen[1].resting.len(), MAX_ORDERS);
        assert_eq!(seen[1].resting.last(), Some(&resting(Side::Buy, P - 19, 1)));
        assert_eq!(report.rejects[RejectReason::TooManyOrders as usize], 5);
    }

    #[test]
    fn invalid_orders_are_rejected() {
        let list = vec![buy(P, 0), buy(0, 1), sell(-5, 1), mbuy(-1), msell(0), buy(P, 1)];
        let (report, seen) = run(&[quiet(0), quiet(100)], vec![list]);
        assert_eq!(report.rejects, [3, 2, 0, 0]);
        assert_eq!(seen[1].resting, [resting(Side::Buy, P, 1)]);
    }

    #[test]
    fn rejected_limit_at_an_existing_price_cancels_it() {
        let (report, seen) = run(&[quiet(0), quiet(100), quiet(200)], vec![vec![buy(P, 5)], vec![buy(P, 0)]]);
        assert_eq!(seen[1].resting.len(), 1);
        assert!(seen[2].resting.is_empty());
        assert_eq!(report.rejects[RejectReason::InvalidQty as usize], 1);
    }

    #[test]
    fn position_check_is_worst_case_over_the_list() {
        let limit = POSITION_LIMIT;
        let list = vec![buy(P, 600_000), buy(P - 1, 500_000), sell(P + 500, limit), msell(1), buy(P - 2, 400_000)];
        let (report, seen) = run(&[quiet(0), quiet(100)], vec![list]);
        assert_eq!(report.rejects[RejectReason::PositionLimit as usize], 2);
        assert_eq!(
            seen[1].resting,
            [resting(Side::Buy, P, 600_000), resting(Side::Sell, P + 500, limit), resting(Side::Buy, P - 2, 400_000)]
        );

        // Resizing a resting order counts its new size; market orders count too.
        let lists = vec![vec![buy(P, 600_000)], vec![buy(P, limit), mbuy(1)], vec![mbuy(limit), buy(P - 1, 1)]];
        let (report, seen) = run(&[quiet(0), quiet(100), quiet(200), quiet(300)], lists);
        assert_eq!(seen[2].resting, [resting(Side::Buy, P, limit)]);
        assert_eq!(seen[3].position, limit);
        assert_eq!(report.rejects[RejectReason::PositionLimit as usize], 2);

        // Absurd quantities are rejected, not overflowed.
        let (report, _) = run(&[quiet(0), quiet(100)], vec![vec![buy(P, Qty::MAX), msell(Qty::MAX)]]);
        assert_eq!(report.rejects[RejectReason::PositionLimit as usize], 2);
    }

    #[test]
    fn position_check_uses_the_current_position() {
        let lists = vec![vec![mbuy(900_000)], vec![buy(P - 500, 200_000), sell(P + 500, 1_900_000)]];
        let (report, seen) = run(&[quiet(0), quiet(100), quiet(200)], lists);
        assert_eq!(seen[1].position, 900_000);
        assert_eq!(seen[2].resting, [resting(Side::Sell, P + 500, 1_900_000)]);
        assert_eq!(report.rejects[RejectReason::PositionLimit as usize], 1);
    }

    #[test]
    fn end_of_run_liquidation() {
        let deep = snap(100, &[(P - 1, 1_000_000)], &[(P, 1_000_000)]);
        let last = snap(200, &[(P, 5), (P - 100, 100)], &[(P + 1, 1_000_000)]);
        // Long 10, sell 3 at the last snapshot, liquidate 7 from what is left of it.
        let (report, _) = run(&[quiet(0), deep.clone(), last], vec![vec![mbuy(10)], vec![msell(3)]]);
        let got: Vec<_> = report.fills.iter().map(|f| (f.side, f.price, f.qty)).collect();
        assert_eq!(got, [(Side::Buy, P, 10), (Side::Sell, P, 3), (Side::Sell, P, 2), (Side::Sell, P - 100, 5)]);
        assert_eq!((report.liquidation_qty, report.liquidation_fallback_qty), (7, 0));

        // Thin book: the rest goes at the worst visible price minus 1%, rounded down.
        let thin = snap(200, &[(P, 5), (P - 101, 5)], &[(P + 1, 10)]);
        let (report, _) = run(&[quiet(0), deep.clone(), thin], vec![vec![mbuy(15)]]);
        let last = report.fills.last().unwrap();
        assert_eq!((last.side, last.price, last.qty), (Side::Sell, (P - 101) * 99 / 100, 5));
        assert_eq!((report.liquidation_qty, report.liquidation_fallback_qty), (15, 5));

        // Short on a thin book: worst ask plus 1%, rounded up.
        let thin = snap(200, &[(P - 1, 10)], &[(P, 5), (P + 101, 5)]);
        let (report, _) = run(&[quiet(0), deep, thin], vec![vec![msell(15)]]);
        let last = report.fills.last().unwrap();
        // 8,300,101 * 1.01 = 8,383,102.01, rounded up.
        assert_eq!((last.side, last.price, last.qty), (Side::Buy, 8_383_103, 5));
        assert_eq!(report.max_abs_position, 15);
    }

    #[test]
    fn last_list_is_discarded_and_resting_orders_do_not_fill_at_the_end() {
        let snaps = [quiet(0), quiet(100)];
        let (report, _) = run(&snaps, vec![vec![buy(P, 5)], vec![mbuy(5)]]);
        assert_eq!(report, Report { snapshots: 2, ..Report::default() });
    }

    #[test]
    fn fees_round_against_the_participant() {
        // Taker, notional of 1 or 2 cash units: the fee rounds up to 1.
        let tiny = [quiet(0), snap(100, &[(1, 10)], &[(2, 10)])];
        let (report, seen) = run(&tiny, vec![vec![mbuy(1), msell(1)]]);
        assert_eq!(report.fills.iter().map(|f| f.fee).collect::<Vec<_>>(), [1, 1]);
        // Buy: pays 2 + fee 1. Sell: receives 1 - fee 1 = 0.
        assert_eq!(seen[1].cash, STARTING_CASH - 3);
        // Exact multiples are not rounded: 1 bps of 1,000,000 is 100, and the rebate 10.
        assert_eq!(fee(1_000, 1_000, Liquidity::Taker), 100);
        assert_eq!(fee(1_000, 1_000, Liquidity::Maker), -10);
        // A rebate rounds towards zero: 0.1 bps of 150,000 is 1.5, paid as 1; of 99,999, nothing.
        assert_eq!(fee(1_500, 100, Liquidity::Maker), -1);
        assert_eq!(fee(99_999, 1, Liquidity::Maker), 0);
        // A realistic fill: 0.01 BTC at 83,000 USD is 830 USD; 1 bps is 0.083 USD, 0.1 bps 0.0083.
        assert_eq!(fee(P, 1_000, Liquidity::Taker), 830_000);
        assert_eq!(fee(P, 1_000, Liquidity::Maker), -83_000);
    }

    #[test]
    fn maker_rebate_is_credited() {
        let snaps = [quiet(0), quiet(100), snap(200, &[(P - 5, 10)], &[(P - 1, 1_000)])];
        let (report, seen) = run(&snaps, vec![vec![buy(P, 1_000)], vec![buy(P, 1_000)]]);
        assert_eq!(seen[2].cash, STARTING_CASH - P as Cash * 1_000 + 83_000);
        assert_eq!(report.maker_fees, -83_000);
    }

    #[test]
    fn doing_nothing_keeps_the_starting_balance() {
        let snaps: Vec<_> = (0..50).map(|i| quiet(i * 100)).collect();
        let (report, seen) = run(&snaps, vec![]);
        assert_eq!(report, Report { snapshots: 50, ..Report::default() });
        assert_eq!(report.final_balance(), STARTING_CASH);
        assert!(seen.iter().all(|s| *s == Seen { position: 0, cash: STARTING_CASH, resting: vec![] }));
    }

    #[test]
    fn volume_excludes_the_liquidation_and_the_minimum_is_pro_rata() {
        let deep = |ts| snap(ts, &[(P - 1, 1_000_000)], &[(P, 1_000_000)]);
        let snaps: Vec<_> = (0..4).map(|i| deep(i * 100)).collect();
        // Buy 700, sell 300, hold 400 to the end: volume 1,000, liquidation 400.
        let (report, _) = run(&snaps, vec![vec![mbuy(700)], vec![msell(300)]]);
        assert_eq!((report.volume(), report.liquidation_qty), (1_000, 400));
        // 4 snapshots of a day's 864,000 need 4.6 lots, rounded down.
        assert_eq!(report.required_volume(), 4);
        assert!(report.meets_volume());
        let full_day = Report { snapshots: SNAPSHOTS_PER_DAY, maker_qty: MIN_DAILY_VOLUME - 1, ..Report::default() };
        assert_eq!(full_day.required_volume(), MIN_DAILY_VOLUME);
        assert!(!full_day.meets_volume());
        assert!(Report { taker_qty: 1, ..full_day }.meets_volume());
    }

    #[test]
    fn finishing_without_data_is_empty() {
        assert_eq!(Engine::new().finish(), Report::default());
    }

    /// xorshift64*: deterministic randomness for the property tests below.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            lo + (self.next() % (hi - lo + 1) as u64) as i64
        }
    }

    fn random_book(rng: &mut Rng, ts_ms: u64, mid: Price) -> Snapshot {
        let half = rng.range(1, 3);
        let mut side = |start: Price, dir: Price| {
            let (mut price, n) = (start, rng.range(1, 8));
            (0..n)
                .map(|_| {
                    let level = Level { price, qty: rng.range(1, 3_000) };
                    price += dir * rng.range(1, 3);
                    level
                })
                .collect()
        };
        Snapshot { ts_ms, bids: side(mid - half, -1), asks: side(mid + half, 1) }
    }

    /// Random lists, including invalid orders, duplicates, over-long lists and huge sizes.
    struct Chaos {
        rng: Rng,
        mid: Price,
    }

    impl Strategy for Chaos {
        fn info(&self) -> Info {
            Info { nickname: "test", email: "test@example.com", strategy: "chaos" }
        }

        fn call(&mut self, _ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
            assert!(account.position.abs() <= POSITION_LIMIT);
            assert!(account.resting.len() <= MAX_ORDERS);
            assert!(account.resting.iter().all(|r| r.qty > 0));
            for (i, a) in account.resting.iter().enumerate() {
                assert!(account.resting[i + 1..].iter().all(|b| (a.side, a.price) != (b.side, b.price)));
            }
            self.mid = book.mid_x2() / 2;
            let rng = &mut self.rng;
            let n = match rng.range(0, 9) {
                0 => rng.range(21, 30),
                _ => rng.range(0, 6),
            };
            (0..n)
                .map(|_| {
                    let side = if rng.range(0, 1) == 0 { Side::Buy } else { Side::Sell };
                    let qty = match rng.range(0, 19) {
                        0 => rng.range(-2, 0),
                        1 => rng.range(200_000, 2_000_000),
                        _ => rng.range(1, 5_000),
                    };
                    match rng.range(0, 4) {
                        0 => Order::Market { side, qty },
                        1 => Order::Limit { side, price: rng.range(-1, 0), qty },
                        _ => Order::Limit { side, price: self.mid + rng.range(-6, 6), qty },
                    }
                })
                .collect()
        }
    }

    fn chaos_run(seed: u64) -> Report {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut strategy = Chaos { rng: Rng(rng.next() | 1), mid: P };
        let mut engine = Engine::new().recording_fills();
        let mut mid = P;
        for i in 0..300 {
            mid += rng.range(-3, 3);
            engine.step(&random_book(&mut rng, i * 100, mid), &mut strategy);
            assert!(engine.position().abs() <= POSITION_LIMIT);
        }
        engine.finish()
    }

    #[test]
    fn random_runs_keep_the_invariants() {
        for seed in 0..300 {
            let report = chaos_run(seed);
            assert!(report.max_abs_position <= POSITION_LIMIT, "seed {seed}");
            // Cash, position, fees and counts all follow from the fill log.
            let (mut cash, mut position, mut maker_fees, mut taker_fees) = (STARTING_CASH, 0, 0, 0);
            for f in &report.fills {
                let notional = f.price as Cash * f.qty as Cash;
                let rate = if f.liquidity == Liquidity::Maker { MAKER_FEE_PPM } else { TAKER_FEE_PPM };
                // Ceiling, computed differently from the engine.
                assert_eq!(f.fee, -(-(notional * rate)).div_euclid(1_000_000), "seed {seed}");
                match f.side {
                    Side::Buy => (cash, position) = (cash - notional - f.fee, position + f.qty),
                    Side::Sell => (cash, position) = (cash + notional - f.fee, position - f.qty),
                }
                match f.liquidity {
                    Liquidity::Maker => maker_fees += f.fee,
                    Liquidity::Taker => taker_fees += f.fee,
                }
            }
            assert_eq!(position, 0, "seed {seed}: liquidation leaves the account flat");
            assert_eq!(report.pnl, cash - STARTING_CASH, "seed {seed}");
            assert_eq!((report.maker_fees, report.taker_fees), (maker_fees, taker_fees), "seed {seed}");
            let makers = report.fills.iter().filter(|f| f.liquidity == Liquidity::Maker).count() as u64;
            assert_eq!((report.maker_fills, report.taker_fills), (makers, report.fills.len() as u64 - makers));
        }
    }

    #[test]
    fn random_runs_exercise_every_path() {
        let reports: Vec<Report> = (0..300).map(chaos_run).collect();
        assert!(reports.iter().any(|r| r.maker_fills > 0));
        assert!(reports.iter().any(|r| r.taker_fills > 0));
        for reason in RejectReason::ALL {
            assert!(reports.iter().any(|r| r.rejects[reason as usize] > 0), "{reason} never happened");
        }
        assert!(reports.iter().any(|r| r.liquidation_fallback_qty > 0));
    }

    #[test]
    fn runs_are_deterministic() {
        for seed in 0..20 {
            assert_eq!(chaos_run(seed), chaos_run(seed));
        }
    }
}
