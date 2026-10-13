# Rust BTC Trading Challenge — Rules

Write a trading strategy in Rust. We replay real BTC order book data through a simulator and your strategy trades against it.

**The winner is the strategy with the biggest balance at the end.** Section 8 explains exactly how that is counted.

## 1. The task

You implement one Rust file, `strategy.rs`, containing a struct named `MyStrategy` that implements `Default` and the `Strategy` trait from the starter kit. Its `info()` gives your nickname, your email and your strategy's name. The simulator calls your strategy on every order book snapshot, and your strategy answers with the list of orders it wants.

The starter kit contains the simulator (exactly the one used for final scoring), a runner that prints your final balance, and sample data. Start with `cargo test`: it checks your strategy against the rules and fails until you implement it. `cargo run --release` prints your balance on the sample.

## 2. Market data

Each snapshot contains a timestamp and up to 50 price levels on each side of the BTC order book (price and quantity per level, best price first). Snapshots are 100 ms apart; a few are missing where the exchange feed stopped. The snapshot contains only market data; your own orders are never shown in it.

Prices are integers in ticks (1 tick = 0.01 USD). Quantities are integers in lots (1 lot = 0.00001 BTC). Cash is an integer in units of 1e-7 USD, so price × quantity is exact. All accounting is integer arithmetic, so results are exact and reproducible.

The sample data in the starter kit is **not** used for final scoring. Final scoring uses different, larger hidden data (see section 11).

## 3. Your account

| Parameter | Value |
|---|---|
| Starting cash | 10,000 USD |
| Starting position | 0 BTC |
| Position limit | ±10 BTC (±1,000,000 lots) |
| Maximum orders per response | 20 |
| Minimum trading volume | 10 BTC per day of data |
| Shorting | Allowed, within the position limit |

There is no margin or cash check; only the position limit applies. Cash may go negative while you hold a long position.

**Minimum trading volume.** Your strategy must trade at least 10 BTC per day of data: everything it buys plus everything it sells, not counting the final liquidation (section 7). A strategy that does not trade cannot win. The minimum is pro rata to the length of a run: 10 BTC × snapshots / 864,000, since a day has 864,000 snapshots. The 48-minute sample needs about 0.33 BTC, and 4 days of data need 40 BTC. The runner prints your volume and the minimum. A submission that trades less than the minimum in any evaluation run (section 8) is disqualified.

## 4. Orders

Your strategy has two methods. `info()` says who you are (section 10). `call()` receives the snapshot's timestamp (milliseconds since the Unix epoch, UTC; in the evaluation the dates are shifted, section 11), the order book and your account (position, cash and resting orders), and returns a list of orders:

- **Limit order:** side, price, quantity.
- **Market order:** side and quantity. Executes against the book; any unfilled remainder is dropped.

**The list is the complete set of limit orders you want resting.** There are no order ids and no cancel command:

- A limit order at a side and price where you already have a resting order keeps that order and sets its remaining quantity to the new quantity.
- A limit order at a new side and price is placed. The part that can execute immediately does so as a taker (section 5); the rest rests.
- Every resting order you did not return is cancelled. Returning an empty list cancels all your orders.
- Several limit orders with the same side and price are combined into one.
- Market orders execute once and never rest.

Cancelling and re-placing costs nothing: resting orders have no queue position (section 5).

An order is **rejected** (it is simply not placed; the runner reports rejections by reason) if its quantity is not positive, a limit price is not positive, it is beyond the 20th entry of the list, or it could break the position limit. A rejected limit order does not keep an existing order at that price; that order is cancelled.

The position-limit check is worst-case and runs over your list in order: an order is rejected if `current position + total buy quantity of the accepted orders (resting and market) + its quantity > limit`, and likewise for sells. This way, no combination of fills can take you over the limit.

## 5. How orders are executed

This is the most important section. Read it carefully.

**Latency.** The list you return for snapshot `t` takes effect at snapshot `t+1`. You can never trade on the prices you just looked at.

**Your orders don't move the market.** The data is historical, so the book does not react to your trades. To keep this realistic, liquidity you consume at a snapshot is gone for the rest of that snapshot. You cannot buy the same 2 BTC at the best ask twice. The book is fully refreshed at the next snapshot.

**Order of processing at each snapshot:**

1. The new snapshot is loaded.
2. Your list from the previous step is checked (section 4). Resting orders you did not return are cancelled; resting orders you did return get their new quantity.
3. Resting orders kept from earlier steps are checked for passive fills, oldest first.
4. New orders from your list (market orders and limit orders at new prices) are processed in list order.
5. Your strategy is called with the new snapshot and your updated account.

**Market orders and marketable limit orders** take liquidity level by level, best price first, paying each level's price. A limit buy at price P takes ask levels with price ≤ P; a limit sell at P takes bid levels with price ≥ P. These fills pay the taker fee.

