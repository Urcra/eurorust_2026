//! Exhaustion fade: after a large 2 minute move stalls and the book stops pushing it, fade it with
//! a take-profit at half the move and a time stop. Taker round trips cover the minimum volume.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Ring buffer length for mid history (steps of 100 ms). Must be a power of two > `W`.
const RING: usize = 4096;
/// Look-back of the move being faded, in snapshots (2 minutes).
const W: usize = 1200;
/// Move threshold, in standard deviations of `W`-step moves.
const K: f64 = 2.0;
/// Exhaustion window: the last E steps must not continue the move.
const E: usize = 20;
/// Enter only when the imbalance leans against the move by more than this.
const G: f64 = 0.3;
/// Fade size, lots (1 BTC).
const QTY: i64 = 100_000;
/// Maximum holding time, snapshots (60 s).
const HOLD: u32 = 600;
/// Take-profit as a fraction of the faded move.
const TP_FRAC: f64 = 0.5;
/// Snapshots before a new entry is allowed after an entry.
const COOLDOWN: u32 = 600;
/// Snapshots of variance warm-up before trading the fade.
const WARMUP: f64 = 3000.0;
/// Minimum EWMA weight for the variance estimate (~30 min memory).
const VAR_ALPHA_MIN: f64 = 1.0 / 20000.0;

/// Volume fallback: chunk size of one taker round trip, lots (0.02 BTC).
const FB_QTY: i64 = 2_000;
/// Minimum volume per day of data, lots, and snapshots per day.
const MIN_DAILY: i64 = 1_000_000;
const SNAPS_PER_DAY: i64 = 864_000;
/// Feed gap that resets the move history, ms.
const GAP_MS: u64 = 2_000;

pub struct MyStrategy {
    hist: Vec<f64>,
    n: usize,
    var: f64,
    nv: f64,
    held: u32,
    cool: u32,
    entry: f64,
    tgt: i64,
    calls: i64,
    vol: i64,
    last_pos: i64,
    last_ts: u64,
}

impl Default for MyStrategy {
    fn default() -> Self {
        MyStrategy {
            hist: vec![0.0; RING],
            n: 0,
            var: 0.0,
            nv: 0.0,
            held: 0,
            cool: 0,
            entry: 0.0,
            tgt: 0,
            calls: 0,
            vol: 0,
            last_pos: 0,
            last_ts: 0,
        }
    }
}

impl MyStrategy {
    fn back(&self, k: usize) -> f64 {
        self.hist[(self.n - 1 - k) & (RING - 1)]
    }

    fn fade_orders(&mut self, book: &Book, acc: &Account) -> Vec<Order> {
        let mut o = Vec::new();
        let mid = book.mid_x2() as f64 / 2.0;
        self.hist[self.n & (RING - 1)] = mid;
        self.n += 1;
        if self.cool > 0 {
            self.cool -= 1;
        }
        if self.n <= W + 1 {
            return o;
        }
        let mv = mid - self.back(W);
        self.nv += 1.0;
        let a = (1.0 / self.nv).max(VAR_ALPHA_MIN);
        self.var += a * (mv * mv - self.var);

        let pos = acc.position;
        let bb = book.best_bid().price;
        let ba = book.best_ask().price;
        if pos != 0 {
            self.held += 1;
            if self.held >= HOLD {
                let side = if pos > 0 { Side::Sell } else { Side::Buy };
                o.push(Order::Market { side, qty: pos.abs() });
            } else if pos > 0 {
                o.push(Order::Limit { side: Side::Sell, price: self.tgt.max(ba), qty: pos });
            } else {
                o.push(Order::Limit { side: Side::Buy, price: self.tgt.min(bb).max(1), qty: -pos });
            }
            return o;
        }
        if self.cool == 0 && self.nv > WARMUP && self.var > 0.0 {
            let thr = K * self.var.sqrt();
            let s = if mv > 0.0 { 1.0 } else { -1.0 };
            let exhausted = s * (mid - self.back(E)) <= 0.0;
            let (bq, aq) = (book.best_bid().qty as f64, book.best_ask().qty as f64);
            let imb = if bq + aq > 0.0 { (bq - aq) / (bq + aq) } else { 0.0 };
            if mv.abs() > thr && exhausted && s * imb < -G {
                let d = -s;
                self.entry = mid;
                self.held = 0;
                self.cool = COOLDOWN;
                self.tgt = (mid + d * TP_FRAC * mv.abs()).round() as i64;
                let side = if d > 0.0 { Side::Buy } else { Side::Sell };
                o.push(Order::Market { side, qty: QTY });
            }
        }
        o
    }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Exhaustion fade" }
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        self.calls += 1;
        // Restart the history after a feed gap.
        if self.last_ts != 0 && ts_ms.saturating_sub(self.last_ts) > GAP_MS {
            self.n = 0;
        }
        self.last_ts = ts_ms;
        self.vol += (account.position - self.last_pos).abs();
        self.last_pos = account.position;

        let mut o = self.fade_orders(book, account);

        // Volume fallback: when behind pace, send a buy and a sell together as one round trip.
        let required = MIN_DAILY.saturating_mul(self.calls) / SNAPS_PER_DAY;
        let target = required + required / 4 + 2 * FB_QTY;
        let has_market = o.iter().any(|x| matches!(x, Order::Market { .. }));
        if !has_market && self.vol < target && account.position.abs() + FB_QTY <= 900_000 {
            o.push(Order::Market { side: Side::Buy, qty: FB_QTY });
            o.push(Order::Market { side: Side::Sell, qty: FB_QTY });
            self.vol += 2 * FB_QTY;
        }
        o
    }
}
