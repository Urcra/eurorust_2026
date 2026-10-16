//! ML Predictor: a ridge model on book features predicts the 15-minute return and sets a position
//! of up to 5 BTC. The model is off by default, leaving small taker round trips for the volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

pub const NF: usize = 8;
/// Model and trading parameters. Feature order: nret300, nret900, nret1800, imb, imb300, ofi300,
/// dhi14400, nret14400.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct Params {
    /// Ridge coefficients, per-feature clamp bounds and intercept.
    pub a: [f64; NF],
    pub lo: [f64; NF],
    pub hi: [f64; NF],
    pub c: f64,
    /// Prediction scale.
    pub k: f64,
    /// Prediction thresholds (bps) to enter and leave a position.
    pub th_in: f64,
    pub th_out: f64,
    pub qmax: i64,
    /// 0 = market orders, 1 = resting limit orders.
    pub mode: i32,
    pub off: i64,
    pub rehang: i64,
    pub clip: i64,
    /// Cap the position at this fraction of the visible depth on the exit side (0 = off).
    pub dcap: f64,
    /// Seconds of history before trading.
    pub warm: usize,
    /// With limit orders, use a market order when the prediction exceeds this (bps; 0 = never).
    pub th_cross: f64,
    /// Scale the target by |pred| between `th_in` and `th_full` (0 = off).
    pub th_full: f64,
    /// Number of hysteresis rules and the threshold step between them.
    pub nth: usize,
    pub dth: f64,
    /// Stay flat when the 1-hour realised volatility (bps/s RMS) exceeds this.
    pub rvmax: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            a: [-0.0715393, -0.130496, -0.0570823, 0.0701976, -0.274643, -0.0149244, -0.030106, 0.0404235],
            lo: [-2.85175, -2.55619, -2.28295, -0.994937, -0.564609, -6.05449, -3.23328, -2.3361],
            hi: [3.11925, 2.71325, 2.47118, 0.994605, 0.50757, 5.36703, -0.011145, 2.80382],
            c: -0.024373,
            k: 1.8,
            th_in: 1.2,
            th_out: 0.0,
            qmax: 500_000,
            mode: 0,
            off: 0,
            rehang: 0,
            clip: 1_000_000,
            dcap: 0.0,
            warm: 1800,
            th_cross: 0.0,
            th_full: 0.0,
            nth: 5,
            dth: 0.15,
            rvmax: 1e9,
        }
    }
}

/// Trade the model's target. Off because it did not hold up out of sample.
const ML_ENABLED: bool = false;

/// Per-second history length (power of two).
const RB: usize = 16384;
const RMASK: usize = RB - 1;
/// 10-second blocks covering 4 hours, for the running high.
const NBLK: usize = 1440;

// Volume trips: 0.01 BTC, entered on top-of-book imbalance, paced against the requirement.
const REQ_PER_DAY: i64 = 1_000_000;
const SNAPS_PER_DAY: i64 = 864_000;
const PQ: i64 = 1_000;
const TH: f64 = 0.99;
const TH_FORCE: f64 = 0.9;
const TH_PANIC: f64 = 0.5;
const FLIP: f64 = 0.9;
const HOLD: u32 = 1_200;
const B_HI: i64 = 15_000;
const B_LO: i64 = 2_500;
const B_PANIC: i64 = 1_500;
const B_EMERG: i64 = 800;
const B_EXIT: i64 = 600;
const F_MIN: f64 = 0.05;
const GAMMA: i32 = 4;
const RV_A: f64 = 1.0 / 600.0;
const RING: usize = 512;
const SAMP: u32 = 50;
const WARM: usize = 20;

pub struct MyStrategy {
    p: Params,
    cur_sec: u64,
    started: bool,
    /// Seconds completed.
    pub nsec: usize,
    lm: Vec<f64>,
    r2: Vec<f64>,
    imbs: Vec<f64>,
    ofis: Vec<f64>,
    deps: Vec<f64>,
    blk_max: Vec<f64>,
    s_r2: f64,
    s_imb: f64,
    s_ofi: f64,
    s_dep: f64,
    // Running sums for the current second.
    acc_imb: f64,
    acc_dep: f64,
    acc_ofi: f64,
    acc_n: f64,
    last_lm: f64,
    last_imb: f64,
    last_dep: f64,
    prev_b: (i64, i64, i64, i64),
    have_prev: bool,
    pub pred: f64,
    dir: i32,
    dirs: [i32; 8],
    ord_px: i64,
    ord_side: i32,
    n: i64,
    vol: i64,
    last_pos: i64,
    pacer_pos: i64,
    held: u32,
    entry_ts: u64,
    prev_mid: f64,
    rv: f64,
    ring: [f32; RING],
    ring_i: usize,
    ring_n: usize,
    samp_ctr: u32,
}

