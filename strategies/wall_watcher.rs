//! Wall Watcher: stay flat and meet the minimum volume with tiny taker round trips timed on the
//! top-of-book imbalance, skipping entries where the touch is a wall the price just ran into.

use sim::{Account, Book, Info, Level, Order, Side, Strategy};

/// Required volume per day of data, in lots (10 BTC).
const REQ_PER_DAY: i64 = 1_000_000;
/// Snapshots per day of data.
const SNAPS_PER_DAY: i64 = 864_000;
/// Remembered wall prices per side.
const WALL_SLOTS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Lots per round-trip leg.
    pub q: i64,
    /// Imbalance threshold for an opportunistic entry.
    pub th: f64,
    /// Imbalance threshold once below the low pacing band.
    pub th_force: f64,
    /// Max holding time in snapshots before a taker exit.
    pub hold: u32,
    /// Exit when the imbalance turns against the position beyond this (>1 = never).
    pub flip: f64,
    /// Pacing multiplier on the pro-rata requirement (num/den).
    pub mult_num: i64,
    pub mult_den: i64,
    /// Bands above the paced target, in lots.
    pub b_hi: i64,
    pub b_lo: i64,
    pub b_panic: i64,
    /// Wall: minimum level size (lots) and distance window behind the touch (ticks).
    pub wall_qty: i64,
    pub wall_min_d: i64,
    pub wall_max_d: i64,
    /// How long a seen wall is remembered (ms).
    pub wall_mem_ms: u64,
    /// Enable the fresh-wall veto.
    pub veto: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            q: 1_000,
            th: 0.99,
            th_force: 0.9,
            hold: 300,
            flip: 0.9,
            mult_num: 11,
            mult_den: 10,
            b_hi: 10_000,
            b_lo: 3_000,
            b_panic: 1_000,
            wall_qty: 100_000,
            wall_min_d: 100,
            wall_max_d: 1_000,
            wall_mem_ms: 3_000,
            veto: true,
        }
    }
}

/// Prices of recently seen deep walls on one side, with the time last seen deep.
#[derive(Clone, Copy, Default)]
struct Walls {
    px: [i64; WALL_SLOTS],
    ts: [u64; WALL_SLOTS],
}

impl Walls {
    /// Records the deep walls of one side (`sgn` = 1 for bids, -1 for asks).
    fn observe(&mut self, levels: &[Level], sgn: i64, ts: u64, p: &Params) {
        let Some(top) = levels.first() else { return };
        let best = top.price;
        for l in levels.iter().skip(1).take(64) {
            let d = sgn * (best - l.price);
            if d > p.wall_max_d {
                break;
            }
            if d < p.wall_min_d || l.qty < p.wall_qty {
                continue;
            }
            // Refresh an existing slot, else replace the oldest one.
            let mut slot = 0usize;
            let mut oldest = u64::MAX;
            for i in 0..WALL_SLOTS {
                if self.ts[i] != 0 && self.px[i] == l.price {
                    slot = i;
                    break;
                }
                if self.ts[i] < oldest {
                    oldest = self.ts[i];
                    slot = i;
                }
            }
            self.px[slot] = l.price;
            self.ts[slot] = ts.max(1);
        }
    }

    /// Whether `price` was seen as a deep wall within `mem_ms` before `ts`.
    fn recent(&self, price: i64, ts: u64, mem_ms: u64) -> bool {
        (0..WALL_SLOTS).any(|i| self.ts[i] != 0 && self.px[i] == price && ts.saturating_sub(self.ts[i]) <= mem_ms)
    }
}

#[derive(Default)]
pub struct MyStrategy {
    pub p: Params,
    /// Snapshots seen.
    n: i64,
    /// Volume traded so far (lots), from position changes.
    vol: i64,
    last_pos: i64,
    /// Snapshots spent in the current position.
    held: u32,
    /// Timestamp of the entry fill.
    entry_ts: u64,
    /// Calls to wait for an entry order to show up in the position.
    pending: u32,
    bid_walls: Walls,
    ask_walls: Walls,
    /// Entries vetoed so far.
    pub vetoes: u64,
}

impl MyStrategy {
    /// A strategy with custom parameters.
    #[allow(dead_code)]
    pub fn with(p: Params) -> Self {
        MyStrategy { p, ..Default::default() }
    }

    /// Paced volume target for a run of `n` snapshots.
    fn target(&self, n: i64) -> i64 {
        let den = (SNAPS_PER_DAY as i128 * self.p.mult_den.max(1) as i128).max(1);
        (REQ_PER_DAY as i128 * n as i128 * self.p.mult_num as i128 / den) as i64
    }
}

