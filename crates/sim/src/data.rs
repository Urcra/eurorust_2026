//! Challenge data format: JSON Lines, one header line followed by one snapshot per line.
//!
//! ```text
//! {"format":"btcbook","version":1,"levels":50,"interval_ms":100,"tick_size":[1,100],"lot_size":[1,100000]}
//! {"ts_ms":1790607733700,"bids":[[8302142,12345],[8302141,500]],"asks":[[8302143,1000]]}
//! ```
//!
//! Prices are in ticks and quantities in lots (see [`crate::types`]). Levels are best first, and
//! only real levels are listed: a side may have fewer than `levels` entries, never more.
//!
//! The reader is strict: it accepts only integers, no string escapes, no unknown or duplicate
//! keys, and it validates every snapshot (see [`Snapshot::validate`]). It streams line by line,
//! so memory use does not depend on file size.

use crate::types::{LOT_SIZE, Level, TICK_SIZE};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

/// Value of the header's `format` field.
pub const FORMAT: &str = "btcbook";

/// The only format version this crate reads and writes.
pub const VERSION: u32 = 1;

/// File header: the first line of every data file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Maximum number of levels per side.
    pub levels: u32,
    /// Nominal spacing between snapshots. Snapshots may be missing (dropped by the converter),
    /// but timestamps are always multiples of this interval.
    pub interval_ms: u64,
    /// USD per tick, as `(numerator, denominator)`. Always [`TICK_SIZE`].
    pub tick_size: (i64, i64),
    /// BTC per lot, as `(numerator, denominator)`. Always [`LOT_SIZE`].
    pub lot_size: (i64, i64),
}

impl Default for Header {
    fn default() -> Self {
        Self { levels: 50, interval_ms: 100, tick_size: TICK_SIZE, lot_size: LOT_SIZE }
    }
}

/// One order book snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Snapshot time, milliseconds since the Unix epoch (UTC).
    pub ts_ms: u64,
    /// Bid levels, best (highest price) first.
    pub bids: Vec<Level>,
    /// Ask levels, best (lowest price) first.
    pub asks: Vec<Level>,
}

/// Why a snapshot is not a valid book.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Invalid {
    /// One side has no levels.
    EmptySide,
    /// One side has more than `levels` levels.
    TooManyLevels,
    /// A price is zero or negative.
    NonPositivePrice,
    /// A quantity is zero or negative.
    NonPositiveQty,
    /// Prices on a side are not strictly ordered best first (includes duplicate prices).
    Unsorted,
    /// Best bid is greater than or equal to best ask.
    Crossed,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Invalid::EmptySide => "empty side",
            Invalid::TooManyLevels => "too many levels",
            Invalid::NonPositivePrice => "non-positive price",
            Invalid::NonPositiveQty => "non-positive quantity",
            Invalid::Unsorted => "levels not strictly ordered",
            Invalid::Crossed => "crossed book",
        })
    }
}

impl Snapshot {
    /// Checks that this is a well-formed, uncrossed book with at most `levels` levels per side.
    pub fn validate(&self, levels: usize) -> Result<(), Invalid> {
        for (side, better) in [(&self.bids, i64::gt as fn(&i64, &i64) -> bool), (&self.asks, i64::lt)] {
            if side.is_empty() {
                return Err(Invalid::EmptySide);
            }
            if side.len() > levels {
                return Err(Invalid::TooManyLevels);
            }
            if side.iter().any(|l| l.price <= 0) {
                return Err(Invalid::NonPositivePrice);
            }
            if side.iter().any(|l| l.qty <= 0) {
                return Err(Invalid::NonPositiveQty);
            }
            if side.windows(2).any(|w| !better(&w[0].price, &w[1].price)) {
                return Err(Invalid::Unsorted);
            }
        }
        if self.bids[0].price >= self.asks[0].price {
            return Err(Invalid::Crossed);
        }
        Ok(())
    }
}

/// Error reading or writing a data file.
#[derive(Debug)]
pub enum DataError {
    /// Underlying I/O error.
    Io(io::Error),
    /// Malformed or invalid content. `line` is 1-based.
    Format { line: u64, msg: String },
}

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataError::Io(e) => write!(f, "I/O error: {e}"),
            DataError::Format { line, msg } => write!(f, "line {line}: {msg}"),
        }
    }
}

impl std::error::Error for DataError {}

impl From<io::Error> for DataError {
    fn from(e: io::Error) -> Self {
        DataError::Io(e)
    }
}