impl Default for MyStrategy {
    fn default() -> Self {
        Self::with(Params::default())
    }
}

fn clampf(x: f64, lo: f64, hi: f64) -> f64 {
    if x.is_nan() { 0.0 } else { x.max(lo).min(hi) }
}

impl MyStrategy {
    pub fn with(p: Params) -> Self {
        MyStrategy {
            p,
            cur_sec: 0,
            started: false,
            nsec: 0,
            lm: vec![0.0; RB],
            r2: vec![0.0; RB],
            imbs: vec![0.0; RB],
            ofis: vec![0.0; RB],
            deps: vec![0.0; RB],
            blk_max: vec![f64::NEG_INFINITY; NBLK],
            s_r2: 0.0,
            s_imb: 0.0,
            s_ofi: 0.0,
            s_dep: 0.0,
            acc_imb: 0.0,
            acc_dep: 0.0,
            acc_ofi: 0.0,
            acc_n: 0.0,
            last_lm: 0.0,
            last_imb: 0.0,
            last_dep: 0.0,
            prev_b: (0, 0, 0, 0),
            have_prev: false,
            pred: 0.0,
            dir: 0,
            dirs: [0; 8],
            ord_px: 0,
            ord_side: 0,
            n: 0,
            vol: 0,
            last_pos: 0,
            pacer_pos: 0,
            held: 0,
            entry_ts: 0,
            prev_mid: 0.0,
            rv: 0.0,
            ring: [0.0; RING],
            ring_i: 0,
            ring_n: 0,
            samp_ctr: 0,
        }
    }

    /// Appends one completed second.
    fn push_sec(&mut self, lm: f64, imb_avg: f64, dep_avg: f64, ofi: f64) {
        let i = self.nsec;
        let r = if i > 0 { lm - self.lm[(i - 1) & RMASK] } else { 0.0 };
        let r2 = if r.is_finite() { r * r } else { 0.0 };
        // Rolling sums: r2 over 3600 s, imb/ofi/dep over 300 s.
        if i >= 3600 {
            self.s_r2 -= self.r2[(i - 3600) & RMASK];
        }
        if i >= 300 {
            let j = (i - 300) & RMASK;
            self.s_imb -= self.imbs[j];
            self.s_ofi -= self.ofis[j];
            self.s_dep -= self.deps[j];
        }
        let k = i & RMASK;
        self.lm[k] = lm;
        self.r2[k] = r2;
        self.imbs[k] = imb_avg;
        self.ofis[k] = ofi;
        self.deps[k] = dep_avg;
        self.s_r2 += r2;
        self.s_imb += imb_avg;
        self.s_ofi += ofi;
        self.s_dep += dep_avg;
        let b = (i / 10) % NBLK;
        if i.is_multiple_of(10) || lm > self.blk_max[b] {
            self.blk_max[b] = lm;
        }
        self.nsec += 1;
        // Recompute the sums now and then to avoid float drift.
        if self.nsec.is_multiple_of(4096) {
            let n3 = self.nsec.min(3600);
            self.s_r2 = (0..n3).map(|d| self.r2[(self.nsec - 1 - d) & RMASK]).sum();
            let n1 = self.nsec.min(300);
            self.s_imb = (0..n1).map(|d| self.imbs[(self.nsec - 1 - d) & RMASK]).sum();
            self.s_ofi = (0..n1).map(|d| self.ofis[(self.nsec - 1 - d) & RMASK]).sum();
            self.s_dep = (0..n1).map(|d| self.deps[(self.nsec - 1 - d) & RMASK]).sum();
        }
    }

    pub fn features(&self) -> Option<[f64; NF]> {
        let n = self.nsec;
        if n < self.p.warm.max(310) {
            return None;
        }
        let now = self.lm[(n - 1) & RMASK];
        let nr = n.min(3600) as f64;
        let rv = (self.s_r2 / nr).sqrt().max(1e-3);
        let nret = |l: usize| -> f64 {
            let l = l.min(n - 1);
            (now - self.lm[(n - 1 - l) & RMASK]) / (rv * (l as f64).sqrt())
        };
        let n1 = n.min(300) as f64;
        let imb300 = self.s_imb / n1;
        let dep = (self.s_dep / n1).max(1.0);
        let ofi300 = self.s_ofi / (dep * (n1).sqrt());
        // Max over the last NBLK blocks, including the current one.
        let nb = ((n - 1) / 10 + 1).min(NBLK);
        let cb = (n - 1) / 10;
        let mut mx = f64::NEG_INFINITY;
        for d in 0..nb {
            let v = self.blk_max[(cb - d) % NBLK];
            if v > mx {
                mx = v;
            }
        }
        let dhi = (now - mx) / (rv * (14400f64).min(n as f64).sqrt());
        Some([nret(300), nret(900), nret(1800), self.last_imb, imb300, ofi300, dhi, nret(14400)])
    }

