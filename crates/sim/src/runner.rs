//! Runs strategies over a data file and prints a report.
//!
//! The participant runner (`strategy/src/main.rs`) calls [`main`]. [`execute`] runs several
//! strategies side by side, each with its own engine and account, in one pass over the data.

use crate::api::{Account, Book, Info, Order, STARTING_CASH, Strategy};
use crate::data::{Reader, Snapshot};
use crate::engine::{Engine, RejectReason, Report};
use crate::fmt;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Sample data shipped with the starter kit.
pub const DEFAULT_DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/sample/btc_2026-09-28.jsonl");

const USAGE: &str = "options: [--data PATH] [--equity-csv PATH]";

/// Command-line options shared by all runners.
#[derive(Clone, Debug)]
pub struct Options {
    /// Data file. Defaults to [`DEFAULT_DATA`].
    pub data: PathBuf,
    /// If set, mark-to-mid PnL per strategy is written here once per minute of data time.
    pub equity_csv: Option<PathBuf>,
}

impl Options {
    /// Parses `--data PATH` and `--equity-csv PATH`. Other arguments are returned in order.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<(Options, Vec<String>), String> {
        let mut options = Options { data: PathBuf::from(DEFAULT_DATA), equity_csv: None };
        let mut rest = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--data" => options.data = args.next().ok_or("--data needs a path")?.into(),
                "--equity-csv" => options.equity_csv = Some(args.next().ok_or("--equity-csv needs a path")?.into()),
                "-h" | "--help" => return Err(USAGE.into()),
                _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n{USAGE}")),
                _ => rest.push(arg),
            }
        }
        Ok((options, rest))
    }
}

/// Results of one strategy.
pub struct Summary {
    pub name: String,
    pub info: Info,
    pub report: Report,
    /// Total time spent inside [`Strategy::call`].
    pub call_time: Duration,
    /// Longest single call.
    pub max_call: Duration,
}

/// Results of a run.
pub struct Run {
    pub data: PathBuf,
    pub snapshots: u64,
    pub first_ts_ms: Option<u64>,
    pub last_ts_ms: Option<u64>,
    /// Wall time of the whole run, reading the data included.
    pub elapsed: Duration,
    pub summaries: Vec<Summary>,
}

/// Measures the time spent in the wrapped strategy. The strategy itself never sees a clock.
struct Timed {
    inner: Box<dyn Strategy>,
    total: Duration,
    max: Duration,
}

impl Strategy for Timed {
    fn info(&self) -> Info {
        self.inner.info()
    }

    fn call(&mut self, ts_ms: u64, book: &Book, account: &Account) -> Vec<Order> {
        let start = Instant::now();
        let orders = self.inner.call(ts_ms, book, account);
        let elapsed = start.elapsed();
        self.total += elapsed;
        self.max = self.max.max(elapsed);
        orders
    }
}

struct Entry {
    name: String,
    info: Info,
    engine: Engine,
    strategy: Timed,
}

/// Runs every strategy over one pass of the data.
pub fn run(strategies: Vec<(String, Box<dyn Strategy>)>, options: &Options) -> Result<Run, String> {
    let started = Instant::now();
    let path = &options.data;
    let mut reader = Reader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut entries: Vec<Entry> = strategies
        .into_iter()
        .map(|(name, inner)| Entry {
            name,
            info: inner.info(),
            engine: Engine::new(),
            strategy: Timed { inner, total: Duration::ZERO, max: Duration::ZERO },
        })
        .collect();
    let mut equity = match &options.equity_csv {
        Some(p) => Some(Equity::create(p, &entries).map_err(|e| format!("{}: {e}", p.display()))?),
        None => None,
    };
    let mut snapshot = Snapshot::default();
    let (mut snapshots, mut first, mut last) = (0, None, None);
    while reader.next_into(&mut snapshot).map_err(|e| format!("{}: {e}", path.display()))? {
        for e in &mut entries {
            e.engine.step(&snapshot, &mut e.strategy);
        }
        snapshots += 1;
        first.get_or_insert(snapshot.ts_ms);
        last = Some(snapshot.ts_ms);
        if let Some(out) = &mut equity {
            out.row(&snapshot, &entries).map_err(|e| format!("equity csv: {e}"))?;
        }
    }
    if let Some(out) = equity {
        out.file.into_inner().map_err(|e| format!("equity csv: {}", e.error()))?;
    }
    let summaries = entries
        .into_iter()
        .map(|e| Summary {
            name: e.name,
            info: e.info,
            report: e.engine.finish(),
            call_time: e.strategy.total,
            max_call: e.strategy.max,
        })
        .collect();
    Ok(Run {
        data: path.clone(),
        snapshots,
        first_ts_ms: first,
        last_ts_ms: last,
        elapsed: started.elapsed(),
        summaries,
    })
}

/// Mark-to-mid equity, one row per minute of data time.
struct Equity {
    file: BufWriter<File>,
    minute: Option<u64>,
}

