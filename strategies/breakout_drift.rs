//! Breakout Drift: trade breakouts of 1h, 1.5h and 2h price channels with up to 9.9 BTC, skipping
//! narrow channels. Small taker round trips cover the minimum volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Required volume per day of data, in lots (10 BTC).
const REQ_PER_DAY: i64 = 1_000_000;
/// Snapshots per day of data.
const SNAPS_PER_DAY: i64 = 864_000;
/// Position limit in lots.
const LIMIT: i64 = 1_000_000;
/// Maximum number of channels.
const MAX_CH: usize = 4;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Trend size in lots at full conviction (all channels agree).
    pub size: i64,
    /// Channel lookbacks in seconds (0 = unused).
    pub chans: [u64; MAX_CH],
    /// Exit fraction of the channel width (x1000).
    pub ef_milli: i64,
    /// Max lots per market order.
    pub clip: i64,
    /// +1 follows breakouts, -1 fades them.
    pub sgn: i64,
    /// Allowed trend directions.
    pub allow_long: bool,
    pub allow_short: bool,
    /// A feed gap longer than this (seconds) flattens the trend part and restarts the warm-up.
    pub gap_reset: u64,
    /// Minimum channel width for a new entry, in units of 0.1 bps of the mid (0 = none).
    pub min_w: i64,
    /// A drawdown from peak equity beyond `dd_bps` of the full trend size flattens the trend part
    /// for `cool_s` seconds (0 = off).
    pub dd_bps: i64,
    pub cool_s: u64,
    // Volume trips: `q` lots, entered on top-of-book imbalance, keeping volume above
    // `mult_num / mult_den` of the requirement. The `b_*` bands are lots above that target.
    pub q: i64,
    pub th: f64,
    pub th_force: f64,
    pub hold_ms: u64,
    pub flip: f64,
    pub mult_num: i64,
    pub mult_den: i64,
    pub b_hi: i64,
    pub b_lo: i64,
    pub b_panic: i64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            size: 990_000,
            chans: [3_600, 5_400, 7_200, 0],
            ef_milli: 500,
            clip: 100_000,
            sgn: 1,
            allow_long: true,
            allow_short: true,
            gap_reset: 600,
            min_w: 600,
            dd_bps: 300,
            cool_s: 3_600,
            q: 1_000,
            th: 0.99,
            th_force: 0.9,
            hold_ms: 30_000,
            flip: 0.9,
            mult_num: 11,
            mult_den: 10,
            b_hi: 10_000,
            b_lo: 3_000,
            b_panic: 1_000,
        }
    }
}

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

    /// Updates the state for mid `x`: enter on a break of a wide enough channel, exit after
    /// retracing `ef_milli / 1000` of the channel width.
    fn update(&mut self, x: i64, ef_milli: i64, min_w: i64) {
        let (Some(&(_, h)), Some(&(_, l))) = (self.maxq.front(), self.minq.front()) else { return };
        let w = h - l;
        let wide = w as i128 * 100_000 >= min_w as i128 * x as i128;
        if x > h {
            if self.state == 1 || wide {
                self.state = 1;
            } else {
                self.state = 0;
            }
        } else if x < l {
            if self.state == -1 || wide {
                self.state = -1;
            } else {
                self.state = 0;
            }
        } else if (self.state == 1 && (h - x) * 1000 > ef_milli * w)
            || (self.state == -1 && (x - l) * 1000 > ef_milli * w)
        {
            self.state = 0;
        }
    }
}

#[derive(Default)]
pub struct MyStrategy {
    pub p: Params,
    init: bool,
    chans: Vec<Chan>,
    /// First second seen, for the warm-up.
    first_sec: u64,
    /// Current second and the last `mid_x2` seen in it.
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
    /// Peak equity and the end of the current cool-down in seconds.
    peak: i128,
    cool_until: u64,
}

impl MyStrategy {
    #[allow(dead_code)]
    pub fn with(p: Params) -> Self {
        MyStrategy { p, ..Default::default() }
    }

    fn target_vol(&self, n: i64) -> i64 {
        let den = (SNAPS_PER_DAY as i128 * self.p.mult_den.max(1) as i128).max(1);
        (REQ_PER_DAY as i128 * n as i128 * self.p.mult_num as i128 / den) as i64
    }
}

/// Top-of-book imbalance in [-1, 1], positive when the bid is larger.
fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Breakout Drift" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        let p = self.p;
        self.n += 1;
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        self.last_pos = pos;
        let mid = book.mid_x2();
        let sec = ts_ms / 1000;

        if !self.init {
            self.init = true;
            self.chans = p.chans.iter().filter(|&&l| l > 0).map(|&l| Chan { len: l, ..Default::default() }).collect();
            self.first_sec = sec;
            self.cur_sec = sec;
            self.cur_mid = mid;
        }
        if sec > self.cur_sec + p.gap_reset {
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
            // Close the previous second.
            let (s, v) = (self.cur_sec, self.cur_mid);
            for c in self.chans.iter_mut() {
                c.push(s, v);
            }
            self.cur_sec = sec;
        }
        self.cur_mid = mid;

        // Trend target: the average state of the warmed-up channels.
        let mut sum = 0i64;
        let mut k = 0i64;
        for c in self.chans.iter_mut() {
            k += 1;
            c.expire(sec);
            if sec >= self.first_sec + c.len {
                c.update(mid, p.ef_milli, p.min_w);
                if (c.state > 0 && p.allow_long) || (c.state < 0 && p.allow_short) {
                    sum += c.state;
                }
            }
        }
        let mut trend = if k > 0 { p.sgn * p.size * sum / k } else { 0 };

        if p.dd_bps > 0 {
            let eq = account.cash + pos as i128 * mid as i128 / 2;
            if self.n == 1 || eq > self.peak {
                self.peak = eq;
            }
            let lim = p.dd_bps as i128 * p.size as i128 * mid as i128 / 2 / 10_000;
            if self.peak - eq > lim {
                self.cool_until = sec + p.cool_s;
                self.peak = eq;
                for c in self.chans.iter_mut() {
                    c.state = 0;
                }
            }
            if sec < self.cool_until {
                for c in self.chans.iter_mut() {
                    c.state = 0;
                }
                trend = 0;
            }
        }

        let imb = imbalance(book);
        if self.trip != 0 {
            let against = if self.trip > 0 { -imb } else { imb };
            if ts_ms.saturating_sub(self.trip_ts) >= p.hold_ms || against >= p.flip {
                self.trip = 0;
            }
        } else {
            let t = self.target_vol(self.n + 2);
            if self.vol < t + p.b_hi {
                let go = if self.vol < t + p.b_panic {
                    true
                } else if self.vol < t + p.b_lo {
                    imb.abs() >= p.th_force
                } else {
                    imb.abs() >= p.th
                };
                // Skip while the trend position is still moving, its trades count as volume.
                if go && (trend - pos).abs() <= p.q {
                    self.trip = if imb >= 0.0 { 1 } else { -1 };
                    self.trip_ts = ts_ms;
                }
            }
        }

        let mut target = trend + self.trip * p.q;
        target = target.clamp(-LIMIT, LIMIT);
        let diff = target - pos;
        if diff == 0 {
            return Vec::new();
        }
        let qty = diff.abs().min(p.clip.max(1));
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
            assert!(pos.abs() <= LIMIT);
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
    }
}