    // `!(rv <= max)` on purpose: it is also true for NaN.
    #[allow(clippy::needless_range_loop, clippy::neg_cmp_op_on_partial_ord)]
    fn on_second(&mut self) {
        let f = match self.features() {
            Some(f) => f,
            None => {
                self.pred = 0.0;
                return;
            }
        };
        let mut s = self.p.c;
        for i in 0..NF {
            s += self.p.a[i] * clampf(f[i], self.p.lo[i], self.p.hi[i]);
        }
        let s = s * self.p.k;
        self.pred = if s.is_finite() { s } else { 0.0 };
        let nr = self.nsec.min(3600) as f64;
        let rv = (self.s_r2 / nr).sqrt();
        if !(rv <= self.p.rvmax) {
            self.pred = 0.0;
        }
        let x = self.pred;
        let mut tot = 0i32;
        for j in 0..self.p.nth {
            let ti = self.p.th_in + j as f64 * self.p.dth;
            let to = self.p.th_out + j as f64 * self.p.dth;
            let d = match self.dirs[j] {
                0 => {
                    if x > ti {
                        1
                    } else if x < -ti {
                        -1
                    } else {
                        0
                    }
                }
                1 => {
                    if x < -ti {
                        -1
                    } else if x < to {
                        0
                    } else {
                        1
                    }
                }
                _ => {
                    if x > ti {
                        1
                    } else if x > -to {
                        0
                    } else {
                        -1
                    }
                }
            };
            self.dirs[j] = d;
            tot += d;
        }
        self.dir = tot;
    }

    fn update_book(&mut self, ts_ms: u64, book: &Book) {
        let b = book.best_bid();
        let a = book.best_ask();
        let mid = (b.price + a.price) as f64 * 0.5;
        let lm = mid.ln() * 1e4;
        let dep = (b.qty + a.qty) as f64;
        let imb = if dep > 0.0 { (b.qty - a.qty) as f64 / dep } else { 0.0 };
        let sec = ts_ms / 1000;
        if !self.started {
            self.started = true;
            self.cur_sec = sec;
        } else if sec > self.cur_sec {
            let ia = if self.acc_n > 0.0 { self.acc_imb / self.acc_n } else { 0.0 };
            let da = if self.acc_n > 0.0 { self.acc_dep / self.acc_n } else { self.last_dep };
            self.push_sec(self.last_lm, ia, da, self.acc_ofi);
            let gap = (sec - self.cur_sec - 1).min(4000);
            for _ in 0..gap {
                self.push_sec(self.last_lm, 0.0, self.last_dep, 0.0);
            }
            self.acc_imb = 0.0;
            self.acc_dep = 0.0;
            self.acc_ofi = 0.0;
            self.acc_n = 0.0;
            self.cur_sec = sec;
            self.on_second();
        }
        // Order-flow imbalance against the previous snapshot.
        if self.have_prev {
            let (pb, pbq, pa, paq) = self.prev_b;
            let mut e = 0i64;
            if b.price >= pb {
                e += b.qty;
            }
            if b.price <= pb {
                e -= pbq;
            }
            if a.price <= pa {
                e -= a.qty;
            }
            if a.price >= pa {
                e += paq;
            }
            self.acc_ofi += e as f64;
        }
        self.prev_b = (b.price, b.qty, a.price, a.qty);
        self.have_prev = true;
        self.acc_imb += imb;
        self.acc_dep += dep;
        self.acc_n += 1.0;
        self.last_lm = lm;
        self.last_imb = imb;
        self.last_dep = dep;
    }

    fn required(n: i64) -> i64 {
        (REQ_PER_DAY as i128 * n as i128 / SNAPS_PER_DAY as i128) as i64
    }

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

