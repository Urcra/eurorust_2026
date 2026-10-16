//! Wide net: rest a small limit-order ladder on each side, $15 from the mid, to catch sharp sweeps
//! that tend to revert, and hold the inventory until the other side fills.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Tunable parameters. Prices in ticks (0.01 USD), quantities in lots (1e-5 BTC).
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Distance from the mid to the first quote on each side.
    pub dist: i64,
    /// Spacing between ladder levels.
    pub step: i64,
    /// Ladder levels per side.
    pub levels: usize,
    /// Lots per level.
    pub qty: i64,
    /// Maximum absolute position.
    pub max_pos: i64,
    /// A side is re-centred only when its desired price is more than this away from its anchor.
    pub rehang: i64,
    /// Re-centred orders stay at least this many ticks behind the touch.
    pub min_gap: i64,
    /// Fallback market order size when volume falls behind schedule.
    pub fb_qty: i64,
    /// Minimum calls between fallback market orders.
    pub fb_every: u64,
}

pub const DEFAULT_PARAMS: Params = Params {
    dist: 1_500,
    step: 600,
    levels: 3,
    qty: 10_000,
    max_pos: 60_000,
    rehang: 300,
    min_gap: 100,
    fb_qty: 2_000,
    fb_every: 50,
};

/// Exchange position limit (10 BTC).
const LIMIT: i64 = 1_000_000;
/// Required volume: 10 BTC (1,000,000 lots) per 864,000 snapshots.
const REQ_LOTS: i128 = 1_000_000;
const REQ_SNAPS: i128 = 864_000;

pub struct MyStrategy {
    p: Params,
    calls: u64,
    /// Lots traded so far (lower bound: sum of |position change| between calls).
    volume: i64,
    prev_pos: i64,
    /// Current price of the first bid / ask level; 0 before the first call.
    bid_anchor: i64,
    ask_anchor: i64,
    last_fb: u64,
}

impl Default for MyStrategy {
    fn default() -> Self {
        Self::with_params(DEFAULT_PARAMS)
    }
}

impl MyStrategy {
    pub fn with_params(p: Params) -> Self {
        Self { p, calls: 0, volume: 0, prev_pos: 0, bid_anchor: 0, ask_anchor: 0, last_fb: 0 }
    }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Wide net" }
    }

    fn call(&mut self, _ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        let p = self.p;
        self.calls = self.calls.saturating_add(1);
        let pos = account.position;
        self.volume = self.volume.saturating_add(pos.saturating_sub(self.prev_pos).saturating_abs());
        self.prev_pos = pos;

        let bb = book.best_bid();
        let ba = book.best_ask();
        let mid_x2 = bb.price.saturating_add(ba.price);
        // Desired first-level prices, rounded away from the mid. Each side keeps its price until this
        // drifts too far, so resting orders survive between calls and can fill passively.
        let want_bid = (mid_x2 - 2 * p.dist).div_euclid(2);
        let want_ask = (mid_x2 + 2 * p.dist + 1).div_euclid(2);
        if self.bid_anchor == 0 || (want_bid - self.bid_anchor).abs() > p.rehang {
            self.bid_anchor = want_bid.min(bb.price - p.min_gap);
        }
        if self.ask_anchor == 0 || (want_ask - self.ask_anchor).abs() > p.rehang {
            self.ask_anchor = want_ask.max(ba.price + p.min_gap);
        }

        let mut orders = Vec::with_capacity(2 * p.levels + 1);

        // Volume fallback: when behind schedule (plus 25%), take a small clip that reduces the
        // position, or follows the top-of-book imbalance when flat.
        let required = (self.calls as i128 * REQ_LOTS / REQ_SNAPS) as i64;
        let target = required + required / 4 + 2 * p.fb_qty;
        let (mut mkt_buy, mut mkt_sell) = (0, 0);
        if self.volume < target && self.calls >= self.last_fb.saturating_add(p.fb_every) {
            self.last_fb = self.calls;
            let buy = if pos != 0 { pos < 0 } else { bb.qty >= ba.qty };
            let q = if pos != 0 { pos.saturating_abs().min(p.fb_qty) } else { p.fb_qty };
            if buy {
                orders.push(Order::Market { side: Side::Buy, qty: q });
                mkt_buy = q;
            } else {
                orders.push(Order::Market { side: Side::Sell, qty: q });
                mkt_sell = q;
            }
        }

        let max_pos = p.max_pos.min(LIMIT);
        let mut room = max_pos - pos - mkt_buy;
        for i in 0..p.levels {
            let q = p.qty.min(room);
            let price = self.bid_anchor - p.step * i as i64;
            if q <= 0 || price <= 0 {
                break;
            }
            orders.push(Order::Limit { side: Side::Buy, price, qty: q });
            room -= q;
        }
        let mut room = max_pos + pos - mkt_sell;
        for i in 0..p.levels {
            let q = p.qty.min(room);
            if q <= 0 {
                break;
            }
            let price = self.ask_anchor + p.step * i as i64;
            orders.push(Order::Limit { side: Side::Sell, price, qty: q });
            room -= q;
        }
        orders
    }
}
