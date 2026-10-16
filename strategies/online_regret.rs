//! Online Regret: stay flat and meet the minimum volume with small taker round trips, timed by a
//! ridge regression on top-of-book features that is learned online during the run.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Required volume per day of data, in lots (10 BTC).
const REQ_PER_DAY: i64 = 1_000_000;
/// Snapshots per day of data.
const SNAPS_PER_DAY: i64 = 864_000;
/// Number of model features.
const K: usize = 5;
/// History ring length (must exceed the horizon + 2).
const RING: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Lots per round-trip leg.
    pub q: i64,
    /// Model horizon in snapshots.
    pub h: usize,
    /// Ridge memory in snapshots (forgetting factor 1 - 1/mem).
    pub mem: f64,
    /// Ridge penalty per unit of (forgotten) sample count.
    pub delta: f64,
    /// Effective sample count before the model is used.
    pub warm: f64,
    /// Target exceedance rate of |prediction| for a model entry.
    pub alpha: f64,
    /// Minimum online correlation between predictions and outcomes to use the model.
    pub cmin: f64,
    /// Baseline imbalance thresholds (model not ready).
    pub th: f64,
    pub th_force: f64,
    /// Max holding time in snapshots.
    pub hold: u32,
    /// Exit when the imbalance turns against the position beyond this.
    pub flip: f64,
    /// Pacing multiplier on the pro-rata requirement, per mille.
    pub mult: i64,
    /// Bands above the paced target, in lots: no trips above `b_hi`, weaker signal below
    /// `b_lo`, enter or exit at once below `b_panic`.
    pub b_hi: i64,
    pub b_lo: i64,
    pub b_panic: i64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            q: 1_000,
            h: 30,
            mem: 100_000.0,
            delta: 0.01,
            warm: 20_000.0,
            alpha: 0.01,
            cmin: 0.0,
            th: 0.99,
            th_force: 0.9,
            hold: 3_000,
            flip: 0.99,
            mult: 1_100,
            b_hi: 10_000,
            b_lo: 3_000,
            b_panic: 1_000,
        }
    }
}

impl Params {
    /// Sets a parameter by name (for experiments).
    #[allow(dead_code)]
    pub fn set(&mut self, k: &str, v: f64) {
        match k {
            "q" => self.q = v as i64,
            "h" => self.h = v as usize,
            "mem" => self.mem = v,
            "delta" => self.delta = v,
            "warm" => self.warm = v,
            "alpha" => self.alpha = v,
            "cmin" => self.cmin = v,
            "th" => self.th = v,
            "thf" => self.th_force = v,
            "hold" => self.hold = v as u32,
            "flip" => self.flip = v,
            "mult" => self.mult = v as i64,
            "bhi" => self.b_hi = v as i64,
            "blo" => self.b_lo = v as i64,
            "bpanic" => self.b_panic = v as i64,
            _ => {}
        }
    }
}

#[inline]
fn clampf(x: f64, lo: f64, hi: f64) -> f64 {
    if x.is_finite() { x.max(lo).min(hi) } else { 0.0 }
}

/// Top-of-book features: imbalance, imbalance cubed, and order-flow imbalance at the touch
/// (last snapshot plus 10- and 100-snapshot EMAs) scaled by the typical depth.
#[derive(Clone, Default)]
struct Feat {
    started: bool,
    bid_px: i64,
    bid_qty: f64,
    ask_px: i64,
    ask_qty: f64,
    /// EMA of the mean touch quantity.
    depth: f64,
    ofi10: f64,
    ofi100: f64,
}

impl Feat {
    fn update(&mut self, book: &Book) -> [f64; K] {
        let b = book.bids[0];
        let a = book.asks[0];
        let bq = b.qty.max(0) as f64;
        let aq = a.qty.max(0) as f64;
        if !self.started {
            self.started = true;
            self.bid_px = b.price;
            self.bid_qty = bq;
            self.ask_px = a.price;
            self.ask_qty = aq;
            self.depth = (bq + aq) * 0.5;
        }
        // Order-flow imbalance at the touch.
        let eb = if b.price > self.bid_px {
            bq
        } else if b.price == self.bid_px {
            bq - self.bid_qty
        } else {
            -self.bid_qty
        };
        let ea = if a.price < self.ask_px {
            aq
        } else if a.price == self.ask_px {
            aq - self.ask_qty
        } else {
            -self.ask_qty
        };
        let ofi = eb - ea;
        self.depth += 0.01 * ((bq + aq) * 0.5 - self.depth);
        if !self.depth.is_finite() {
            self.depth = (bq + aq) * 0.5;
        }
        let dep = self.depth.max(1.0);
        self.ofi10 += 0.1 * (ofi - self.ofi10);
        self.ofi100 += 0.01 * (ofi - self.ofi100);
        if !self.ofi10.is_finite() || !self.ofi100.is_finite() {
            self.ofi10 = 0.0;
            self.ofi100 = 0.0;
        }
        let imb = if bq + aq > 0.0 { (bq - aq) / (bq + aq) } else { 0.0 };
        self.bid_px = b.price;
        self.bid_qty = bq;
        self.ask_px = a.price;
        self.ask_qty = aq;
        [
            imb,
            imb * imb * imb,
            clampf(ofi / dep, -5.0, 5.0),
            clampf(self.ofi10 / dep * 3.0, -5.0, 5.0),
            clampf(self.ofi100 / dep * 10.0, -5.0, 5.0),
        ]
    }
}