**Resting limit orders** fill only when the market trades *through* your price. A resting buy at P fills when there are ask levels with price **strictly below** P; it fills up to the total remaining quantity at those levels, at your price P. A resting sell at P fills against bid levels strictly above P, at price P. Merely touching your price is not enough, because in reality you would be at the back of the queue. These fills pay the maker fee.

Your orders never trade with each other.

## 6. Fees

| | Fee |
|---|---|
| Taker (market orders, marketable part of limit orders, final liquidation) | 0.01% of notional (1 bps) |
| Maker (resting limit order fills) | **−0.001%** of notional (−0.1 bps): a rebate paid to you |

Fees and rebates are rounded against you to 1e-7 USD: fees up, rebates down.

## 7. End of the run

After the last snapshot, all open orders are cancelled and your position is closed with a market order against the last snapshot, paying the taker fee. If the visible book is too thin to close the position, the remainder is closed at the worst visible price minus 1% (for sales) or plus 1% (for purchases). The orders your strategy returns for the last snapshot are ignored.

## 8. Winning

**The winner is the strategy with the biggest balance at the end.**

- We find the winner on hidden data covering at least 4 days of BTC order book history (section 11).
- A run starts with a fresh `MyStrategy::default()`, a balance of 10,000 USD and no BTC. At the end of the run your orders are cancelled and your position is closed (section 7). What you have then, in USD, is your **final balance**. Nothing else counts: no BTC is left over and no orders stay open.
- We may replay the data as one run or split it into several. Your **score** is your final balance, or the sum of your final balances if there are several runs. Every submission is run on the same data, split the same way. **The biggest score wins.**
- Only submissions that follow every rule are ranked. Missing the minimum trading volume (section 3) or breaking a technical constraint (section 9) disqualifies the submission, whatever its balance.
- On an exact tie, the earlier submission wins.

A balance can go below zero. There is no cash check (section 3), so a run that loses more than 10,000 USD simply ends with a negative final balance, and it counts as such in your score.

Example: you finish a run with 10,250.00 USD. If that is the only run, your score is 10,250.00 USD. If there are three runs and you finish the other two with 9,980.50 and 10,120.00 USD, your score is 30,350.50 USD.

## 9. Technical constraints

- One file, `strategy.rs`, maximum 200 KB.
- Rust stable 1.96.0 (pinned in the starter kit), standard library only, no external crates.
- Not allowed: `unsafe`, threads, file system, network, processes, environment variables, reading the clock, `include!`-style macros, inline assembly, FFI. Pseudo-random numbers from your own seeded generator are fine.
- Your strategy must be deterministic: the same input must produce the same output. Beware: `HashMap` and `HashSet` iterate in a random order; use `BTreeMap` or `BTreeSet`.
- Memory limit: 1 GiB.
- Speed limit: on average at most 100 µs per snapshot on the evaluation machine. The runner prints your average time per snapshot, so you can check this yourself.
- A submission that fails to build, panics, exceeds a limit, or misses the minimum trading volume (section 3) during final evaluation is disqualified.

Submissions are not checked when they arrive, so check yours before you send it: `cargo test` checks it against the rules, and `cargo run --release` prints its balance, volume and time per call on the sample.

## 10. Submission

- Submit your `strategy.rs` through [the submission form](https://docs.google.com/forms/d/e/1FAIpQLScRaEQ4Gh9ORZOS3dJiwltF3MDNxnjOSj69e_qIRUx9LS8aog/viewform). Its `info()` identifies you: the ranking shows your nickname and your strategy's name, and your email is only used to contact you and is never published.
- Upload the `strategy.rs` file itself, not a zip or the whole folder.
- Deadline: October 16, 2026, 12:00 local time.
- One entry per person, identified by the email in `info()`. You may resubmit until the deadline; your last submission counts.

## 11. Evaluation

After the deadline, all submissions are run on hidden data covering at least 4 days of BTC order book history, not including the sample. Every submission gets the same data, replayed the same way (section 8). Its timestamps are moved back by a secret whole number of days, the same for every submission: times of day are real, dates are not. Your strategy is not told how long a run is or when it ends.

The results are announced on October 16, 2026, at 17:30 local time, at our booth. We will publish the full ranking and, after the announcement, the simulator version, the evaluation data, how it was split into runs and how far its dates were moved, so anyone can reproduce their score.

## 12. Fair play

- Deliberately exploiting a bug in the simulator is not allowed. If a bug is found that affects results, we fix it, announce the fix, and rerun all submissions.
- Code that tries to escape the sandbox or interfere with evaluation is disqualified.
- Your strategy must not contain historical market data or try to find out which days it is trading on. We read the code of the top submissions before the results are announced.
- The organizers' decisions are final.

Questions: ask in [our Discord channel](https://discord.com/channels/1422894279683997836/1559615257414992033).
