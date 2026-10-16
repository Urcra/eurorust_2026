//! Jump Fader + Hold: fade sharp 30-second moves with a 1 BTC market order held for 5 minutes, on
//! top of a static 2 BTC long. Small taker round trips cover the minimum volume.

/// Static long in lots (2 BTC).
pub const HOLD_SIZE: i64 = 200_000;

const NAME: &str = "Jump Fader + Hold";
const LIMIT: i64 = 1_000_000;
/// Max lots per hold market order (1 BTC).
const CLIP: i64 = 100_000;
/// Hold orders take at most the liquidity within this many 0.1 bps of the touch,
const CLIP_BAND: i64 = 30;
/// but at least this much (0.01 BTC).
const CLIP_MIN: i64 = 1_000;

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

/// Log-mid history length (power of two).
const HL: usize = 1 << 16;
const HM: usize = HL - 1;
/// Window for the volatility estimate, in snapshots.
const SIGW: usize = 3000;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// 0 = off, 1 = follow the move, -1 = fade it.
    pub dir: i32,
    /// An event is a move over `w` snapshots larger than `k` sigma and `mb` bps.
    pub w: usize,
    pub k: f64,
    pub mb: f64,
    /// Hold time in snapshots.
    pub t: u32,
    /// Fade size in lots.
    pub size: i64,
    /// Snapshots between events.
    pub cool: i64,
    /// Skip when the prior `l`-snapshot move in the event direction exceeds this (bps).
    pub cm: f64,
    pub l: usize,
    /// Stop loss in bps from the entry mid.
    pub sl: f64,
    /// After a stop-out, block same-direction fades for this many snapshots.
    pub block: i64,
    /// Second detector, off when `w2` is 0.
    pub w2: usize,
    pub k2: f64,
    pub mb2: f64,
    pub t2: u32,
    /// On books with 10+ levels, cap the size at this fraction of the depth it will close into.
    pub dcap: f64,
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
        }
    }
}

/// Which component sent the orders of the previous call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Who {
    None,
    Pacer,
    Fade,
    Hold,
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
    hist: Vec<f64>,
    var_hist: Vec<f32>,
    r2: Vec<f64>,
    r2_sum: f64,
    last_ev: i64,
    /// Fade target in lots.
    target: i64,
    ov_active: bool,
    ov_start: i64,
    ov_entry_lm: f64,
    ov_side: i32,
    ov_t: i64,
    block_until: i64,
    block_side: i32,
    /// Sub-positions in lots: pacer, fade, hold.
    epos: i64,
    fpos: i64,
    hpos: i64,
    who: Who,
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
            ov_start: 0,
            ov_entry_lm: 0.0,
            ov_side: 0,
            ov_t: 0,
            block_until: 0,
            block_side: 0,
            epos: 0,
            fpos: 0,
            hpos: 0,
            who: Who::None,
        }
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

    /// Records the mid; returns the event direction (+1/-1, or 0) and which detector fired.
    fn update_events(&mut self, book: &Book) -> (i32, u32) {
        let mid = (book.bids[0].price + book.asks[0].price) as f64 * 0.5;
        let lm = if mid > 0.0 { mid.ln() * 1e4 } else { 0.0 };
        let i = self.n as usize;
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

fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

/// Lots a market order of `side` can take within `band` 0.1 bps of the touch.
fn near_liquidity(book: &Book, side: Side, band: i64) -> i64 {
    let levels = match side {
        Side::Buy => book.asks,
        Side::Sell => book.bids,
    };
    let best = levels[0].price;
    let tol = (best as i128 * band as i128 / 100_000) as i64;
    let mut q = 0i64;
    for l in levels {
        if (l.price - best).abs() > tol {
            break;
        }
        q = q.saturating_add(l.qty);
    }
    q
}

/// A market order for `want` signed lots, clipped to keep the position within the limit.
fn market(pos: i64, want: i64) -> Option<Order> {
    if want > 0 {
        let q = want.min(LIMIT - pos);
        (q > 0).then_some(Order::Market { side: Side::Buy, qty: q })
    } else if want < 0 {
        let q = (-want).min(LIMIT + pos);
        (q > 0).then_some(Order::Market { side: Side::Sell, qty: q })
    } else {
        None
    }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: NAME }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.n += 1;
        self.update_vol(book);
        let (ev, which) = self.update_events(book);
        let pos = account.position;

        // Each call sends orders for one component only, so the position change belongs to it.
        let dpos = pos - self.last_pos;
        self.vol += dpos.abs();
        match self.who {
            Who::Pacer => {
                let was = self.epos;
                self.epos += dpos;
                if was == 0 && self.epos != 0 && !self.ov_active {
                    self.held = 0;
                    self.entry_ts = ts_ms;
                    self.pending = 0;
                }
            }
            Who::Fade => self.fpos += dpos,
            _ => self.hpos += dpos,
        }
        if self.epos + self.fpos + self.hpos != pos {
            self.hpos = pos - self.epos - self.fpos;
        }
        self.last_pos = pos;
        self.who = Who::None;
        let imb = imbalance(book);
        let s = self.vol - Self::required(self.n + 2);

        let lm = self.hist[(self.n as usize) & HM];
        if self.ov_active {
            let age = self.n - self.ov_start;
            let adverse = -(self.ov_side as f64) * (lm - self.ov_entry_lm);
            if age >= self.ov_t || adverse > self.p.sl {
                if adverse > self.p.sl && self.p.block > 0 {
                    self.block_until = self.n + self.p.block;
                    self.block_side = self.ov_side;
                }
                self.ov_active = false;
                self.target = 0;
            }
        } else if ev != 0 {
            let side = ev * self.p.dir;
            if !(self.n < self.block_until && side == self.block_side) {
                self.ov_active = true;
                self.ov_start = self.n;
                self.ov_side = side;
                self.ov_entry_lm = lm;
                self.ov_t = if which == 2 { self.p.t2 as i64 } else { self.p.t as i64 };
                let mut q = self.p.size.min(900_000);
                let exit_side = if side > 0 { book.bids } else { book.asks };
                if exit_side.len() >= 10 {
                    let depth: i64 = exit_side.iter().map(|l| l.qty).sum();
                    q = q.min((depth as f64 * self.p.dcap) as i64);
                }
                q = q / 1_000 * 1_000;
                self.target = side as i64 * q;
                if q < 10_000 {
                    self.ov_active = false;
                    self.ov_side = 0;
                } else if self.epos != 0 {
                    // The pacer pauses during the fade, which takes over its open trip.
                    self.fpos += self.epos;
                    self.epos = 0;
                }
            }
        }
        let mut fade_busy = false;
        if self.ov_active || self.ov_side != 0 {
            let diff = self.target - self.fpos;
            if diff != 0
                && let Some(o) = market(pos, diff)
            {
                self.who = Who::Fade;
                return vec![o];
            }
            if self.ov_active {
                fade_busy = true;
            } else {
                // Fade closed: hand back to the pacer.
                self.ov_side = 0;
                self.pending = 0;
                self.held = 0;
            }
        }

        if !fade_busy {
            let e = self.pacer(ts_ms, imb, s);
            if e != 0
                && let Some(o) = market(pos, e)
            {
                self.who = Who::Pacer;
                return vec![o];
            }
        }

        let diff = HOLD_SIZE.clamp(0, LIMIT - 200_000) - self.hpos;
        if diff != 0 {
            let side = if diff > 0 { Side::Buy } else { Side::Sell };
            let avail = near_liquidity(book, side, CLIP_BAND).max(CLIP_MIN);
            let q = diff.abs().min(CLIP).min(avail);
            if let Some(o) = market(pos, if diff > 0 { q } else { -q }) {
                self.who = Who::Hold;
                return vec![o];
            }
        }
        Vec::new()
    }
}