impl Equity {
    fn create(path: &PathBuf, entries: &[Entry]) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        write!(file, "ts_ms,utc,mid_usd")?;
        for e in entries {
            write!(file, ",{0}_position_btc,{0}_pnl_usd", e.name)?;
        }
        writeln!(file)?;
        Ok(Self { file, minute: None })
    }

    fn row(&mut self, snapshot: &Snapshot, entries: &[Entry]) -> std::io::Result<()> {
        let minute = snapshot.ts_ms / 60_000;
        if self.minute == Some(minute) {
            return Ok(());
        }
        self.minute = Some(minute);
        let mid_x2 = snapshot.bids[0].price + snapshot.asks[0].price;
        let (ts, utc) = (snapshot.ts_ms, fmt::utc_ms(snapshot.ts_ms));
        // Mid in 0.001 USD: half a tick is 0.005 USD.
        let mid = mid_x2 * 5;
        write!(self.file, "{ts},{utc},{}.{:03}", mid / 1_000, mid % 1_000)?;
        for e in entries {
            let (position, cash) = (e.engine.position(), e.engine.cash());
            let pnl = cash + position as i128 * mid_x2 as i128 / 2 - STARTING_CASH;
            write!(self.file, ",{},{}", fmt::btc(position), fmt::usd(pnl))?;
        }
        writeln!(self.file)
    }
}

/// Prints a human-readable report.
pub fn print(run: &Run) {
    println!("data          {}", run.data.display());
    match (run.first_ts_ms, run.last_ts_ms) {
        (Some(a), Some(b)) => println!(
            "              {} snapshots, {} .. {} UTC",
            fmt::count(run.snapshots),
            fmt::utc_ms(a),
            fmt::utc_ms(b)
        ),
        _ => println!("              no snapshots"),
    }
    println!("run time      {:.1} s", run.elapsed.as_secs_f64());
    for s in &run.summaries {
        print_summary(s, run.snapshots);
    }
    if run.summaries.len() > 1 {
        print_table(run);
    }
}

fn avg_us(s: &Summary, snapshots: u64) -> f64 {
    s.call_time.as_secs_f64() * 1e6 / snapshots.max(1) as f64
}

fn print_summary(s: &Summary, snapshots: u64) {
    let r = &s.report;
    println!();
    println!("== {} ==", s.name);
    println!("player        {} <{}>, strategy \"{}\"", s.info.nickname, s.info.email, s.info.strategy);
    println!("balance       {} USD at the end (started with {})", fmt::usd(r.final_balance()), fmt::usd(STARTING_CASH));
    println!("PnL           {} USD", fmt::usd(r.pnl));
    println!(
        "fees          {} USD net (taker {}, maker {})",
        fmt::usd(r.fees()),
        fmt::usd(r.taker_fees),
        fmt::usd(r.maker_fees)
    );
    println!("maker fills   {} ({} BTC)", fmt::count(r.maker_fills), fmt::btc(r.maker_qty));
    println!("taker fills   {} ({} BTC)", fmt::count(r.taker_fills), fmt::btc(r.taker_qty));
    let rejects: Vec<String> = RejectReason::ALL
        .iter()
        .filter(|&&reason| r.rejects[reason as usize] > 0)
        .map(|&reason| format!("{} {reason}", fmt::count(r.rejects[reason as usize])))
        .collect();
    println!("rejects       {}", if rejects.is_empty() { "none".to_string() } else { rejects.join(", ") });
    println!("max |pos|     {} BTC", fmt::btc(r.max_abs_position));
    let fallback = match r.liquidation_fallback_qty {
        0 => String::new(),
        q => format!(" ({} BTC at the fallback price)", fmt::btc(q)),
    };
    println!("liquidation   {} BTC{fallback}", fmt::btc(r.liquidation_qty));
    println!(
        "volume        {} BTC, minimum {} BTC for this data: {}",
        fmt::btc(r.volume()),
        fmt::btc(r.required_volume()),
        if r.meets_volume() { "met" } else { "NOT MET, would be disqualified" }
    );
    println!("time/call     avg {:.2} µs, max {:.1} µs", avg_us(s, snapshots), s.max_call.as_secs_f64() * 1e6);
}

fn print_table(run: &Run) {
    let w = run.summaries.iter().map(|s| s.name.len()).max().unwrap_or(0).max(8);
    println!();
    println!(
        "{:<w$}  {:>17}  {:>16}  {:>14}  {:>9}  {:>9}  {:>9}  {:>14}  {:>8}",
        "strategy", "balance USD", "PnL USD", "net fees USD", "maker", "taker", "max |pos|", "volume BTC", "µs/call"
    );
    for s in &run.summaries {
        let r = &s.report;
        println!(
            "{:<w$}  {:>17}  {:>16}  {:>14}  {:>9}  {:>9}  {:>9}  {:>14}  {:>8.2}",
            s.name,
            fmt::usd(r.final_balance()),
            fmt::usd(r.pnl),
            fmt::usd(r.fees()),
            fmt::count(r.maker_fills),
            fmt::count(r.taker_fills),
            fmt::btc(r.max_abs_position),
            format!("{}{}", fmt::btc(r.volume()), if r.meets_volume() { "  " } else { " !" }),
            avg_us(s, run.snapshots)
        );
    }
    if run.summaries.iter().any(|s| !s.report.meets_volume()) {
        let minimum = run.summaries[0].report.required_volume();
        println!("! below the minimum volume of {} BTC for this data", fmt::btc(minimum));
    }
}

/// Runs the strategies, prints the report, and returns the process exit code.
pub fn execute(strategies: Vec<(String, Box<dyn Strategy>)>, options: &Options) -> ExitCode {
    match run(strategies, options) {
        Ok(run) => {
            print(&run);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Entry point of the participant runner: `cargo run --release -- [--data PATH] [--equity-csv PATH]`.
pub fn main<S: Strategy + Default + 'static>() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok((options, rest)) if rest.is_empty() => options,
        Ok((_, rest)) => {
            eprintln!("unexpected argument {}\n{USAGE}", rest[0]);
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let strategy = S::default();
    let name = strategy.info().strategy.to_string();
    execute(vec![(name, Box::new(strategy))], &options)
}
