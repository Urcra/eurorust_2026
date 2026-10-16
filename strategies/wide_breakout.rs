//! Wide Breakout: join breakouts of 1h, 1.5h and 2h price channels with a small 3 BTC position, but
//! only when the channel is already at least 1% wide. Small taker round trips cover the volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Required volume per day of data, in lots (10 BTC).
const REQ_PER_DAY: i64 = 1_000_000;
/// Snapshots per day of data.
const SNAPS_PER_DAY: i64 = 864_000;
/// Position limit in lots.
const LIMIT: i64 = 1_000_000;

/// Trend size when all channels agree, in lots (3 BTC, 1 BTC per channel).
const SIZE: i64 = 300_000;
/// Channel lookbacks in seconds.
const CHANS: [u64; 3] = [3_600, 5_400, 7_200];
/// Minimum channel width for an entry, in units of 0.1 bps of the mid (1000 = 1%).
const MIN_W: i64 = 1_000;
/// Max lots per market order.
const CLIP: i64 = 100_000;
/// A feed gap longer than this (seconds) flattens the trend part and restarts the warm-up.
const GAP_RESET: u64 = 600;

// Volume trips: 0.01 BTC, entered on top-of-book imbalance, paced against the requirement.
const Q: i64 = 1_000;
const TH: f64 = 0.99;
const TH_FORCE: f64 = 0.9;
const TH_PANIC: f64 = 0.5;
const FLIP: f64 = 0.9;
/// Max holding time of a trip, in ms (2 minutes).
const HOLD_MS: u64 = 120_000;
// Volume surplus bands, in lots above the requirement.
const B_HI: i64 = 15_000;
const B_LO: i64 = 2_500;
const B_PANIC: i64 = 1_500;
const B_EMERG: i64 = 800;
const B_EXIT: i64 = 600;
// Volatility gate: prefer trips when short-term volatility is low relative to its history.
const F_MIN: f64 = 0.05;
const GAMMA: i32 = 4;
const RV_A: f64 = 1.0 / 600.0;
const RING: usize = 512;
const SAMP: u32 = 50;
const WARM: usize = 20;

/// Monotonic-deque rolling max and min over (second, value) samples.
#[derive(Default, Clone)]
struct Chan {
    len: u64,
    /// Decreasing values (front = max).
    maxq: std::collections::VecDeque<(u64, i64)>,
    /// Increasing values (front = min).
    minq: std::collections::VecDeque<(u64, i64)>,
    state: i64,
}

impl Chan {
    fn push(&mut self, sec: u64, v: i64) {
        while let Some(&(_, b)) = self.maxq.back() {
            if b <= v {
                self.maxq.pop_back();
            } else {
                break;
            }
        }
        self.maxq.push_back((sec, v));
        while let Some(&(_, b)) = self.minq.back() {
            if b >= v {
                self.minq.pop_back();
            } else {
                break;
            }
        }
        self.minq.push_back((sec, v));
    }

    /// Drops samples older than `len` seconds before `now_sec`.
    fn expire(&mut self, now_sec: u64) {
        let cut = now_sec.saturating_sub(self.len);
        while let Some(&(s, _)) = self.maxq.front() {
            if s < cut {
                self.maxq.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(s, _)) = self.minq.front() {
            if s < cut {
                self.minq.pop_front();
            } else {
                break;
            }
        }
    }

    /// Stop-and-reverse breakout state with a width filter on entries.
    fn update(&mut self, x: i64) {
        let (Some(&(_, h)), Some(&(_, l))) = (self.maxq.front(), self.minq.front()) else { return };
        let wide = (h - l) as i128 * 100_000 >= MIN_W as i128 * x as i128;
        if x > h {
            self.state = if self.state == 1 || wide { 1 } else { 0 };
        } else if x < l {
            self.state = if self.state == -1 || wide { -1 } else { 0 };
        }
    }
}

pub struct MyStrategy {
    init: bool,
    chans: Vec<Chan>,
    first_sec: u64,
    cur_sec: u64,
    cur_mid: i64,
    /// Snapshots seen.
    n: i64,
    /// Volume traded so far in lots, from position changes.
    vol: i64,
    last_pos: i64,
    /// Volume trip: direction (-1, 0, +1) and start time.
    trip: i64,
    trip_ts: u64,
    // Short-term volatility state.
    prev_mid: f64,
    rv: f64,
    ring: [f32; RING],
    ring_i: usize,
    ring_n: usize,
    samp_ctr: u32,
}

impl Default for MyStrategy {
    fn default() -> Self {
        MyStrategy {
            init: false,
            chans: Vec::new(),
            first_sec: 0,
            cur_sec: 0,
            cur_mid: 0,
            n: 0,
            vol: 0,
            last_pos: 0,
            trip: 0,
            trip_ts: 0,
            prev_mid: 0.0,
            rv: 0.0,
            ring: [0.0; RING],
            ring_i: 0,
            ring_n: 0,
            samp_ctr: 0,
        }
    }
}

impl MyStrategy {
    fn required(n: i64) -> i64 {
        (REQ_PER_DAY as i128 * n as i128 / SNAPS_PER_DAY as i128) as i64
    }

