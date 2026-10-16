//! Calm Grid: tiny taker round trips on the top-of-book imbalance cover the minimum volume, and an
//! inventory grid around a slow mid EMA trades only while volatility is low and there is no trend.

use sim::{Account, Book, Info, Order, Side, Strategy};

const REQ_PER_DAY: i64 = 1_000_000;
const SNAPS_PER_DAY: i64 = 864_000;
const RING: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    // Volume engine.
    pub ml_q: i64,
    pub ml_th: f64,
    pub ml_th_force: f64,
    pub ml_hold: u32,
    pub ml_flip: f64,
    pub mult_num: i64,
    pub mult_den: i64,
    pub b_hi: i64,
    pub b_lo: i64,
    pub b_panic: i64,
    // Grid.
    /// Lattice spacing, bps of price.
    pub g_bps: f64,
    /// Lots per level.
    pub q: i64,
    /// Max inventory levels (0 disables the grid).
    pub maxk: i64,
    /// Levels quoted per side.
    pub levels: i64,
    /// Center EMA time constant, calls.
    pub center_tau: f64,
    /// Min distance of a quote from the touch, ticks.
    pub min_gap: i64,
    // Regime filter: trend windows (1 s samples), z-score limits, stop in grid steps, cool-down in calls.
    pub w1: usize,
    pub w2: usize,
    pub w3: usize,
    pub block_z: f64,
    pub flat_z: f64,
    pub stop_g: f64,
    pub cool: i64,
    /// Calm gate on the fast 1 s vol, bps (0 = off).
    pub calm_vmax: f64,
    /// Fast vol EWMA length, 1 s samples.
    pub fast_tau: f64,
    /// Relative calm gate: fast vol must also be below this fraction of the slow vol (0 = off).
    pub calm_rel: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            ml_q: 1_000,
            ml_th: 0.99,
            ml_th_force: 0.9,
            ml_hold: 300,
            ml_flip: 0.9,
            mult_num: 11,
            mult_den: 10,
            b_hi: 10_000,
            b_lo: 3_000,
            b_panic: 1_000,
            g_bps: 3.5,
            q: 5_000,
            maxk: 3,
            levels: 2,
            center_tau: 18_000.0,
            min_gap: 10,
            w1: 60,
            w2: 300,
            w3: 1200,
            block_z: 2.0,
            flat_z: 3.0,
            stop_g: 3.0,
            cool: 3_000,
            calm_vmax: 0.25,
            fast_tau: 600.0,
            calm_rel: 0.0,
        }
    }
}

pub struct MyStrategy {
    pub p: Params,
    n: i64,
    vol: i64,
    last_pos: i64,
    // Volume engine.
    ml_pos: i64,
    ml_held: u32,
    ml_entry_ts: u64,
    ml_pending: u32,
    /// Market order sent by the volume engine at the previous call: (is_buy, qty).
    ml_sent: Option<(bool, i64)>,
    // Grid and regime.
    started: bool,
    g: i64,
    g_mid: f64,
    center: f64,
    ring: Vec<f64>,
    ring_n: usize,
    var1s: f64,
    var_fast: f64,
    grid_last: i64,
    avg: f64,
    block_long_until: i64,
    block_short_until: i64,
    wait: i64,
}

impl Default for MyStrategy {
    fn default() -> Self {
        Self::with(Params::default())
    }
}

fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl MyStrategy {
    pub fn with(p: Params) -> Self {
        MyStrategy {
            p,
            n: 0,
            vol: 0,
            last_pos: 0,
            ml_pos: 0,
            ml_held: 0,
            ml_entry_ts: 0,
            ml_pending: 0,
            ml_sent: None,
            started: false,
            g: 0,
            g_mid: 0.0,
            center: 0.0,
            ring: vec![0.0; RING],
            ring_n: 0,
            var1s: 0.0,
            var_fast: 0.0,
            grid_last: 0,
            avg: 0.0,
            block_long_until: 0,
            block_short_until: 0,
            wait: 0,
        }
    }

    /// Paced volume target for a run of `n` snapshots.
    fn target(&self, n: i64) -> i64 {
        let den = (SNAPS_PER_DAY as i128 * self.p.mult_den.max(1) as i128).max(1);
        (REQ_PER_DAY as i128 * n as i128 * self.p.mult_num as i128 / den) as i64
    }

    /// Mid drift over the last `w` samples, as a z-score.
    fn z(&self, w: usize, now: f64) -> f64 {
        if w == 0 || w >= RING || self.ring_n <= w || self.var1s <= 0.0 {
            return 0.0;
        }
        let past = self.ring[(self.ring_n - 1 - w) % RING];
        (now - past) / (self.var1s * w as f64).sqrt()
    }

    /// Imbalance round trips on their own sub-position. Returns at most one market order.
    fn volume_engine(&mut self, ts_ms: u64, imb: f64) -> Option<Order> {
        let p = self.p;
        if self.ml_pos == 0 {
            self.ml_held = 0;
            if self.ml_pending > 0 {
                self.ml_pending -= 1;
                if self.ml_pending > 0 {
                    return None;
                }
            }
            let t = self.target(self.n + 2);
            if self.vol >= t + p.b_hi {
                return None;
            }
            let go = if self.vol < t + p.b_panic {
                true
            } else if self.vol < t + p.b_lo {
                imb.abs() >= p.ml_th_force
            } else {
                imb.abs() >= p.ml_th
            };
            if !go {
                return None;
            }
            let side = if imb >= 0.0 { Side::Buy } else { Side::Sell };
            self.ml_pending = 2;
            self.ml_pos = if side == Side::Buy { p.ml_q } else { -p.ml_q };
            self.ml_held = 0;
            self.ml_entry_ts = ts_ms;
            return Some(Order::Market { side, qty: p.ml_q });
        }
        self.ml_held = self.ml_held.saturating_add(1);
        let long = self.ml_pos > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.ml_held >= p.ml_hold || ts_ms.saturating_sub(self.ml_entry_ts) >= p.ml_hold as u64 * 100;
        if timed_out || against >= p.ml_flip {
            let side = if long { Side::Sell } else { Side::Buy };
            let qty = self.ml_pos.saturating_abs();
            self.ml_pos = 0;
            self.ml_pending = 0;
            return Some(Order::Market { side, qty });
        }
        None
    }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Calm Grid" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        let p = self.p;
        self.n = self.n.saturating_add(1);
        let pos = account.position;
        let bb = book.best_bid().price;
        let ba = book.best_ask().price;
        let mid = (bb as f64 + ba as f64) * 0.5;
        let imb = imbalance(book);

        self.vol = self.vol.saturating_add(pos.saturating_sub(self.last_pos).saturating_abs());
        self.last_pos = pos;

        // The volume engine's last market order filled against this snapshot: drop any part the
        // book could not fill from its sub-position.
        if let Some((buy, qty)) = self.ml_sent.take() {
            let levels = if buy { book.asks } else { book.bids };
            let avail = levels.iter().fold(0i64, |a, l| a.saturating_add(l.qty));
            let short = qty.saturating_sub(avail).max(0);
            if buy {
                self.ml_pos = self.ml_pos.saturating_sub(short);
            } else {
                self.ml_pos = self.ml_pos.saturating_add(short);
            }
        }
        // Grid inventory is the position minus the volume engine's.
        let gpos = pos.saturating_sub(self.ml_pos);
        let dg = gpos.saturating_sub(self.grid_last);
        if dg != 0 {
            if gpos == 0 {
                self.avg = 0.0;
            } else if self.grid_last == 0 || (self.grid_last > 0) != (gpos > 0) {
                self.avg = mid;
            } else if gpos.saturating_abs() > self.grid_last.saturating_abs() {
                let a = self.grid_last.saturating_abs() as f64;
                let b = (gpos.saturating_abs() - self.grid_last.saturating_abs()) as f64;
                self.avg = (self.avg * a + mid * b) / (a + b);
            }
        }
        self.grid_last = gpos;

        let mut orders: Vec<Order> = Vec::with_capacity(8);
        if let Some(o) = self.volume_engine(ts_ms, imb) {
            if let Order::Market { side, qty } = o {
                self.ml_sent = Some((side == Side::Buy, qty));
            }
            orders.push(o);
        }
        if p.maxk <= 0 {
            return orders;
        }

        if !self.started {
            self.started = true;
            self.center = mid;
        }
        if self.g == 0 || self.g_mid <= 0.0 || (mid / self.g_mid - 1.0).abs() > 0.25 {
            self.g = ((p.g_bps * 1e-4 * mid).round() as i64).max(10);
            self.g_mid = mid;
        }
        let g = self.g;
        self.center += (mid - self.center) / p.center_tau.max(1.0);

        if self.n % 10 == 1 {
            if self.ring_n > 0 {
                let d = mid - self.ring[(self.ring_n - 1) % RING];
                let d2 = (d * d).min(1e10);
                if self.ring_n < 300 {
                    self.var1s += (d2 - self.var1s) / self.ring_n as f64;
                    self.var_fast = self.var1s;
                } else {
                    self.var1s += (d2 - self.var1s) / 1800.0;
                    self.var_fast += (d2 - self.var_fast) / p.fast_tau.max(1.0);
                }
            }
            self.ring[self.ring_n % RING] = mid;
            self.ring_n += 1;
        }
        let mut trend = self.z(p.w1, mid);
        for w in [p.w2, p.w3] {
            let z = self.z(w, mid);
            if z.abs() > trend.abs() {
                trend = z;
            }
        }
        let vfast_bps = self.var_fast.max(0.0).sqrt() / mid.max(1.0) * 1e4;
        let wild = (p.calm_vmax > 0.0 && vfast_bps > p.calm_vmax)
            || (p.calm_rel > 0.0 && self.var_fast > p.calm_rel * p.calm_rel * self.var1s)
            || self.ring_n < 300;

        if self.wait > 0 {
            self.wait -= 1;
        }
        // Flatten the grid on a strong trend against it or on the stop-loss.
        if self.wait == 0 && gpos != 0 {
            let against = (gpos > 0 && trend < -p.flat_z) || (gpos < 0 && trend > p.flat_z);
            let stop =
                if gpos > 0 { mid < self.avg - p.stop_g * g as f64 } else { mid > self.avg + p.stop_g * g as f64 };
            if against || stop {
                let side = if gpos > 0 { Side::Sell } else { Side::Buy };
                if gpos > 0 {
                    self.block_long_until = self.n.saturating_add(p.cool);
                } else {
                    self.block_short_until = self.n.saturating_add(p.cool);
                }
                self.center = mid;
                self.wait = 2;
                orders.push(Order::Market { side, qty: gpos.saturating_abs() });
                return orders;
            }
        }
        if self.wait > 0 {
            return orders;
        }

        let q = p.q.max(1);
        let cq = (self.center / g as f64).round() as i64 * g;
        let k = ((gpos as f64) / q as f64).round() as i64;
        let maxpos = p.maxk.saturating_mul(q).min(200_000);
        let block_long = trend < -p.block_z || self.n < self.block_long_until || wild;
        let block_short = trend > p.block_z || self.n < self.block_short_until || wild;
        let lat_floor = |x: i64| x.div_euclid(g) * g;
        let lat_ceil = |x: i64| -((-x).div_euclid(g) * g);

        let mut have = gpos;
        let mut last_px = i64::MAX;
        for j in 1..=p.levels.clamp(0, 8) {
            let tgt = (k + j) * q;
            if tgt > maxpos || (block_long && tgt > 0) {
                break;
            }
            let qty = tgt - have;
            if qty <= 0 {
                continue;
            }
            let mut px = cq - (k + j) * g;
            if px > bb - p.min_gap {
                px = lat_floor(bb - p.min_gap);
            }
            if px >= last_px {
                px = last_px - g;
            }
            if px <= 0 {
                break;
            }
            orders.push(Order::Limit { side: Side::Buy, price: px, qty });
            last_px = px;
            have = tgt;
        }
        let mut have = gpos;
        let mut last_px = i64::MIN;
        for j in 1..=p.levels.clamp(0, 8) {
            let tgt = (k - j) * q;
            if tgt < -maxpos || (block_short && tgt < 0) {
                break;
            }
            let qty = have - tgt;
            if qty <= 0 {
                continue;
            }
            let mut px = cq - (k - j) * g;
            if px < ba + p.min_gap {
                px = lat_ceil(ba + p.min_gap);
            }
            if px <= last_px {
                px = last_px + g;
            }
            orders.push(Order::Limit { side: Side::Sell, price: px, qty });
            last_px = px;
            have = tgt;
        }
        orders
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
}
