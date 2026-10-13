//! Integer units used throughout the simulator.

/// Price in ticks. 1 tick = 0.01 USD.
pub type Price = i64;

/// Quantity in lots. 1 lot = 0.00001 BTC.
pub type Qty = i64;

/// Cash in units of 1e-7 USD, so that `price * qty` is exact.
pub type Cash = i128;

/// USD per tick, as `(numerator, denominator)`.
pub const TICK_SIZE: (i64, i64) = (1, 100);

/// BTC per lot, as `(numerator, denominator)`.
pub const LOT_SIZE: (i64, i64) = (1, 100_000);

/// One price level of the order book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    /// Price in ticks.
    pub price: Price,
    /// Total quantity at this price, in lots. Always positive.
    pub qty: Qty,
}
