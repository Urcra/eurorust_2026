# Results

Submissions closed on October 16, 2026, at 12:00. Thank you to everyone who took part!

## Ranking

Score: the final balance of one run over the evaluation data, in USD (RULES.md §8). Every
strategy started with 10,000 USD.

| Rank | Player | Strategy | Score (USD) |
|---|---|---|---|
| 1 | Urcra | Bold Ramp | 26966.9174106 |
| 2 | bit-aloo | Aabra ka daabra | 9699.3033799 |
| 3 | thaod | Toll Road | 9447.0146921 |
| 4 | Drizzt | Streams of Silver | 5639.2260777 |
| 5 | trim-the-main | Average | 2792.1531862 |
| 6 | be4st | Through-fill ladder | 2627.8478970 |
| 7 | the_fast_guy | Fast arb strategy | -23435.5690496 |

All 7 submissions followed the rules and traded more than the minimum volume.

## Evaluation data

BTC order book snapshots from 2026-09-30 00:00:00.000 to 2026-10-08 23:59:59.900 UTC: 9 days,
7,775,661 snapshots, in the same format as the sample.

- Download: [btc_2026-09-30_2026-10-08.jsonl.zst](https://github.com/raven-trading/eurorust_2026/releases/download/results-2026-10-16/btc_2026-09-30_2026-10-08.jsonl.zst) (237 MB, 11.2 GB unpacked)
- SHA-256 of the unpacked file: `d32fb0a2fd5195707d488eaf937d5601a32fdaa68107a806655082e22a4996c6`
- Split: one run over the whole file. The minimum volume was 90.0 BTC (10 BTC × 7,775,661 / 864,000).
- Dates: during the evaluation, timestamps were moved back by 93 days. Whole days keep every time
  of day and every elapsed time, so a strategy that does not look at dates scores exactly the same
  on the file as published.
- Simulator: the one in this repository, unchanged since it was published.

## Reproduce your score

Put your `strategy.rs` in `strategy/src/`, then:

```sh
zstd -d btc_2026-09-30_2026-10-08.jsonl.zst
cargo run --release -- --data btc_2026-09-30_2026-10-08.jsonl
```

The balance it prints is your score.
