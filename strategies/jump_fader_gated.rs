//! Jump Fader (gated): fade sudden 30 s moves with 1 BTC for 5 minutes, but only once paper-traded
//! fades have been profitable during the run. Small taker round trips cover the minimum volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

const REQ_PER_DAY: i64 = 1_000_000;
const SNAPS_PER_DAY: i64 = 864_000;

// Volume trips: 0.01 BTC, entered on top-of-book imbalance, paced against the requirement.
const Q: i64 = 1_000;
const TH: f64 = 0.99;
const TH_FORCE: f64 = 0.9;
const TH_PANIC: f64 = 0.5;
const FLIP: f64 = 0.9;
const HOLD: u32 = 1_200;
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

/// Log-mid history length (power of two).
const HL: usize = 1 << 16;
const HM: usize = HL - 1;
/// Window for the sigma estimate, in snapshots (5 minutes).
const SIGW: usize = 3000;
/// Number of paper-trade results kept.
const REC: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// 0 = overlay off, 1 = continuation, -1 = fade.
    pub dir: i32,
    /// Event window in snapshots.
    pub w: usize,
    /// Minimum move in sigmas.
    pub k: f64,
    /// Minimum move in bps.
    pub mb: f64,
    /// Hold time in snapshots.
    pub t: u32,
    /// Overlay size in lots.
    pub size: i64,
    /// Cooldown between events in snapshots.
    pub cool: i64,
    /// Skip an event when the move over the `l` snapshots before it, in its direction, exceeds
    /// this many bps.
    pub cm: f64,
    pub l: usize,
    /// Stop loss in bps from the entry mid.
    pub sl: f64,
    /// After a stop-out, block same-direction fades for this many snapshots.
    pub block: i64,
    /// Second detector (w2 = 0 disables).
    pub w2: usize,
    pub k2: f64,
    pub mb2: f64,
    pub t2: u32,
    /// On books with 10+ levels, cap the size at this fraction of the depth it will close into.
    pub dcap: f64,
    /// Trade for real only after `gmin` paper trades, and while the last `gn` have
    /// mean * sqrt(n) > gz * (sd + gpad). A negative `gz` disables the gate.
    pub gmin: usize,
    pub gn: usize,
    pub gz: f64,
    pub gpad: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            dir: -1,
            w: 300,
            k: 10.0,
            mb: 10.0,
            t: 3000,
            size: 100_000,
            cool: 100,
            cm: 1e9,
            l: 36_000,
            sl: 150.0,
            block: 72_000,
            w2: 0,
            k2: 6.0,
            mb2: 10.0,
            t2: 6000,
            dcap: 0.25,
            gmin: 6,
            gn: 40,
            gz: 1.0,
            gpad: 5.0,
        }
    }
}

pub struct MyStrategy {
    p: Params,
    n: i64,
    vol: i64,
    last_pos: i64,
    held: u32,
    entry_ts: u64,
    pending: u32,
    prev_mid: f64,
    rv: f64,
    ring: [f32; RING],
    ring_i: usize,
    ring_n: usize,
    samp_ctr: u32,
    // Event overlay state.
    hist: Vec<f64>,
    var_hist: Vec<f32>,
    r2: Vec<f64>,
    r2_sum: f64,
    last_ev: i64,
    /// Overlay target position in lots.
    target: i64,
    ov_active: bool,
    ov_side: i32,
    // Paper trade state and the record of its net results in bps.
    pp_active: bool,
    pp_start: i64,
    pp_side: i32,
    pp_entry_lm: f64,
    pp_entry_hs: f64,
    pp_t: i64,
    rec: [f64; REC],
    rec_n: usize,
    block_until: i64,
    block_side: i32,
}

impl Default for MyStrategy {
    fn default() -> Self {
        Self::with(Params::default())
    }
}

impl MyStrategy {
    pub fn with(p: Params) -> Self {
        MyStrategy {
            p,
            n: 0,
            vol: 0,
            last_pos: 0,
            held: 0,
            entry_ts: 0,
            pending: 0,
            prev_mid: 0.0,
            rv: 0.0,
            ring: [0.0; RING],
            ring_i: 0,
            ring_n: 0,
            samp_ctr: 0,
            hist: vec![0.0; HL],
            var_hist: vec![0.0; HL],
            r2: vec![0.0; SIGW],
            r2_sum: 0.0,
            last_ev: -1_000_000_000,
            target: 0,
            ov_active: false,
            ov_side: 0,
            pp_active: false,
            pp_start: 0,
            pp_side: 0,
            pp_entry_lm: 0.0,
            pp_entry_hs: 0.0,
            pp_t: 0,
            rec: [0.0; REC],
            rec_n: 0,
            block_until: 0,
            block_side: 0,
        }
    }

    /// Adds a paper-trade result in bps.
    fn record(&mut self, net: f64) {
        if net.is_finite() {
            self.rec[self.rec_n % REC] = net;
            self.rec_n += 1;
        }
    }