/// Top-of-book imbalance in [-1, 1]: positive when the bid holds more than the ask.
fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Wall Watcher" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.n += 1;
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        if self.last_pos == 0 && pos != 0 {
            self.held = 0;
            self.entry_ts = ts_ms;
            self.pending = 0;
        }
        self.last_pos = pos;
        let p = self.p;

        if book.bids.is_empty() || book.asks.is_empty() {
            // One-sided book: flatten if possible.
            if pos > 0 && !book.bids.is_empty() {
                return vec![Order::Market { side: Side::Sell, qty: pos }];
            }
            if pos < 0 && !book.asks.is_empty() {
                return vec![Order::Market { side: Side::Buy, qty: -pos }];
            }
            return Vec::new();
        }

        // Check the touch against walls seen before this snapshot.
        let fresh_bid = self.bid_walls.recent(book.bids[0].price, ts_ms, p.wall_mem_ms);
        let fresh_ask = self.ask_walls.recent(book.asks[0].price, ts_ms, p.wall_mem_ms);
        self.bid_walls.observe(book.bids, 1, ts_ms, &p);
        self.ask_walls.observe(book.asks, -1, ts_ms, &p);

        let imb = imbalance(book);

        if pos == 0 {
            self.held = 0;
            if self.pending > 0 {
                self.pending -= 1;
                if self.pending > 0 {
                    return Vec::new();
                }
            }
            let t = self.target(self.n + 2);
            if self.vol >= t + p.b_hi {
                return Vec::new();
            }
            let panic = self.vol < t + p.b_panic;
            let go = if panic {
                true
            } else if self.vol < t + p.b_lo {
                imb.abs() >= p.th_force
            } else {
                imb.abs() >= p.th
            };
            if !go {
                return Vec::new();
            }
            // Veto: the heavy side of the touch is a wall the price just ran into.
            let heavy_fresh = if imb >= 0.0 { fresh_bid } else { fresh_ask };
            if p.veto && !panic && heavy_fresh {
                self.vetoes += 1;
                return Vec::new();
            }
            let side = if imb >= 0.0 { Side::Buy } else { Side::Sell };
            self.pending = 2;
            return vec![Order::Market { side, qty: p.q }];
        }

        // In a position: hold while the book still leans our way.
        self.held = self.held.saturating_add(1);
        let long = pos > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.held >= p.hold || ts_ms.saturating_sub(self.entry_ts) >= p.hold as u64 * 100;
        if timed_out || against >= p.flip {
            let side = if long { Side::Sell } else { Side::Buy };
            return vec![Order::Market { side, qty: pos.abs() }];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod own_tests {
    use super::*;
    use sim::{Level, STARTING_CASH};

    /// With no imbalance signal, pacing alone keeps volume above the minimum.
    #[test]
    fn pacing_without_signal() {
        let bids = [Level { price: 8_000_000, qty: 100_000 }];
        let asks = [Level { price: 8_000_001, qty: 100_000 }];
        let book = Book { bids: &bids, asks: &asks };
        let mut s = MyStrategy::default();
        let (mut pos, mut vol) = (0i64, 0i64);
        let mut pending: Vec<Order> = Vec::new();
        for n in 1..=200_000i64 {
            for o in pending.drain(..) {
                if let Order::Market { side, qty } = o {
                    pos += if side == Side::Buy { qty } else { -qty };
                    vol += qty;
                }
            }
            if n >= 2 {
                assert!(vol >= REQ_PER_DAY * n / SNAPS_PER_DAY, "behind at snapshot {n}");
            }
            assert!(pos.abs() <= 1_000);
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
    }

    /// A bid wall that was deep and is now the touch vetoes a buy on a strong bid imbalance.
    #[test]
    fn fresh_wall_vetoes_entry() {
        let mut s = MyStrategy::default();
        let acc = Account { position: 0, cash: STARTING_CASH, resting: &[] };
        // Ahead of pace, so the veto applies.
        s.vol = 5_000;
        let bids1 = [Level { price: 8_000_500, qty: 1_000 }, Level { price: 8_000_000, qty: 500_000 }];
        let asks1 = [Level { price: 8_000_501, qty: 1_000 }];
        let _ = s.call(100, &Book { bids: &bids1, asks: &asks1 }, &acc);
        let bids2 = [Level { price: 8_000_000, qty: 500_000 }];
        let asks2 = [Level { price: 8_000_001, qty: 1 }];
        let out = s.call(200, &Book { bids: &bids2, asks: &asks2 }, &acc);
        assert!(out.is_empty());
        assert_eq!(s.vetoes, 1);
        // Once the wall is forgotten, the same book is an entry again.
        let out = s.call(10_000, &Book { bids: &bids2, asks: &asks2 }, &acc);
        assert_eq!(out.len(), 1);
    }
}