impl MyStrategy {
    /// Volume trips on their own sub-position: the signed order wanted, or 0.
    fn pacer(&mut self, ts_ms: u64, imb: f64, s: i64) -> i64 {
        let pos = self.epos;
        if pos == 0 {
            self.held = 0;
            if self.pending > 0 {
                self.pending -= 1;
                if self.pending > 0 {
                    return 0;
                }
            }
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
            self.pending = 2;
            return if imb >= 0.0 { Q } else { -Q };
        }
        self.held = self.held.saturating_add(1);
        let long = pos > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.held >= HOLD || ts_ms.saturating_sub(self.entry_ts) >= HOLD as u64 * 100;
        if timed_out || against >= FLIP || s < B_EXIT {
            return -pos;
        }
        0
    }
}

#[cfg(test)]
mod own_tests {
    use super::*;
    use sim::{Level, STARTING_CASH};

    /// On a flat, thin book volume keeps up, the hold is reached and the sub-positions add up.
    #[test]
    fn pacing_and_hold() {
        let bids = [Level { price: 8_000_000, qty: 30_000 }];
        let asks = [Level { price: 8_000_001, qty: 30_000 }];
        let book = Book { bids: &bids, asks: &asks };
        let mut s = MyStrategy::default();
        let (mut pos, mut vol) = (0i64, 0i64);
        let mut pending: Vec<Order> = Vec::new();
        for n in 1..=300_000i64 {
            for o in pending.drain(..) {
                if let Order::Market { side, qty } = o {
                    let f = qty.min(30_000);
                    pos += if side == Side::Buy { f } else { -f };
                    vol += f;
                }
            }
            if n >= 2 {
                assert!(vol >= REQ_PER_DAY * n / SNAPS_PER_DAY, "behind at snapshot {n}");
            }
            assert!(pos.abs() <= LIMIT);
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
            assert_eq!(s.epos + s.fpos + s.hpos, pos);
            assert!(pending.len() <= 1);
        }
        assert_eq!(s.hpos, HOLD_SIZE);
        assert!(s.epos.abs() <= Q);
    }

    /// A sharp drop is faded with a 1 BTC buy that is closed again later, while the hold stays.
    #[test]
    fn fades_a_drop() {
        let mut s = MyStrategy::default();
        let (mut pos, mut maxf) = (0i64, 0i64);
        let mut pending: Vec<Order> = Vec::new();
        for n in 1..=12_000i64 {
            for o in pending.drain(..) {
                if let Order::Market { side, qty } = o {
                    pos += if side == Side::Buy { qty } else { -qty };
                }
            }
            assert!(pos.abs() <= LIMIT);
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
            maxf = maxf.max(s.fpos);
        }
        assert!(maxf >= 100_000, "no fade entry");
        assert_eq!(s.fpos, 0, "fade not closed");
        assert_eq!(s.hpos, HOLD_SIZE);
        assert!(s.epos.abs() <= Q);
    }
}
