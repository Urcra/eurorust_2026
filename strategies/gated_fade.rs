//! Gated Fade: fade moves away from a 15-minute EMA with a 2 BTC position, but only while the
//! 2-hour variance ratio says the market is mean-reverting. Small taker trips cover the volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Required volume per day of data, in lots (10 BTC).
const REQ_PER_DAY: i64 = 1_000_000;
/// Snapshots per day of data.
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

/// Fade position size in lots (2 BTC).
const FADE_SIZE: i64 = 200_000;
/// Max lots per market order (1 BTC).
const CLIP: i64 = 100_000;
/// EMA length in seconds (15 minutes), used to scale the z-score.
const L_EMA: f64 = 900.0;
const A_EMA: f64 = 1.0 / 900.0;
/// Smoothing for the per-second variance (30 minutes).
const A_SD: f64 = 1.0 / 1800.0;
/// Smoothing for the variance ratio inputs (2 hours).
const A_VR: f64 = 1.0 / 7200.0;
/// Enter when the z-score is beyond this.
const Z_IN: f64 = 1.5;
/// Only enter while the 30m/1m variance ratio is below this.
const VR_MAX: f64 = 0.7;
/// Max fade hold in seconds (2 hours).
const HOLD_S: u64 = 7_200;
/// Stop out after an adverse move of this many z units.
const STOP_K: f64 = 3.0;
/// Warm-up before the first fade, in seconds (4 hours).
const WARM_S: u64 = 14_400;
/// Fade decisions every this many seconds.
const DECIDE_EVERY: u64 = 5;
/// Seconds of log-mid history kept (30 minutes + 1).
const HIST: usize = 1_801;

pub struct MyStrategy {
    n: i64,
    vol: i64,
    last_pos: i64,
    /// Volume trip position (0 or +-Q).
    pc: i64,
    held: u32,
    entry_ts: u64,
    prev_mid: f64,
    rv: f64,
    ring: [f32; RING],
    ring_i: usize,
    ring_n: usize,
    samp_ctr: u32,
    last_sec: u64,
    secs: u64,
    hist: Vec<f64>,
    ema: f64,
    v_sd: f64,
    v_1: f64,
    v_30: f64,
    /// Fade target in lots.
    f: i64,
    f_entry_m: f64,
    f_entry_s: u64,
    f_lim: f64,
}

impl Default for MyStrategy {
    fn default() -> Self {
        MyStrategy {
            n: 0,
            vol: 0,
            last_pos: 0,
            pc: 0,
            held: 0,
            entry_ts: 0,
            prev_mid: 0.0,
            rv: 0.0,
            ring: [0.0; RING],
            ring_i: 0,
            ring_n: 0,
            samp_ctr: 0,
            last_sec: u64::MAX,
            secs: 0,
            hist: vec![0.0; HIST],
            ema: 0.0,
            v_sd: 0.0,
            v_1: 0.0,
            v_30: 0.0,
            f: 0,
            f_entry_m: 0.0,
            f_entry_s: 0,
            f_lim: 0.0,
        }
    }
}

impl MyStrategy {
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

    /// Log mid (bps) `k` seconds ago (k < HIST), or the oldest sample if the history is shorter.
    fn ago(&self, k: u64) -> f64 {
        if self.secs == 0 {
            return 0.0;
        }
        let k = k.min(self.secs - 1).min(HIST as u64 - 1);
        let i = ((self.secs - 1 - k) % HIST as u64) as usize;
        self.hist[i]
    }

    /// Feeds one per-second log mid into the fade model and updates the fade target.
    fn fade_second(&mut self, m: f64) {
        let i = (self.secs % HIST as u64) as usize;
        self.hist[i] = m;
        self.secs += 1;
        let s = self.secs;
        if s == 1 {
            self.ema = m;
        }
        self.ema += A_EMA * (m - self.ema);
        let r1 = if s > 60 { m - self.ago(60) } else { 0.0 };
        let r30 = if s > 1800 { m - self.ago(1800) } else { 0.0 };
        self.v_sd += A_SD * (r1 * r1 / 60.0 - self.v_sd);
        self.v_1 += A_VR * (r1 * r1 - self.v_1);
        self.v_30 += A_VR * (r30 * r30 - self.v_30);
        if s < WARM_S || !s.is_multiple_of(DECIDE_EVERY) {
            return;
        }
        let sd = self.v_sd.max(1e-4).sqrt();
        let scale = sd * L_EMA.sqrt();
        let z = (m - self.ema) / scale;
        if !z.is_finite() {
            return;
        }
        if self.f == 0 {
            let vr = self.v_30 / (30.0 * self.v_1).max(1e-9);
            if vr < VR_MAX && z.abs() > Z_IN {
                self.f = if z > 0.0 { -FADE_SIZE } else { FADE_SIZE };
                self.f_entry_m = m;
                self.f_entry_s = s;
                self.f_lim = STOP_K * scale;
            }
        } else {
            let long = self.f > 0;
            let stretch = if long { -z } else { z };
            let adverse = if long { self.f_entry_m - m } else { m - self.f_entry_m };
            // Exit on the stop, when price crosses the EMA, or after the max hold.
            if adverse > self.f_lim || stretch < 0.0 || s - self.f_entry_s >= HOLD_S {
                self.f = 0;
            }
        }
    }

    /// Opens or closes a volume trip on the virtual position `pc`.
    fn pacer(&mut self, ts_ms: u64, imb: f64) {
        let s = self.vol - Self::required(self.n + 2);
        if self.pc == 0 {
            self.held = 0;
            if s >= B_HI {
                return;
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
            if go {
                self.pc = if imb >= 0.0 { Q } else { -Q };
                self.entry_ts = ts_ms;
            }
            return;
        }
        self.held = self.held.saturating_add(1);
        let against = if self.pc > 0 { -imb } else { imb };
        let timed_out = self.held >= HOLD || ts_ms.saturating_sub(self.entry_ts) >= HOLD as u64 * 100;
        if timed_out || against >= FLIP || s < B_EXIT {
            self.pc = 0;
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
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Gated Fade" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.n += 1;
        self.update_vol(book);
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        self.last_pos = pos;

        let sec = ts_ms / 1000;
        if sec != self.last_sec {
            self.last_sec = sec;
            let mid = (book.bids[0].price + book.asks[0].price) as f64 * 0.5;
            if mid > 0.0 {
                let m = mid.ln() * 1e4;
                if m.is_finite() {
                    self.fade_second(m);
                }
            }
        }
        let imb = imbalance(book);
        self.pacer(ts_ms, imb);

        let target = (self.f + self.pc).clamp(-990_000, 990_000);
        let diff = target - pos;
        if diff == 0 {
            return Vec::new();
        }
        let qty = diff.abs().min(CLIP);
        let side = if diff > 0 { Side::Buy } else { Side::Sell };
        vec![Order::Market { side, qty }]
    }
}
