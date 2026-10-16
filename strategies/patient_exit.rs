//! Patient Exit: stay flat and meet the minimum volume with small taker round trips, entered on
//! an extreme top-of-book imbalance and closed only on a strong reversal or after 60 seconds.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Required volume per day of data, in lots (10 BTC).
const REQ_PER_DAY: i64 = 1_000_000;
/// Snapshots per day of data.
const SNAPS_PER_DAY: i64 = 864_000;

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
    /// Bands above the paced target, in lots: no trips above `b_hi`, weaker signal below
    /// `b_lo`, enter at once below `b_panic`.
    pub b_hi: i64,
    pub b_lo: i64,
    pub b_panic: i64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            q: 1_000,
            th: 0.99,
            th_force: 0.9,
            hold: 600,
            flip: 0.99,
            mult_num: 11,
            mult_den: 10,
            b_hi: 10_000,
            b_lo: 3_000,
            b_panic: 1_000,
        }
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
}

impl MyStrategy {
    /// A strategy with custom parameters (for experiments).
    #[allow(dead_code)]
    pub fn with(p: Params) -> Self {
        MyStrategy { p, ..Default::default() }
    }

    /// Volume target after `n` snapshots: the requirement times the multiplier.
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
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Patient Exit" }
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
        let imb = imbalance(book);
        let p = self.p;

        if pos == 0 {
            self.held = 0;
            if self.pending > 0 {
                self.pending -= 1;
                if self.pending > 0 {
                    return Vec::new();
                }
            }
            // Target for a run ending a couple of snapshots from now.
            let t = self.target(self.n + 2);
            if self.vol >= t + p.b_hi {
                return Vec::new();
            }
            let go = if self.vol < t + p.b_panic {
                true
            } else if self.vol < t + p.b_lo {
                imb.abs() >= p.th_force
            } else {
                imb.abs() >= p.th
            };
            if !go {
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

    /// Without any imbalance signal, pacing alone keeps volume above the minimum.
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
}
