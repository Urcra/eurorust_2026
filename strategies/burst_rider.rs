//! Burst Rider: small taker round trips for the minimum volume, plus 0.1 BTC trips that ride a fast
//! move while the book still leans the same way, switched off for the run after a drawdown.

use sim::{Account, Book, Info, Order, Side, Strategy};

const REQ_PER_DAY: i64 = 1_000_000;
const SNAPS_PER_DAY: i64 = 864_000;

/// Volume trip size in lots (0.01 BTC).
const Q: i64 = 1_000;
/// Entry imbalance thresholds, from relaxed to most urgent.
const TH: f64 = 0.99;
const TH_FORCE: f64 = 0.9;
const TH_PANIC: f64 = 0.5;
/// Exit when the imbalance turns this far against the position.
const FLIP: f64 = 0.9;
/// Burst trips: when the mid moved BURST_BPS over the last BK snapshots and the imbalance still
/// leans BURST_IMB the same way, ride it with QB lots until the imbalance turns BURST_FLIP against.
const QB: i64 = 10_000;
const BURST_BPS: f64 = 3.0;
const BK: usize = 30;
const BURST_IMB: f64 = 0.9;
/// Stop burst trips for the run once their summed result falls this many bps below its peak.
const KILL_DD: f64 = 40.0;
const BURST_FLIP: f64 = 0.99;
/// Max holding time, in snapshots (2 minutes).
const HOLD: u32 = 1_200;
/// Volume surplus bands, in lots above the requirement.
const B_HI: i64 = 15_000;
const B_LO: i64 = 2_500;
const B_PANIC: i64 = 1_500;
const B_EMERG: i64 = 800;
/// While in a position, exit at once when the surplus falls below this.
const B_EXIT: i64 = 600;
/// Volatility gate: trade only when the volatility rank is at most F_MIN + (1 - F_MIN) * x^GAMMA.
const F_MIN: f64 = 0.05;
const GAMMA: i32 = 4;
/// EWMA weight of the volatility (1 minute).
const RV_A: f64 = 1.0 / 600.0;
/// Volatility history: a sample every SAMP snapshots, RING samples, gate on after WARM.
const RING: usize = 512;
const SAMP: u32 = 50;
const WARM: usize = 20;

pub struct MyStrategy {
    n: i64,
    vol: i64,
    last_pos: i64,
    /// Snapshots spent in the current position.
    held: u32,
    /// Timestamp of the entry fill, so feed gaps count towards the holding time.
    entry_ts: u64,
    /// Calls to wait for an entry order to show up in the position.
    pending: u32,
    /// The current trip is a burst trip.
    burst: bool,
    burst_on: bool,
    b_cum: f64,
    b_peak: f64,
    b_cash0: i128,
    mids: [f64; 64],
    prev_mid: f64,
    /// Short-term realized volatility, bps per snapshot.
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
            held: 0,
            entry_ts: 0,
            pending: 0,
            burst: false,
            burst_on: true,
            b_cum: 0.0,
            b_peak: 0.0,
            b_cash0: 0,
            mids: [0.0; 64],
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
}

/// Top-of-book imbalance in [-1, 1]: positive when the bid holds more than the ask.
fn imbalance(book: &Book) -> f64 {
    let (b, a) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
    if b + a <= 0.0 { 0.0 } else { (b - a) / (b + a) }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Burst Rider" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.n += 1;
        self.update_vol(book);
        let pos = account.position;
        self.vol += (pos - self.last_pos).abs();
        if self.burst && self.last_pos != 0 && pos == 0 {
            let mid = (book.bids[0].price + book.asks[0].price) as f64 * 0.5;
            let bps = (account.cash - self.b_cash0) as f64 / (mid * QB as f64) * 1e4;
            if bps.is_finite() {
                self.b_cum += bps;
                self.b_peak = self.b_peak.max(self.b_cum);
                if self.b_cum < self.b_peak - KILL_DD {
                    self.burst_on = false;
                }
            }
            self.burst = false;
        }
        if self.last_pos == 0 && pos != 0 {
            self.held = 0;
            self.entry_ts = ts_ms;
            self.pending = 0;
        }
        self.last_pos = pos;
        let imb = imbalance(book);
        let mid_now = (book.bids[0].price + book.asks[0].price) as f64 * 0.5;
        let slot = (self.n as usize) % 64;
        self.mids[slot] = mid_now;
        let r_k = if self.n as usize > BK {
            let old = self.mids[(self.n as usize - BK) % 64];
            if old > 0.0 { (mid_now - old) / old * 1e4 } else { 0.0 }
        } else {
            0.0
        };
        // Surplus over the requirement for a run ending a couple of snapshots from now.
        let s = self.vol - Self::required(self.n + 2);

        if pos == 0 {
            self.held = 0;
            if self.pending > 0 {
                self.pending -= 1;
                if self.pending > 0 {
                    return Vec::new();
                }
            }
            if QB > 0 && self.burst_on && r_k.abs() >= BURST_BPS && r_k.signum() * imb >= BURST_IMB {
                let side = if r_k > 0.0 { Side::Buy } else { Side::Sell };
                self.pending = 2;
                self.burst = true;
                self.b_cash0 = account.cash;
                return vec![Order::Market { side, qty: QB }];
            }
            self.burst = false;
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

        // In a position: hold while the book still leans our way.
        self.held = self.held.saturating_add(1);
        let long = pos > 0;
        let against = if long { -imb } else { imb };
        let timed_out = self.held >= HOLD || ts_ms.saturating_sub(self.entry_ts) >= HOLD as u64 * 100;
        let flip = if self.burst { BURST_FLIP } else { FLIP };
        if timed_out || against >= flip || s < B_EXIT {
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

    /// On a flat book volume keeps up with the requirement at every snapshot.
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

    /// With an extreme imbalance all the time, volume stays close to the cap.
    #[test]
    fn cap_holds() {
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
            let req = REQ_PER_DAY * n / SNAPS_PER_DAY;
            if n >= 2 {
                assert!(vol >= req, "behind at snapshot {n}");
            }
            assert!(vol <= req + B_HI + 2 * Q, "over cap at snapshot {n}");
            let p = 8_000_000 + (n % 7) * 13;
            let (bq, aq) = if (n / 50) % 2 == 0 { (100_000, 1) } else { (1, 100_000) };
            let bids = [Level { price: p, qty: bq }];
            let asks = [Level { price: p + 1, qty: aq }];
            let book = Book { bids: &bids, asks: &asks };
            let account = Account { position: pos, cash: STARTING_CASH, resting: &[] };
            pending = s.call(n as u64 * 100, &book, &account);
        }
    }
}