    /// Volume trips on their own small position: the signed order wanted, or 0.
    fn pacer(&mut self, ts_ms: u64, imb: f64, room_long: bool, room_short: bool) -> i64 {
        let s = self.vol - Self::required(self.n + 2);
        if self.pacer_pos == 0 {
            self.held = 0;
            if s >= B_HI {
                return 0;
            }
            let go = if s < B_EMERG {
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
            };
            if !go {
                return 0;
            }
            let mut d = if imb >= 0.0 { PQ } else { -PQ };
            if d > 0 && !room_long {
                d = -PQ;
            }
            if d < 0 && !room_short {
                d = PQ;
            }
            self.pacer_pos = d;
            self.entry_ts = ts_ms;
            return d;
        }
        self.held = self.held.saturating_add(1);
        let long = self.pacer_pos > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.held >= HOLD || ts_ms.saturating_sub(self.entry_ts) >= HOLD as u64 * 100;
        if timed_out || against >= FLIP || s < B_EXIT {
            let d = -self.pacer_pos;
            self.pacer_pos = 0;
            return d;
        }
        0
    }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Calm Pacer" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.n += 1;
        self.update_vol(book);
        self.update_book(ts_ms, book);
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        self.last_pos = pos;
        let b = book.best_bid();
        let a = book.best_ask();
        let dep = (b.qty + a.qty) as f64;
        let imb = if dep > 0.0 { (b.qty - a.qty) as f64 / dep } else { 0.0 };

        let dir = if ML_ENABLED { self.dir } else { 0 };
        let mut tgt = dir as i64 * self.p.qmax / self.p.nth as i64 / 1000 * 1000;
        if self.p.th_full > self.p.th_in && tgt != 0 {
            let f = ((self.pred.abs() - self.p.th_out) / (self.p.th_full - self.p.th_out)).clamp(0.0, 1.0);
            tgt = (tgt as f64 * f) as i64 / 10_000 * 10_000;
        }
        if self.p.dcap > 0.0 {
            let cap_l: i64 = (book.bids.iter().map(|l| l.qty).sum::<i64>() as f64 * self.p.dcap) as i64;
            let cap_s: i64 = (book.asks.iter().map(|l| l.qty).sum::<i64>() as f64 * self.p.dcap) as i64;
            tgt = tgt.min(cap_l.max(0)).max(-cap_s.max(0));
        }
        let lim = 1_000_000 - PQ - 1_000;
        tgt = tgt.clamp(-lim, lim);

        let mut orders = Vec::new();
        if !ML_ENABLED {
            // Without the model the pacer owns the whole position.
            self.pacer_pos = pos;
        }
        let pd = self.pacer(ts_ms, imb, tgt + PQ <= lim + PQ, tgt - PQ >= -lim - PQ);
        if pd > 0 {
            orders.push(Order::Market { side: Side::Buy, qty: pd });
        } else if pd < 0 {
            orders.push(Order::Market { side: Side::Sell, qty: -pd });
        }
        if !ML_ENABLED {
            return orders;
        }
        // Model part of the position: everything except the pacer's holding before this call.
        let ml_pos = pos - self.pacer_pos + pd;
        let diff = tgt - ml_pos;
        let mut q = diff.abs().min(self.p.clip);
        // Room left under the position limit, counting the pacer order.
        let pend_buy = pd.max(0);
        let pend_sell = (-pd).max(0);
        if diff > 0 {
            q = q.min(1_000_000 - pos - pend_buy);
        } else {
            q = q.min(1_000_000 + pos - pend_sell);
        }
        if q < 1_000 {
            self.ord_side = 0;
            return orders;
        }
        let side = if diff > 0 { 1 } else { -1 };
        let strong = self.p.th_cross > 0.0 && self.pred.abs() >= self.p.th_cross;
        if self.p.mode == 0 || strong {
            self.ord_side = 0;
            orders.push(Order::Market { side: if side > 0 { Side::Buy } else { Side::Sell }, qty: q });
            return orders;
        }
        // Resting order, re-priced only when the wanted price moves away by more than `rehang`.
        let want = if side > 0 { b.price - self.p.off } else { a.price + self.p.off };
        let px = if self.ord_side == side {
            let moved_away = if side > 0 { want - self.ord_px } else { self.ord_px - want };
            if moved_away > self.p.rehang { want } else { self.ord_px }
        } else {
            want
        };
        self.ord_side = side;
        self.ord_px = px;
        orders.push(Order::Limit { side: if side > 0 { Side::Buy } else { Side::Sell }, price: px, qty: q });
        orders
    }
}

#[cfg(test)]
mod own_tests {
    use super::*;
    use sim::{Level, STARTING_CASH};

    /// On a flat book with no signal, pacing alone keeps volume above the minimum.
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
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
    }
}