/// Exponentially forgotten ridge regression, solved by Cholesky.
#[derive(Clone)]
struct Ridge {
    /// Lower triangle of the forgotten X'X.
    a: [[f64; K]; K],
    /// Forgotten X'y.
    b: [f64; K],
    w: [f64; K],
    lam: f64,
    delta: f64,
    /// Forgotten sample count.
    cnt: f64,
}

impl Ridge {
    fn new(lam: f64, delta: f64) -> Self {
        Ridge { a: [[0.0; K]; K], b: [0.0; K], w: [0.0; K], lam, delta, cnt: 0.0 }
    }

    #[allow(clippy::needless_range_loop)]
    fn update(&mut self, x: &[f64; K], y: f64) {
        if !y.is_finite() || x.iter().any(|v| !v.is_finite()) {
            return;
        }
        let l = self.lam;
        for i in 0..K {
            let xi = x[i];
            for j in 0..=i {
                self.a[i][j] = self.a[i][j] * l + xi * x[j];
            }
            self.b[i] = self.b[i] * l + xi * y;
        }
        self.cnt = self.cnt * l + 1.0;
    }

    /// Solves (A + delta * cnt * I) w = b; keeps the old weights if anything is off.
    // `!(s > eps)` on purpose: it is also true for NaN.
    #[allow(clippy::needless_range_loop, clippy::neg_cmp_op_on_partial_ord)]
    fn solve(&mut self) {
        let mut m = [[0.0f64; K]; K];
        let reg = self.delta * self.cnt + 1e-9;
        for i in 0..K {
            m[i][..=i].copy_from_slice(&self.a[i][..=i]);
            m[i][i] += reg;
        }
        for j in 0..K {
            let mut s = m[j][j];
            for k in 0..j {
                s -= m[j][k] * m[j][k];
            }
            if !(s > 1e-12) || !s.is_finite() {
                return;
            }
            let d = s.sqrt();
            m[j][j] = d;
            for i in j + 1..K {
                let mut t = m[i][j];
                for k in 0..j {
                    t -= m[i][k] * m[j][k];
                }
                m[i][j] = t / d;
            }
        }
        let mut z = [0.0; K];
        for i in 0..K {
            let mut t = self.b[i];
            for k in 0..i {
                t -= m[i][k] * z[k];
            }
            z[i] = t / m[i][i];
        }
        let mut w = [0.0; K];
        for i in (0..K).rev() {
            let mut t = z[i];
            for k in i + 1..K {
                t -= m[k][i] * w[k];
            }
            w[i] = t / m[i][i];
        }
        if w.iter().all(|v| v.is_finite() && v.abs() < 1e6) {
            self.w = w;
        }
    }

    fn predict(&self, x: &[f64; K]) -> f64 {
        let s: f64 = self.w.iter().zip(x).map(|(w, x)| w * x).sum();
        if s.is_finite() { s } else { 0.0 }
    }
}

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
    feat: Feat,
    ridge: Ridge,
    /// Feature, mid and raw-prediction history (ring buffers).
    xs: [[f64; K]; RING],
    mids: [f64; RING],
    preds: [f64; RING],
    /// Online (1 - alpha) quantile of |prediction|, in bps.
    thr: f64,
    /// EMAs of pred*y, pred^2 and y^2 (online skill).
    s_py: f64,
    s_pp: f64,
    s_yy: f64,
}

impl Default for MyStrategy {
    fn default() -> Self {
        Self::with(Params::default())
    }
}

