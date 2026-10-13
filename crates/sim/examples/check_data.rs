//! Streams a data file through the reader and prints a summary. Usage: `check_data FILE`.

use sim::{Reader, Snapshot};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: check_data FILE")?;
    let mut reader = Reader::open(&path)?;
    let (mut count, mut s) = (0u64, Snapshot::default());
    let (mut first, mut last) = (None, 0);
    while reader.next_into(&mut s)? {
        count += 1;
        first.get_or_insert(s.ts_ms);
        last = s.ts_ms;
    }
    println!("{path}: {:?}", reader.header());
    println!("{count} snapshots, ts_ms {} .. {last}", first.unwrap_or(0));
    Ok(())
}