    /// True when recent paper fades are good enough to trade for real.
    fn gate_open(&self) -> bool {
        if self.p.gz < 0.0 {
            return true;
        }
        if self.rec_n < self.p.gmin.max(1) {
            return false;
        }
        let n = self.rec_n.min(self.p.gn.clamp(1, REC));
        let mut sum = 0.0;
        let mut sq = 0.0;
        for j in 0..n {
            let x = self.rec[(self.rec_n - 1 - j) % REC];
            sum += x;
            sq += x * x;
        }
        let mean = sum / n as f64;
        let sd = (sq / n as f64 - mean * mean).max(0.0).sqrt();
        mean > 0.0 && mean * (n as f64).sqrt() > self.p.gz * (sd + self.p.gpad)
    }

    fn required(n: i64) -> i64 {
        (REQ_PER_DAY as i128 * n as i128 / SNAPS_PER_DAY as i128) as i64
    }

    /// Fraction of the volatility history below `x`.
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

    /// Records the log mid and returns the event direction (+1, -1 or 0) and which detector fired.
    fn update_events(&mut self, book: &Book) -> (i32, u32) {
        let mid = (book.bids[0].price + book.asks[0].price) as f64 * 0.5;
        let lm = if mid > 0.0 { mid.ln() * 1e4 } else { 0.0 };
        let i = self.n as usize; // 1-based count
        let prev = if i > 1 { self.hist[(i - 1) & HM] } else { lm };
        self.hist[i & HM] = lm;
        let r = lm - prev;
        let slot = i % SIGW;
        self.r2_sum += r * r - self.r2[slot];
        self.r2[slot] = r * r;
        if i.is_multiple_of(100_000) {
            self.r2_sum = self.r2.iter().sum();
        }
        let var = (self.r2_sum.max(0.0) / SIGW as f64) as f32;
        self.var_hist[i & HM] = var;
        if self.p.dir == 0 || self.n - self.last_ev < self.p.cool {
            return (0, 0);
        }
        for which in 1..=2u32 {
            let (w, k, mb) =
                if which == 1 { (self.p.w, self.p.k, self.p.mb) } else { (self.p.w2, self.p.k2, self.p.mb2) };
            if w == 0 {
                continue;
            }
            let w = w.clamp(1, 30_000);
            if i <= SIGW + w + 1 {
                continue;
            }
            let mv = lm - self.hist[(i - w) & HM];
            let sig = (self.var_hist[(i - w) & HM] as f64 * w as f64).sqrt();
            if mv.abs() > k * sig && mv.abs() > mb {
                self.last_ev = self.n;
                let d = if mv > 0.0 { 1 } else { -1 };
                // Move over the `l` snapshots before the window, in the event direction.
                let l = self.p.l.min(HL - w - 2);
                let j0 = if i > w + l { i - w - l } else { 1 };
                let ctx = d as f64 * (self.hist[(i - w) & HM] - self.hist[j0 & HM]);
                if ctx > self.p.cm {
                    return (0, 0);
                }
                return (d, which);
            }
        }
        (0, 0)
    }
}

/// Half the quoted spread in bps of the mid.
fn half_spread_bps(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].price as f64, book.asks[0].price as f64);
    if b + a <= 0.0 { 0.0 } else { (a - b) / (a + b) * 1e4 }
}

