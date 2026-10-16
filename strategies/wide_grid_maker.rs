//! Wide grid maker: rest bids and asks on a fixed price grid, far from the mid and wider when
//! volatile, so they fill only on big moves. Small taker trades top up the minimum volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Nearest quote distance from mid, ticks (lower bound).
    pub dmin: f64,
    /// Nearest distance grows with volatility: max(dmin, volk * sigma of 60 s mid changes).
    pub volk: f64,
    /// Grid spacing, ticks.
    pub grid: i64,
    /// Levels per side.
    pub levels: usize,
    /// Quantity per level, lots.
    pub q: i64,
    /// Max absolute position, lots.
    pub maxpos: i64,
    /// Volume guard safety factor over the pro-rata minimum.
    pub vol_safety: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params { dmin: 1000.0, volk: 0.2, grid: 250, levels: 2, q: 5_000, maxpos: 50_000, vol_safety: 1.5 }
    }
}

pub struct MyStrategy {
    pub p: Params,
    /// Calls so far.
    n: u64,
    last_pos: i64,
    /// Lower bound of traded volume (sum of |position change|), lots.
    vol: i64,
    started: bool,
    last_mid2: i64,
    /// EWMA of the squared 100 ms mid change, ticks^2.
    var: f64,
    /// Alternates the side of guard trades when flat.
    flip: bool,
}

impl Default for MyStrategy {
    fn default() -> Self {
        Self::with_params(Params::default())
    }
}

impl MyStrategy {
    pub fn with_params(p: Params) -> Self {
        MyStrategy { p, n: 0, last_pos: 0, vol: 0, started: false, last_mid2: 0, var: 2.4e4, flip: false }
    }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Wide grid maker" }
    }

    fn call(&mut self, _ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        let p = self.p;
        self.n += 1;
        let pos = account.position;
        let mid2 = book.mid_x2();
        if self.started {
            self.vol = self.vol.saturating_add((pos - self.last_pos).abs());
            let d = (mid2 - self.last_mid2) as f64 * 0.5;
            // About 5 minutes of memory, with single outliers clamped.
            self.var += ((d * d).min(1e8) - self.var) / 3000.0;
        }
        self.started = true;
        self.last_pos = pos;
        self.last_mid2 = mid2;

        let bid = book.best_bid().price;
        let ask = book.best_ask().price;
        let mid = mid2 as f64 * 0.5;
        let sigma60 = (self.var * 600.0).sqrt();
        let dmin = p.dmin.max(p.volk * sigma60);

        let mut orders: Vec<Order> = Vec::with_capacity(2 * p.levels + 1);

        // Volume guard: stay ahead of the pro-rata minimum with small taker trades.
        let need = (p.vol_safety * 1_000_000.0 * self.n as f64 / 864_000.0) as i64;
        let (mut mkt_buy, mut mkt_sell) = (0i64, 0i64);
        if self.n > 3000 && self.vol + 2_000 < need {
            let qty = (need - self.vol).min(5_000);
            let sell = if pos != 0 { pos > 0 } else { self.flip };
            self.flip = !self.flip;
            if sell {
                mkt_sell = qty;
                orders.push(Order::Market { side: Side::Sell, qty });
            } else {
                mkt_buy = qty;
                orders.push(Order::Market { side: Side::Buy, qty });
            }
        }

        let g = p.grid.max(1);
        let mut buy_room = p.maxpos - pos - mkt_buy;
        let mut sell_room = p.maxpos + pos - mkt_sell;
        let top_b = (((mid - dmin) / g as f64).floor() as i64 * g).min(bid);
        let top_a = (((mid + dmin) / g as f64).ceil() as i64 * g).max(ask);
        for i in 0..p.levels as i64 {
            let pb = top_b - i * g;
            if buy_room >= p.q && pb > 0 {
                orders.push(Order::Limit { side: Side::Buy, price: pb, qty: p.q });
                buy_room -= p.q;
            }
            let pa = top_a + i * g;
            if sell_room >= p.q {
                orders.push(Order::Limit { side: Side::Sell, price: pa, qty: p.q });
                sell_room -= p.q;
            }
        }
        orders.truncate(20);
        orders
    }
}