    /// Fraction of the volatility history strictly below `x`.
    fn rank(&self, x: f64) -> f64 {
        let n = self.ring_n.min(RING);
        if n == 0 {
            return 0.0;
        }
        let x = x as f32;
        let c = self.ring[..n].iter().filter(|&&v| v < x).count();
        c as f64 / n as f64
    }

    fn update_vol(&mut self, book: &Book) {
        let mid = (book.bids[0].price + book.asks[0].price) as f64 * 0.5;
        if self.n == 1 || self.prev_mid <= 0.0 {
            self.prev_mid = mid;
        }
        if mid > 0.0 {
            let d = (mid - self.prev_mid).abs() / mid * 1e4;
            if d.is_finite() {
                self.rv += RV_A * (d - self.rv);
            }
        }
        self.prev_mid = mid;
        self.samp_ctr += 1;
        if self.samp_ctr >= SAMP && self.n > 600 {
            self.samp_ctr = 0;
            self.ring[self.ring_i] = self.rv as f32;
            self.ring_i = (self.ring_i + 1) % RING;
            self.ring_n += 1;
        }
    }

    /// Whether to start a trip, given the volume surplus `s` over the requirement.
    fn want_trip(&self, s: i64, imb: f64) -> bool {
        if s >= B_HI {
            false
        } else if s < B_EMERG {
            true
        } else if s < B_PANIC {
            imb.abs() >= TH_PANIC
        } else if s < B_LO {
            imb.abs() >= TH_FORCE
        } else if imb.abs() >= TH {
            if self.ring_n >= WARM {
                let x = ((B_HI - s) as f64 / (B_HI - B_LO) as f64).clamp(0.0, 1.0);
                let f = F_MIN + (1.0 - F_MIN) * x.powi(GAMMA);
                self.rank(self.rv) <= f
            } else {
                true
            }
        } else {
            false
        }
    }
}

/// Top-of-book imbalance in [-1, 1], positive when the bid is larger.
fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Wide Breakout" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.n += 1;
        self.update_vol(book);
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        self.last_pos = pos;
        let mid = book.mid_x2();
        let sec = ts_ms / 1000;

        if !self.init {
            self.init = true;
            self.chans = CHANS.iter().map(|&l| Chan { len: l, ..Default::default() }).collect();
            self.first_sec = sec;
            self.cur_sec = sec;
            self.cur_mid = mid;
        }
        if sec > self.cur_sec + GAP_RESET {
            // After a long feed gap the channels are stale, so start over.
            for c in self.chans.iter_mut() {
                c.maxq.clear();
                c.minq.clear();
                c.state = 0;
            }
            self.first_sec = sec;
            self.cur_sec = sec;
        }
        if sec != self.cur_sec {
            let (s, v) = (self.cur_sec, self.cur_mid);
            for c in self.chans.iter_mut() {
                c.push(s, v);
            }
            self.cur_sec = sec;
        }
        self.cur_mid = mid;

        // Trend target: the average state of the warmed-up channels.
        let mut sum = 0i64;
        for c in self.chans.iter_mut() {
            c.expire(sec);
            if sec >= self.first_sec + c.len {
                c.update(mid);
                sum += c.state;
            }
        }
        let trend = SIZE * sum / CHANS.len() as i64;

        let imb = imbalance(book);
        let s = self.vol - Self::required(self.n + 2);
        if self.trip != 0 {
            let against = if self.trip > 0 { -imb } else { imb };
            if ts_ms.saturating_sub(self.trip_ts) >= HOLD_MS || against >= FLIP || s < B_EXIT {
                self.trip = 0;
            }
        } else if (trend - pos).abs() <= Q && self.want_trip(s, imb) {
            // Only while the trend position is not moving, its trades count as volume.
            self.trip = if imb >= 0.0 { 1 } else { -1 };
            self.trip_ts = ts_ms;
        }

        let target = (trend + self.trip * Q).clamp(-LIMIT, LIMIT);
        let diff = target - pos;
        if diff == 0 {
            return Vec::new();
        }
        let qty = diff.abs().min(CLIP);
        let side = if diff > 0 { Side::Buy } else { Side::Sell };
        vec![Order::Market { side, qty }]
    }
}

#[cfg(test)]
mod own_tests {
    use super::*;
    use sim::{Level, STARTING_CASH};

    /// With no imbalance signal, pacing alone keeps volume above the requirement.
    #[test]
    fn pacing_without_signal() {
        let bids = [Level { price: 8_000_000, qty: 100_000 }];
        let asks = [Level { price: 8_000_001, qty: 100_000 }];
        let book = Book { bids: &bids, asks: &asks };
        let mut s = MyStrategy::default();
        let (mut pos, mut vol) = (0i64, 0i64);
        let mut pending: Vec<Order> = Vec::new();
        for n in 1..=300_000i64 {
            for o in pending.drain(..) {
                if let Order::Market { side, qty } = o {
                    pos += if side == Side::Buy { qty } else { -qty };
                    vol += qty;
                }
            }
            if n >= 2 {
                assert!(vol >= REQ_PER_DAY * n / SNAPS_PER_DAY, "behind at snapshot {n}");
            }
            assert!(pos.abs() <= LIMIT);
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
    }
}