impl MyStrategy {
    /// A strategy with custom parameters (for experiments).
    pub fn with(p: Params) -> Self {
        let mem = if p.mem.is_finite() && p.mem > 10.0 { p.mem } else { 10.0 };
        MyStrategy {
            p,
            n: 0,
            vol: 0,
            last_pos: 0,
            held: 0,
            entry_ts: 0,
            pending: 0,
            feat: Feat::default(),
            ridge: Ridge::new(1.0 - 1.0 / mem, p.delta),
            xs: [[0.0; K]; RING],
            mids: [0.0; RING],
            preds: [0.0; RING],
            thr: 0.5,
            s_py: 0.0,
            s_pp: 0.0,
            s_yy: 0.0,
        }
    }

    /// Paced volume target for a run of `n` snapshots.
    fn target(&self, n: i64) -> i64 {
        let mult = self.p.mult.clamp(1_000, 3_000) as i128;
        (REQ_PER_DAY as i128 * n as i128 * mult / (SNAPS_PER_DAY as i128 * 1_000)) as i64
    }

    /// Learns from the snapshot whose outcome just became known and returns the prediction in
    /// bps, or 0 when the model is not ready or not working.
    fn learn(&mut self, book: &Book) -> f64 {
        let x = self.feat.update(book);
        let mid = (book.bids[0].price + book.asks[0].price) as f64 * 0.5;
        let t = (self.n - 1).max(0) as usize;
        let h = self.p.h.clamp(1, RING - 2);
        let i = t % RING;
        self.xs[i] = x;
        self.mids[i] = mid;
        if t > h {
            // Features from t-1-h; an order sent then executed at t-h.
            let si = (t - 1 - h) % RING;
            let e = (t - h) % RING;
            let m0 = self.mids[e];
            if m0 > 0.0 {
                let y = clampf((mid - m0) / m0 * 1e4, -100.0, 100.0);
                let xo = self.xs[si];
                self.ridge.update(&xo, y);
                let po = self.preds[si];
                self.s_py += 1e-4 * (po * y - self.s_py);
                self.s_pp += 1e-4 * (po * po - self.s_pp);
                self.s_yy += 1e-4 * (y * y - self.s_yy);
            }
        }
        if t.is_multiple_of(10) {
            self.ridge.solve();
        }
        let warm = self.ridge.cnt >= self.p.warm.min(0.9 * self.p.mem);
        let raw = if warm { self.ridge.predict(&x) } else { 0.0 };
        self.preds[i] = raw;
        if raw != 0.0 {
            let up = if raw.abs() > self.thr { 1.0 } else { 0.0 };
            self.thr = clampf(self.thr * (0.01 * (up - self.p.alpha)).exp(), 1e-4, 100.0);
        }
        let corr = self.s_py / (self.s_pp * self.s_yy).sqrt().max(1e-12);
        let skilled = corr.is_finite() && corr >= self.p.cmin;
        if skilled { raw } else { 0.0 }
    }
}

/// Top-of-book imbalance in [-1, 1]: positive when the bid holds more than the ask.
fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Online Regret" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        if book.bids.is_empty() || book.asks.is_empty() {
            return Vec::new();
        }
        self.n += 1;
        let pred = self.learn(book);
        let model_on = pred != 0.0;
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
        let t = self.target(self.n + 2);

        if pos == 0 {
            self.held = 0;
            if self.pending > 0 {
                self.pending -= 1;
                if self.pending > 0 {
                    return Vec::new();
                }
            }
            if self.vol >= t + p.b_hi {
                return Vec::new();
            }
            let (go, dir) = if self.vol < t + p.b_panic {
                (true, if model_on { pred } else { imb })
            } else {
                let strong = self.vol >= t + p.b_lo;
                if model_on {
                    let thr = if strong { self.thr } else { self.thr * 0.5 };
                    (pred.abs() >= thr, pred)
                } else {
                    let th = if strong { p.th } else { p.th_force };
                    (imb.abs() >= th, imb)
                }
            };
            if !go {
                return Vec::new();
            }
            let side = if dir >= 0.0 { Side::Buy } else { Side::Sell };
            self.pending = 2;
            return vec![Order::Market { side, qty: p.q.max(1) }];
        }

        // In a position: hold while the book does not lean hard against it.
        self.held = self.held.saturating_add(1);
        let long = pos > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.held >= p.hold || ts_ms.saturating_sub(self.entry_ts) >= p.hold as u64 * 100;
        // The exit leg counts as volume, so take it if the schedule falls behind.
        let behind = self.vol < t + p.b_panic;
        if timed_out || against >= p.flip || behind {
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

    /// Volume stays above the minimum both with no signal and with a constant one.
    #[test]
    fn pacing_keeps_volume() {
        for &(bq, aq) in &[(100_000i64, 100_000i64), (100_000, 1)] {
            let bids = [Level { price: 8_000_000, qty: bq }];
            let asks = [Level { price: 8_000_001, qty: aq }];
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
}
