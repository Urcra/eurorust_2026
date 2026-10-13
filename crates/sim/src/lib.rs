//! Simulator for the Rust BTC Trading Challenge.
//!
//! Participants only need [`api`]: implement [`Strategy`] for `MyStrategy`. All prices,
//! quantities and cash amounts are integers; see [`types`] for the units.

pub mod api;
pub mod data;
pub mod engine;
pub mod fmt;
pub mod runner;
pub mod types;

pub use api::{
    Account, Book, CASH_PER_USD, Info, MAKER_FEE_PPM, MAX_ORDERS, MIN_DAILY_VOLUME, Order, POSITION_LIMIT, Resting,
    STARTING_CASH, Side, Strategy, TAKER_FEE_PPM,
};
pub use data::{DataError, Header, Reader, Snapshot, Writer};
pub use types::{Cash, LOT_SIZE, Level, Price, Qty, TICK_SIZE};
