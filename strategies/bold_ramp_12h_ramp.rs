//! Bold Ramp, earlier version: hold a 10 BTC short, ramped in over the first 12 hours, and go flat
//! for good once the run is up 5% of the position. Small taker round trips cover the minimum volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// +1 long, -1 short.
const SIDE: i64 = -1;
/// Full position in lots (10 BTC).
const HOLD: i64 = 1_000_000;
/// Snapshots to ramp up to the full position (12 hours).
const RAMP: i64 = 432_000;
/// Ramp step in lots (0.5 BTC).
const STEP: i64 = 50_000;
/// Max lots per order when building or closing the position (1 BTC).
const CLIP: i64 = 100_000;
/// Only take liquidity within this many bps of the touch.
const SLIP_BPS: i64 = 2;
/// Take profit at this gain, in bps of the full position (500 bps of 10 BTC at $80k = $40k).
const LOCK_BPS: i64 = 500;
/// Flat from 23:55 UTC to midnight, so a run ending at midnight has nothing to liquidate.
const DIP_FROM_MS: u64 = 86_100_000;

const LIMIT: i64 = 1_000_000;
const REQ_PER_DAY: i64 = 1_000_000;
const SNAPS_PER_DAY: i64 = 864_000;
const START_CASH: i64 = 10_000 * 10_000_000;

// Volume trips: 0.01 BTC, entered on top-of-book imbalance, paced against the requirement.
const Q: i64 = 1_000;
const TH: f64 = 0.99;
const TH_FORCE: f64 = 0.9;
const TH_PANIC: f64 = 0.5;
const FLIP: f64 = 0.9;
const HOLD_TRIP: u32 = 1_200;
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

pub struct MyStrategy {
    n: i64,
    vol: i64,
    last_pos: i64,
    target: i64,
    locked: bool,
    trip_open: bool,
    held: u32,
    entry_ts: u64,
    pending: u32,
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
            n: 0,
            vol: 0,
            last_pos: 0,
            target: 0,
            locked: false,
            trip_open: false,
            held: 0,
            entry_ts: 0,
            pending: 0,
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

    fn ramp_target(&self, ts_ms: u64) -> i64 {
        if self.locked {
            return 0;
        }
        if DIP_FROM_MS > 0 && ts_ms % 86_400_000 >= DIP_FROM_MS {
            return 0;
        }
        let size = if RAMP > 0 && self.n < RAMP {
            let raw = (HOLD as i128 * self.n as i128 / RAMP as i128) as i64;
            raw / STEP * STEP
        } else {
            HOLD
        };
        SIDE * size.clamp(0, LIMIT)
    }

    /// One market order towards the target, capped by CLIP and the liquidity near the touch.
    fn adjust(&self, pos: i64, book: &Book) -> Vec<Order> {
        let want = self.target - pos;
        if want == 0 {
            return Vec::new();
        }
        let (side, levels) = if want > 0 { (Side::Buy, book.asks) } else { (Side::Sell, book.bids) };
        let Some(top) = levels.first() else { return Vec::new() };
        let lim = top.price as i128 * SLIP_BPS as i128 / 10_000;
        let mut avail: i64 = 0;
        for l in levels {
            if (l.price as i128 - top.price as i128).abs() > lim {
                break;
            }
            avail = avail.saturating_add(l.qty);
        }
        let qty = want.abs().min(CLIP).min(avail.max(Q));
        if qty <= 0 {
            return Vec::new();
        }
        vec![Order::Market { side, qty }]
    }
}

fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Bold Ramp" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        if book.bids.is_empty() || book.asks.is_empty() {
            return Vec::new();
        }
        self.n += 1;
        self.update_vol(book);
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        self.last_pos = pos;

        if LOCK_BPS > 0 && !self.locked {
            let mid2 = (book.bids[0].price + book.asks[0].price) as i128;
            let eq = account.cash + pos as i128 * mid2 / 2 - START_CASH as i128;
            let goal = HOLD as i128 * mid2 / 2 * LOCK_BPS as i128 / 10_000;
            if eq >= goal {
                self.locked = true;
            }
        }
        let t = self.ramp_target(ts_ms);
        if t != self.target {
            // Moving the position also closes any open trip.
            self.target = t;
            self.trip_open = false;
            self.pending = 0;
        }

        let imb = imbalance(book);
        let s = self.vol - Self::required(self.n + 2);
        let p = pos - self.target;

        if p == 0 {
            self.held = 0;
            if self.pending > 0 {
                self.pending -= 1;
                if self.pending > 0 {
                    return Vec::new();
                }
            }
            self.trip_open = false;
            if s >= B_HI {
                return Vec::new();
            }
            let urgent = s < B_LO;
            let go = if s < B_EMERG {
                true
            } else if s < B_PANIC {
                imb.abs() >= TH_PANIC
            } else if urgent {
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
            let mut side = if imb >= 0.0 { Side::Buy } else { Side::Sell };
            let fits = |sd: Side| (pos + if sd == Side::Buy { Q } else { -Q }).abs() <= LIMIT;
            if !fits(side) {
                // At the full position only a reducing trip fits.
                if !urgent {
                    return Vec::new();
                }
                side = if side == Side::Buy { Side::Sell } else { Side::Buy };
                if !fits(side) {
                    return Vec::new();
                }
            }
            self.pending = 2;
            self.trip_open = true;
            self.held = 0;
            self.entry_ts = ts_ms;
            return vec![Order::Market { side, qty: Q }];
        }

        if !self.trip_open {
            return self.adjust(pos, book);
        }

        // A trip is open on top of the target: close it when the book turns or time runs out.
        self.pending = 0;
        self.held = self.held.saturating_add(1);
        let long = p > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.held >= HOLD_TRIP || ts_ms.saturating_sub(self.entry_ts) >= HOLD_TRIP as u64 * 100;
        if timed_out || against >= FLIP || s < B_EXIT {
            let side = if long { Side::Sell } else { Side::Buy };
            return vec![Order::Market { side, qty: p.abs() }];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod own_tests {
    use super::*;
    use sim::{Level, STARTING_CASH};

    /// On a flat book that always favours buying, the position stays within the limit, volume
    /// keeps up with the requirement and the target is reached.
    #[test]
    fn limit_and_pacing() {
        let bids = [Level { price: 8_000_000, qty: 100 }];
        let asks = [Level { price: 8_000_001, qty: 10_000_000 }];
        let book = Book { bids: &bids, asks: &asks };
        let mut s = MyStrategy::default();
        let (mut pos, mut vol) = (0i64, 0i64);
        let mut pending: Vec<Order> = Vec::new();
        for n in 1..=1_500_000i64 {
            let mut buys = 0;
            let mut sells = 0;
            for o in pending.drain(..) {
                if let Order::Market { side, qty } = o {
                    if side == Side::Buy { buys += qty } else { sells += qty }
                }
            }
            assert!(pos + buys <= LIMIT && pos - sells >= -LIMIT, "limit at {n}");
            pos += buys - sells;
            vol += buys + sells;
            if n >= 2 {
                assert!(vol >= REQ_PER_DAY * n / SNAPS_PER_DAY, "behind at snapshot {n}");
            }
            let account = Account { position: pos, cash: STARTING_CASH - pos as i128 * 8_000_000, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
        assert!((pos - SIDE * HOLD).abs() <= Q);
    }
}