/// Top-of-book imbalance in [-1, 1], positive when the bid is larger.
fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Jump Fader (gated)" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.n += 1;
        self.update_vol(book);
        let (ev, which) = self.update_events(book);
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        let pacer_entry = self.last_pos == 0 && pos != 0 && !self.ov_active;
        if pacer_entry {
            self.held = 0;
            self.entry_ts = ts_ms;
            self.pending = 0;
        }
        self.last_pos = pos;
        let imb = imbalance(book);
        let s = self.vol - Self::required(self.n + 2);

        // Every event is paper-traded. The real fade follows the paper trade when the gate is open.
        let lm = self.hist[(self.n as usize) & HM];
        let hs = half_spread_bps(book);
        if self.pp_active {
            let age = self.n - self.pp_start;
            let adverse = -(self.pp_side as f64) * (lm - self.pp_entry_lm);
            if age >= self.pp_t || adverse > self.p.sl {
                let stopped = adverse > self.p.sl;
                if stopped && self.p.block > 0 {
                    self.block_until = self.n + self.p.block;
                    self.block_side = self.pp_side;
                }
                // Charge 1 bps per side plus half the spread at entry and exit.
                let net = self.pp_side as f64 * (lm - self.pp_entry_lm) - 2.0 - self.pp_entry_hs - hs;
                self.record(net);
                self.pp_active = false;
                if self.ov_active {
                    self.ov_active = false;
                    self.target = 0;
                }
            }
        } else if ev != 0 {
            let side = ev * self.p.dir;
            if !(self.n < self.block_until && side == self.block_side) {
                self.pp_active = true;
                self.pp_start = self.n;
                self.pp_side = side;
                self.pp_entry_lm = lm;
                self.pp_entry_hs = hs;
                self.pp_t = if which == 2 { self.p.t2 as i64 } else { self.p.t as i64 };
                if self.gate_open() {
                    self.ov_active = true;
                    self.ov_side = side;
                    let mut q = self.p.size.min(900_000);
                    let exit_side = if side > 0 { book.bids } else { book.asks };
                    if exit_side.len() >= 10 {
                        let depth: i64 = exit_side.iter().map(|l| l.qty).sum();
                        q = q.min((depth as f64 * self.p.dcap) as i64);
                    }
                    q = q / 1_000 * 1_000;
                    self.target = side as i64 * q;
                    // Skip fades too small to cover the volume missed while the trips are paused.
                    if q < 10_000 {
                        self.ov_active = false;
                        self.ov_side = 0;
                        self.target = 0;
                    }
                }
            }
        }
        if self.ov_active || self.ov_side != 0 {
            // On thin books the unfilled rest is re-sent on the next call.
            let diff = self.target - pos;
            if diff != 0 {
                let side = if diff > 0 { Side::Buy } else { Side::Sell };
                return vec![Order::Market { side, qty: diff.abs() }];
            }
            if self.ov_active {
                return Vec::new();
            }
            // Closed out, hand back to the volume trips.
            self.ov_side = 0;
            self.pending = 0;
            self.held = 0;
        }

        // Volume trips while the overlay is flat.
        if pos == 0 {
            self.held = 0;
            if self.pending > 0 {
                self.pending -= 1;
                if self.pending > 0 {
                    return Vec::new();
                }
            }
            if s >= B_HI {
                return Vec::new();
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
                return Vec::new();
            }
            let side = if imb >= 0.0 { Side::Buy } else { Side::Sell };
            self.pending = 2;
            return vec![Order::Market { side, qty: Q }];
        }

        self.held = self.held.saturating_add(1);
        let long = pos > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.held >= HOLD || ts_ms.saturating_sub(self.entry_ts) >= HOLD as u64 * 100;
        if timed_out || against >= FLIP || s < B_EXIT {
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

    /// A flat book never triggers the overlay, and pacing keeps volume above the requirement.
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
            assert!(pos.abs() <= Q);
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
    }

    /// With the gate off, a sharp drop is bought and closed after the holding time.
    #[test]
    fn fades_a_drop() {
        let mut s = MyStrategy::with(Params { gz: -1.0, ..Params::default() });
        let (mut pos, mut maxpos) = (0i64, 0i64);
        let mut pending: Vec<Order> = Vec::new();
        for n in 1..=12_000i64 {
            for o in pending.drain(..) {
                if let Order::Market { side, qty } = o {
                    pos += if side == Side::Buy { qty } else { -qty };
                }
            }
            maxpos = maxpos.max(pos);
            assert!(pos.abs() <= 1_000_000);
            let base = 8_000_000 + (n % 2);
            let p = if n < 5_000 {
                base
            } else if n < 5_100 {
                base - (n - 5_000) * 300
            } else {
                base - 30_000
            };
            let bids = [Level { price: p, qty: 500_000 }];
            let asks = [Level { price: p + 1, qty: 500_000 }];
            let book = Book { bids: &bids, asks: &asks };
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
        assert!(maxpos >= 100_000, "no fade entry");
        assert!(pos.abs() <= Q, "not closed: {pos}");
    }

    /// Repeated drops that revert are paper-traded first, then faded for real.
    #[test]
    fn gate_opens_after_good_paper_record() {
        let mut s = MyStrategy::default();
        let (mut pos, mut early_max, mut late_max) = (0i64, 0i64, 0i64);
        let mut pending: Vec<Order> = Vec::new();
        let cycle = 5_000i64;
        for n in 1..=(14 * cycle + 6_000) {
            for o in pending.drain(..) {
                if let Order::Market { side, qty } = o {
                    pos += if side == Side::Buy { qty } else { -qty };
                }
            }
            assert!(pos.abs() <= 1_000_000);
            if n < 6 * cycle {
                early_max = early_max.max(pos.abs());
            } else {
                late_max = late_max.max(pos.abs());
            }
            let c = n % cycle;
            let base = 8_000_000 + (n % 2);
            // Quiet, then a 100 bps drop over 50 snapshots, then back up over 500 snapshots.
            let p = if c < 4_000 {
                base
            } else if c < 4_050 {
                base - (c - 4_000) * 1_600
            } else if c < 4_550 {
                base - 80_000 + (c - 4_050) * 160
            } else {
                base
            };
            let bids = [Level { price: p, qty: 500_000 }];
            let asks = [Level { price: p + 1, qty: 500_000 }];
            let book = Book { bids: &bids, asks: &asks };
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
        assert!(early_max <= Q, "traded before the record was good: {early_max}");
        assert!(late_max >= 100_000, "gate never opened");
        assert!(s.rec_n >= 7);
    }
}
