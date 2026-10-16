//! Quiet Hours: stay flat and meet the minimum volume with small taker round trips on an extreme
//! top-of-book imbalance, building extra volume while the market is calm.

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
    /// Bands above the paced target, in lots. The no-trip cap is `b_quiet` when calm, `b_hi`
    /// normally and `b_loud` when turbulent.
    pub b_hi: i64,
    pub b_quiet: i64,
    pub b_loud: i64,
    pub b_lo: i64,
    pub b_panic: i64,
    /// Volatility regime: fast and slow EWMAs of 1 s absolute mid returns. Calm when
    /// fast/slow < `r_quiet`, turbulent when > `r_loud`.
    pub fast_n: f64,
    pub slow_n: f64,
    pub r_quiet: f64,
    pub r_loud: f64,
    /// Seconds of samples before the regime is trusted.
    pub warm: u32,
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
            b_quiet: 30_000,
            b_loud: 3_000,
            b_lo: 3_000,
            b_panic: 1_000,
            fast_n: 60.0,
            slow_n: 7_200.0,
            r_quiet: 0.5,
            r_loud: 1.5,
            warm: 600,
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
    /// Volatility regime state.
    last_sec: u64,
    last_mid: f64,
    samples: u32,
    fast: f64,
    slow: f64,
}

impl MyStrategy {
    /// A strategy with custom parameters (for experiments).
    #[allow(dead_code)]
    pub fn with(p: Params) -> Self {
        MyStrategy { p, ..Default::default() }
    }

    /// Paced volume target for a run of `n` snapshots.
    fn target(&self, n: i64) -> i64 {
        let den = (SNAPS_PER_DAY as i128 * self.p.mult_den.max(1) as i128).max(1);
        (REQ_PER_DAY as i128 * n as i128 * self.p.mult_num as i128 / den) as i64
    }

    /// Updates the volatility state with one sample per second of market time.
    fn update_vol(&mut self, ts_ms: u64, book: &Book) {
        let sec = ts_ms / 1000;
        if sec == self.last_sec && self.last_mid > 0.0 {
            return;
        }
        let mid = book.mid_x2() as f64;
        if self.last_mid > 0.0 && mid > 0.0 {
            let r = ((mid / self.last_mid).ln() * 1e4).abs();
            let r = if r.is_finite() { r.min(50.0) } else { 0.0 };
            self.samples = self.samples.saturating_add(1);
            let k = self.samples as f64;
            let af = (1.0 / self.p.fast_n.max(1.0)).max(1.0 / k);
            let asl = (1.0 / self.p.slow_n.max(1.0)).max(1.0 / k);
            self.fast += (r - self.fast) * af;
            self.slow += (r - self.slow) * asl;
        }
        self.last_mid = mid;
        self.last_sec = sec;
    }

    /// Regime: -1 calm, 0 normal, 1 turbulent.
    fn regime(&self) -> i8 {
        let p = &self.p;
        if self.samples < p.warm || self.slow <= 1e-9 {
            return 0;
        }
        let ratio = self.fast / self.slow;
        if ratio < p.r_quiet {
            -1
        } else if ratio > p.r_loud {
            1
        } else {
            0
        }
    }
}

/// Top-of-book imbalance in [-1, 1]: positive when the bid holds more than the ask.
fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Quiet Hours" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        if book.bids.is_empty() || book.asks.is_empty() {
            return Vec::new();
        }
        self.n += 1;
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        if self.last_pos == 0 && pos != 0 {
            self.held = 0;
            self.entry_ts = ts_ms;
            self.pending = 0;
        }
        self.last_pos = pos;
        self.update_vol(ts_ms, book);
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
            let band = match self.regime() {
                -1 => p.b_quiet,
                1 => p.b_loud,
                _ => p.b_hi,
            };
            if self.vol >= t + band {
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
