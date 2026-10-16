//! Momentum taker: on an extreme top-of-book imbalance backed by a $3 move, take 0.05 BTC and rest
//! a passive profit target, with a stop and a time limit. Trades only as much as the volume needs.

use sim::{Account, Book, Info, Order, Side, Strategy};

/// Lots per trade (0.05 BTC).
const SIZE: i64 = 5_000;
/// Top-of-book imbalance threshold for an entry.
const IMB_TH: f64 = 0.9;
/// Required mid move (ticks) in the signal direction over the last `JUMP_LOOKBACK` snapshots.
const JUMP: i64 = 300;
const JUMP_LOOKBACK: usize = 10;
/// Passive profit target, ticks beyond the touch at entry.
const TARGET: i64 = 2_500;
/// Stop: adverse mid move from the entry reference, in ticks.
const STOP: i64 = 4_000;
/// Maximum holding time in snapshots (30 s).
const MAX_HOLD: u32 = 300;
/// Minimum volume per snapshot in lots, as num/den. Entries stop at 1.25x the minimum plus a buffer.
const MIN_VOL_NUM: i64 = 1_000_000;
const MIN_VOL_DEN: i64 = 864_000;
const BUDGET_BUFFER: i64 = 3 * SIZE;
/// Below this cushion over the minimum, a moderate imbalance is enough to enter (any at half of it).
const URGENT_BUFFER: i64 = SIZE;
/// Snapshots before the urgent rules apply.
const WARMUP: i64 = 3_000;
const RING: usize = 16;

pub struct MyStrategy {
    snaps: i64,
    /// Volume traded (lots), estimated from position changes (a lower bound).
    volume: i64,
    last_pos: i64,
    /// Ring buffer of 2x mid prices.
    mids: [i64; RING],
    /// Direction of the open trade (0 = flat), its reference price, target and age.
    dir: i64,
    eref: i64,
    target: i64,
    age: u32,
}

impl Default for MyStrategy {
    fn default() -> Self {
        MyStrategy { snaps: 0, volume: 0, last_pos: 0, mids: [0; RING], dir: 0, eref: 0, target: 0, age: 0 }
    }
}

impl MyStrategy {
    fn required(&self) -> i64 {
        self.snaps * MIN_VOL_NUM / MIN_VOL_DEN
    }
}

impl Strategy for MyStrategy {
    fn info(&self) -> Info {
        Info { nickname: "Urcra", email: "you@example.com", strategy: "Momentum taker, passive target" }
    }

    fn call(&mut self, _ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        let mut orders = Vec::new();
        if book.bids.is_empty() || book.asks.is_empty() {
            return orders;
        }
        let bid = book.bids[0].price;
        let ask = book.asks[0].price;
        let mid2 = bid + ask;
        let idx = (self.snaps as usize) % RING;
        let past = if self.snaps as usize >= JUMP_LOOKBACK {
            Some(self.mids[(self.snaps as usize - JUMP_LOOKBACK) % RING])
        } else {
            None
        };
        self.mids[idx] = mid2;
        self.snaps += 1;

        let pos = account.position;
        self.volume += (pos - self.last_pos).abs();
        self.last_pos = pos;

        let (bq, aq) = (book.bids[0].qty as f64, book.asks[0].qty as f64);
        let imb = if bq + aq > 0.0 { (bq - aq) / (bq + aq) } else { 0.0 };

        if pos != 0 {
            let d = if pos > 0 { 1 } else { -1 };
            if self.dir != d {
                // Position we did not expect (e.g. the exit filled first): manage it anyway.
                self.dir = d;
                self.eref = if d > 0 { ask } else { bid };
                self.target = self.eref + d * TARGET;
                self.age = 0;
            }
            self.age += 1;
            let exit_side = if d > 0 { Side::Sell } else { Side::Buy };
            let qty = pos.abs();
            let adverse = (2 * self.eref - mid2) * d / 2;
            if adverse > STOP || self.age >= MAX_HOLD {
                orders.push(Order::Market { side: exit_side, qty });
            } else {
                orders.push(Order::Limit { side: exit_side, price: self.target, qty });
            }
            return orders;
        }

        // Flat.
        self.dir = 0;
        let required = self.required();
        if self.volume >= required + required / 4 + BUDGET_BUFFER {
            return orders;
        }
        let urgent = self.snaps > WARMUP && self.volume < required + URGENT_BUFFER;
        let d = if imb > IMB_TH {
            1
        } else if imb < -IMB_TH {
            -1
        } else if urgent && (imb.abs() > 0.5 || self.volume < required + URGENT_BUFFER / 2) {
            if imb >= 0.0 { 1 } else { -1 }
        } else {
            0
        };
        if d == 0 {
            return orders;
        }
        if !urgent {
            match past {
                Some(p) if (mid2 - p) / 2 * d >= JUMP => {}
                _ => return orders,
            }
        }
        self.dir = d;
        self.eref = if d > 0 { ask } else { bid };
        self.target = self.eref + d * TARGET;
        self.age = 0;
        let (entry_side, exit_side) = if d > 0 { (Side::Buy, Side::Sell) } else { (Side::Sell, Side::Buy) };
        if urgent {
            orders.push(Order::Market { side: entry_side, qty: SIZE });
        } else {
            // Limit at the touch we saw, so it never walks the thin side of the book.
            orders.push(Order::Limit { side: entry_side, price: self.eref, qty: SIZE });
        }
        if self.target > 0 {
            orders.push(Order::Limit { side: exit_side, price: self.target, qty: SIZE });
        }
        orders
    }
}