/// Streaming reader for the challenge data format.
pub struct Reader<R> {
    input: R,
    header: Header,
    line: String,
    line_no: u64,
    last_ts: Option<u64>,
}

impl Reader<BufReader<File>> {
    /// Opens a data file and reads its header.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DataError> {
        Reader::new(BufReader::with_capacity(1 << 20, File::open(path)?))
    }
}

impl<R: BufRead> Reader<R> {
    /// Reads the header from `input`.
    pub fn new(mut input: R) -> Result<Self, DataError> {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Err(DataError::Format { line: 1, msg: "empty file, expected a header".into() });
        }
        let header = parse_header(trim_newline(&line)).map_err(|msg| DataError::Format { line: 1, msg })?;
        Ok(Self { input, header, line, line_no: 1, last_ts: None })
    }

    /// The file header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Reads the next snapshot into `snapshot`, reusing its buffers.
    /// Returns `Ok(false)` at end of file.
    pub fn next_into(&mut self, snapshot: &mut Snapshot) -> Result<bool, DataError> {
        self.line.clear();
        if self.input.read_line(&mut self.line)? == 0 {
            return Ok(false);
        }
        self.line_no += 1;
        let line = self.line_no;
        let err = |msg: String| DataError::Format { line, msg };
        parse_snapshot(trim_newline(&self.line), snapshot).map_err(err)?;
        snapshot.validate(self.header.levels as usize).map_err(|e| err(e.to_string()))?;
        check_ts(self.last_ts, snapshot.ts_ms, self.header.interval_ms).map_err(err)?;
        self.last_ts = Some(snapshot.ts_ms);
        Ok(true)
    }
}

impl<R: BufRead> Iterator for Reader<R> {
    type Item = Result<Snapshot, DataError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut snapshot = Snapshot::default();
        match self.next_into(&mut snapshot) {
            Ok(true) => Some(Ok(snapshot)),
            Ok(false) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Writer for the challenge data format. Validates every snapshot it writes.
pub struct Writer<W: Write> {
    output: BufWriter<W>,
    header: Header,
    buf: String,
    line_no: u64,
    last_ts: Option<u64>,
}

impl<W: Write> Writer<W> {
    /// Writes `header` to `output`.
    pub fn new(output: W, header: Header) -> Result<Self, DataError> {
        check_header(&header).map_err(|msg| DataError::Format { line: 1, msg })?;
        let mut output = BufWriter::with_capacity(1 << 20, output);
        let (t, l) = (header.tick_size, header.lot_size);
        writeln!(
            output,
            r#"{{"format":"{FORMAT}","version":{VERSION},"levels":{},"interval_ms":{},"tick_size":[{},{}],"lot_size":[{},{}]}}"#,
            header.levels, header.interval_ms, t.0, t.1, l.0, l.1
        )?;
        Ok(Self { output, header, buf: String::new(), line_no: 1, last_ts: None })
    }

    /// Appends one snapshot. Fails if it is invalid or its timestamp does not follow the
    /// previous one.
    pub fn write(&mut self, snapshot: &Snapshot) -> Result<(), DataError> {
        use fmt::Write as _;
        let line = self.line_no + 1;
        let err = |msg: String| DataError::Format { line, msg };
        snapshot.validate(self.header.levels as usize).map_err(|e| err(e.to_string()))?;
        check_ts(self.last_ts, snapshot.ts_ms, self.header.interval_ms).map_err(err)?;
        let b = &mut self.buf;
        b.clear();
        write!(b, r#"{{"ts_ms":{},"bids":["#, snapshot.ts_ms).unwrap();
        write_levels(b, &snapshot.bids);
        b.push_str(r#"],"asks":["#);
        write_levels(b, &snapshot.asks);
        b.push_str("]}\n");
        self.output.write_all(b.as_bytes())?;
        self.line_no = line;
        self.last_ts = Some(snapshot.ts_ms);
        Ok(())
    }

    /// Number of snapshots written so far.
    pub fn count(&self) -> u64 {
        self.line_no - 1
    }

    /// Flushes and returns the underlying writer.
    pub fn finish(self) -> Result<W, DataError> {
        self.output.into_inner().map_err(|e| DataError::Io(e.into_error()))
    }
}

fn write_levels(b: &mut String, levels: &[Level]) {
    use fmt::Write as _;
    for (i, l) in levels.iter().enumerate() {
        if i > 0 {
            b.push(',');
        }
        write!(b, "[{},{}]", l.price, l.qty).unwrap();
    }
}

fn trim_newline(line: &str) -> &str {
    line.strip_suffix('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).unwrap_or(line)
}

fn check_header(h: &Header) -> Result<(), String> {
    if h.levels == 0 {
        return Err("levels must be positive".into());
    }
    if h.interval_ms == 0 {
        return Err("interval_ms must be positive".into());
    }
    if h.tick_size != TICK_SIZE || h.lot_size != LOT_SIZE {
        return Err(format!(
            "unsupported units: tick_size {:?}, lot_size {:?} (expected {TICK_SIZE:?}, {LOT_SIZE:?})",
            h.tick_size, h.lot_size
        ));
    }
    Ok(())
}

fn check_ts(last: Option<u64>, ts: u64, interval_ms: u64) -> Result<(), String> {
    if !ts.is_multiple_of(interval_ms) {
        return Err(format!("ts_ms {ts} is not a multiple of interval_ms {interval_ms}"));
    }
    match last {
        Some(last) if ts <= last => Err(format!("ts_ms {ts} does not follow previous {last}")),
        _ => Ok(()),
    }
}

fn parse_header(line: &str) -> Result<Header, String> {
    let (mut format, mut version) = (None, None);
    let (mut levels, mut interval_ms, mut tick_size, mut lot_size) = (None, None, None, None);
    let mut p = Parser::new(line);
    p.object(|p, key| match key {
        "format" => set(&mut format, key, p.string()?.to_owned()),
        "version" => set(&mut version, key, p.int()?),
        "levels" => set(&mut levels, key, p.int()?),
        "interval_ms" => set(&mut interval_ms, key, p.int()?),
        "tick_size" => set(&mut tick_size, key, p.pair()?),
        "lot_size" => set(&mut lot_size, key, p.pair()?),
        _ => Err(format!("unknown header key \"{key}\"")),
    })?;
    p.end()?;
    let format = format.ok_or("missing \"format\"")?;
    if format != FORMAT {
        return Err(format!("format is \"{format}\", expected \"{FORMAT}\""));
    }
    let version = version.ok_or("missing \"version\"")?;
    if version != VERSION as i64 {
        return Err(format!("unsupported version {version}, expected {VERSION}"));
    }
    let header = Header {
        levels: u32::try_from(levels.ok_or("missing \"levels\"")?).map_err(|_| "levels out of range")?,
        interval_ms: u64::try_from(interval_ms.ok_or("missing \"interval_ms\"")?)
            .map_err(|_| "interval_ms out of range")?,
        tick_size: tick_size.ok_or("missing \"tick_size\"")?,
        lot_size: lot_size.ok_or("missing \"lot_size\"")?,
    };
    check_header(&header)?;
    Ok(header)
}

fn parse_snapshot(line: &str, s: &mut Snapshot) -> Result<(), String> {
    s.bids.clear();
    s.asks.clear();
    let (mut ts, mut bids, mut asks) = (None, None, None);
    let mut p = Parser::new(line);
    p.object(|p, key| match key {
        "ts_ms" => set(&mut ts, key, p.int()?),
        "bids" => set(&mut bids, key, p.levels(&mut s.bids)?),
        "asks" => set(&mut asks, key, p.levels(&mut s.asks)?),
        _ => Err(format!("unknown snapshot key \"{key}\"")),
    })?;
    p.end()?;
    s.ts_ms = u64::try_from(ts.ok_or("missing \"ts_ms\"")?).map_err(|_| "negative ts_ms")?;
    bids.ok_or("missing \"bids\"")?;
    asks.ok_or("missing \"asks\"")?;
    Ok(())
}

fn set<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!("duplicate key \"{key}\""));
    }
    Ok(())
}

/// Minimal strict JSON parser for the fixed shapes of this format.
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self { b: s.as_bytes(), i: 0 }
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let hit = self.b.get(self.i) == Some(&c);
        self.i += hit as usize;
        hit
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) { Ok(()) } else { Err(format!("expected '{}' at column {}", c as char, self.i + 1)) }
    }

    fn end(&mut self) -> Result<(), String> {
        self.ws();
        if self.i == self.b.len() { Ok(()) } else { Err(format!("trailing characters at column {}", self.i + 1)) }
    }

    fn string(&mut self) -> Result<&'a str, String> {
        self.expect(b'"')?;
        let start = self.i;
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'"' => {
                    let s = &self.b[start..self.i];
                    self.i += 1;
                    return std::str::from_utf8(s).map_err(|e| e.to_string());
                }
                b'\\' => return Err(format!("string escapes are not supported (column {})", self.i + 1)),
                _ => self.i += 1,
            }
        }
        Err("unterminated string".into())
    }

    fn int(&mut self) -> Result<i64, String> {
        self.ws();
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = self.i;
        while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
            self.i += 1;
        }
        if self.i == digits || matches!(self.b.get(self.i), Some(b'.' | b'e' | b'E')) {
            return Err(format!("expected an integer at column {}", start + 1));
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).unwrap();
        s.parse().map_err(|_| format!("integer out of range at column {}", start + 1))
    }

    fn pair(&mut self) -> Result<(i64, i64), String> {
        self.expect(b'[')?;
        let a = self.int()?;
        self.expect(b',')?;
        let b = self.int()?;
        self.expect(b']')?;
        Ok((a, b))
    }

    fn levels(&mut self, out: &mut Vec<Level>) -> Result<(), String> {
        self.expect(b'[')?;
        if self.eat(b']') {
            return Ok(());
        }
        loop {
            let (price, qty) = self.pair()?;
            out.push(Level { price, qty });
            if !self.eat(b',') {
                return self.expect(b']');
            }
        }
    }

    fn object(&mut self, mut field: impl FnMut(&mut Self, &str) -> Result<(), String>) -> Result<(), String> {
        self.expect(b'{')?;
        if self.eat(b'}') {
            return Ok(());
        }
        loop {
            let key = self.string()?;
            self.expect(b':')?;
            field(self, key)?;
            if !self.eat(b',') {
                return self.expect(b'}');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        r#"{"format":"btcbook","version":1,"levels":50,"interval_ms":100,"tick_size":[1,100],"lot_size":[1,100000]}"#;

    fn lv(price: i64, qty: i64) -> Level {
        Level { price, qty }
    }

    fn snap(ts_ms: u64) -> Snapshot {
        Snapshot {
            ts_ms,
            bids: vec![lv(8_300_000, 5), lv(8_299_999, 100_000)],
            asks: vec![lv(8_300_001, 1), lv(8_300_010, 12_345_678)],
        }
    }

    fn write_all(header: Header, snaps: &[Snapshot]) -> String {
        let mut w = Writer::new(Vec::new(), header).unwrap();
        for s in snaps {
            w.write(s).unwrap();
        }
        String::from_utf8(w.finish().unwrap()).unwrap()
    }

    fn read_all(text: &str) -> Result<Vec<Snapshot>, DataError> {
        Reader::new(text.as_bytes())?.collect()
    }

    fn read_err(body: &str) -> String {
        read_all(&format!("{HEADER}\n{body}\n")).unwrap_err().to_string()
    }

    #[test]
    fn round_trip() {
        let snaps = vec![snap(100), snap(200), snap(1_000)];
        let text = write_all(Header::default(), &snaps);
        assert_eq!(text.lines().next().unwrap(), HEADER);
        let reader = Reader::new(text.as_bytes()).unwrap();
        assert_eq!(reader.header(), &Header::default());
        assert_eq!(read_all(&text).unwrap(), snaps);
    }

    #[test]
    fn round_trip_is_byte_identical() {
        let text = write_all(Header::default(), &[snap(100), snap(300)]);
        let again = write_all(Header::default(), &read_all(&text).unwrap());
        assert_eq!(text, again);
    }

    #[test]
    fn exact_line_format() {
        let text = write_all(Header::default(), &[snap(100)]);
        assert_eq!(
            text.lines().nth(1).unwrap(),
            r#"{"ts_ms":100,"bids":[[8300000,5],[8299999,100000]],"asks":[[8300001,1],[8300010,12345678]]}"#
        );
    }

    #[test]
    fn accepts_whitespace_key_order_and_crlf() {
        let body = "{ \"asks\" : [ [11, 1] ] , \"bids\":[[10,2]], \"ts_ms\": 200 }\r";
        let snaps = read_all(&format!("{HEADER}\r\n{body}\n")).unwrap();
        assert_eq!(snaps, vec![Snapshot { ts_ms: 200, bids: vec![lv(10, 2)], asks: vec![lv(11, 1)] }]);
    }

    #[test]
    fn empty_body_is_fine() {
        assert_eq!(read_all(&format!("{HEADER}\n")).unwrap(), vec![]);
    }

    #[test]
    fn rejects_bad_headers() {
        let bad = |h: &str| Reader::new(h.as_bytes()).err().unwrap().to_string();
        assert!(bad("").contains("empty file"));
        assert!(bad(&HEADER.replace("btcbook", "other")).contains("format"));
        assert!(bad(&HEADER.replace("\"version\":1", "\"version\":2")).contains("unsupported version"));
        assert!(bad(&HEADER.replace("[1,100000]", "[1,10000]")).contains("unsupported units"));
        assert!(bad(&HEADER.replace(",\"levels\":50", "")).contains("missing \"levels\""));
        assert!(bad(&HEADER.replace("{", "{\"x\":1,")).contains("unknown header key"));
        assert!(bad(&HEADER.replace("{", "{\"levels\":5,")).contains("duplicate key"));
    }

    #[test]
    fn rejects_malformed_snapshots() {
        let ok = r#"{"ts_ms":100,"bids":[[10,1]],"asks":[[11,1]]}"#;
        assert!(read_err(&ok.replace("[10,1]", "[10.5,1]")).contains("expected an integer"));
        assert!(read_err(&ok.replace("[10,1]", "[1e3,1]")).contains("expected an integer"));
        assert!(read_err(&ok.replace("[10,1]", "[99999999999999999999,1]")).contains("out of range"));
        assert!(read_err(&format!("{ok}x")).contains("trailing"));
        assert!(read_err(&ok.replace("\"bids\"", "\"bid\"")).contains("unknown snapshot key"));
        assert!(read_err(&ok.replace(",\"asks\":[[11,1]]", "")).contains("missing \"asks\""));
        assert!(read_err(&ok.replace(":100,", ":-100,")).contains("negative ts_ms"));
        assert!(read_err("").starts_with("line 2:"));
    }

    #[test]
    fn rejects_invalid_books() {
        let book = |bids: &str, asks: &str| format!(r#"{{"ts_ms":100,"bids":[{bids}],"asks":[{asks}]}}"#);
        assert!(read_err(&book("", "[11,1]")).contains("empty side"));
        assert!(read_err(&book("[10,1]", "[10,1]")).contains("crossed"));
        assert!(read_err(&book("[10,1]", "[9,1]")).contains("crossed"));
        assert!(read_err(&book("[9,1],[10,1]", "[11,1]")).contains("not strictly ordered"));
        assert!(read_err(&book("[10,1],[10,1]", "[11,1]")).contains("not strictly ordered"));
        assert!(read_err(&book("[10,1]", "[12,1],[11,1]")).contains("not strictly ordered"));
        assert!(read_err(&book("[10,0]", "[11,1]")).contains("non-positive quantity"));
        assert!(read_err(&book("[0,1]", "[11,1]")).contains("non-positive price"));
        let many: Vec<String> = (1..=51).map(|p| format!("[{p},1]")).collect();
        assert!(read_err(&book("[1,1]", &many.join(","))).contains("too many levels"));
    }

    #[test]
    fn rejects_bad_timestamps() {
        let at = |ts: u64| format!(r#"{{"ts_ms":{ts},"bids":[[10,1]],"asks":[[11,1]]}}"#);
        assert!(read_err(&format!("{}\n{}", at(200), at(200))).contains("does not follow"));
        assert!(read_err(&format!("{}\n{}", at(200), at(100))).contains("does not follow"));
        assert!(read_err(&at(150)).contains("not a multiple"));
    }

    #[test]
    fn writer_validates() {
        let mut w = Writer::new(Vec::new(), Header::default()).unwrap();
        w.write(&snap(200)).unwrap();
        assert!(w.write(&snap(200)).is_err());
        let mut crossed = snap(300);
        crossed.asks[0].price = crossed.bids[0].price;
        assert!(w.write(&crossed).is_err());
        assert_eq!(w.count(), 1);
        let bad = Header { lot_size: (1, 10_000), ..Header::default() };
        assert!(Writer::new(Vec::new(), bad).is_err());
    }

    #[test]
    fn committed_sample_is_valid() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/sample/btc_2026-09-28.jsonl");
        let mut reader = Reader::open(path).unwrap();
        assert_eq!(reader.header(), &Header::default());
        let (mut count, mut s) = (0, Snapshot::default());
        while reader.next_into(&mut s).unwrap() {
            count += 1;
        }
        assert!(count > 1_000, "only {count} snapshots");
    }
}
