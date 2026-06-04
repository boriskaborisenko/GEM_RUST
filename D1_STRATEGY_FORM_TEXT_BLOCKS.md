# D1 Strategy Form Text Blocks

Use this file as copy/paste material for the strategy-idea form.

Goal of the form: make the external service think specifically about **GEM_RUST Dynamic Grid D1**, not about a generic trading bot, not about marketing, and not about unrelated product ideas.

## Version A: Direct Strategy Review

### 1. Company & Product / Business Context

We are building `GEM_RUST`, a Rust-based paper-trading and strategy research engine for short Polymarket crypto UP/DOWN markets, focused mainly on BTC 15-minute windows and secondarily on BTC 5-minute windows for faster data collection. The current active strategy is `Dynamic Grid D1`: a one-leg first-entry strategy that buys only one side near parity before the window opens, then manages the position with time/ATR/PTB-aware hedging, runner preservation, late tail liquidation, and optional LLM directional forecasts. The goal is not a pretty winrate; the goal is positive expectancy per window, controlled drawdown, and scalable trade sizing.

### 2. Problem

The current D1 strategy is unstable. It can produce good windows, but recent logs show poor overall results, weak entry-side accuracy, and inconsistent LLM value across 5m vs 15m. The hardest problem is deciding which side to buy first and when to hedge without destroying the upside of a correctly guessed runner. If D1 hedges too early, it kills the `x2` payout potential. If it hedges too late or chooses the wrong first side, the position can expire at zero. The strategy needs a coherent decision framework for first-side selection, hedge timing, runner-to-redeem preservation, and tail liquidation.

### 3. Hypothesis

D1 can become profitable if it treats the first buy as a directional micro-prior, not as a neutral pair trade. The strategy should use a richer context window before buying: current spot velocity/acceleration, ATR regime, pre-start bid/ask balance, recent 10-window performance, recent LLM accuracy, entry-side accuracy, winner bias, runner redeem rate, hedge cost vs rescue value, tail liquidation losses, and slippage sensitivity. A qualified LLM forecast may improve side selection, but only if it receives useful recent-window context and is treated as one factor, not an unconditional override.

### 4. Metrics

1. Avg PnL per closed window.
2. Median PnL per closed window.
3. Max drawdown over the run.
4. Entry-side accuracy: how often the first bought side becomes the winner.
5. LLM directional accuracy, split by 5m and 15m.
6. PnL by entry source: `llm_prior`, `momentum_no_ptb`, `directional`, `bootstrap`.
7. Runner held-to-redeem rate: how often the original correct side survives to 1.00 redeem.
8. Hedge cost vs hedge rescue value: whether opposite-side buys reduce loss or just consume profit.
9. Tail liquidation loss/value: whether weak tail selling preserves meaningful capital.
10. Slippage sensitivity at `±0.01` and `±0.02`.
11. 15m entry count and 15m expectancy, because 15m is the priority interval.

### 5. Feature Ideas

#### Idea 1: Rolling 10-Window LLM Context

Before each future-window forecast, pass the LLM compact data from the last 10 closed windows: avg/median PnL, drawdown, entry-side accuracy, LLM accuracy, winner counts, runner redeem rate, hedge cost/rescue value, tail liquidation value, slippage sensitivity, and compact per-window rows. Ask the LLM to use this as regime memory, not as a blind rule.

#### Idea 2: LLM as Side-Selection Prior, Not Trade Permission

Use LLM only to influence which side to buy first (`UP` or `DOWN`) when ask is near parity. Do not let LLM decide whether the trade is allowed. Trade permission should remain controlled by price corridor, spread, ATR, current momentum, and D1 risk constraints.

#### Idea 3: Confidence-Weighted LLM Gate by Interval

Because recent logs show different behavior on 5m and 15m, use different confidence thresholds. For example, require lower LLM confidence on 15m only if recent 15m LLM accuracy is good, but raise or disable the LLM gate when recent 15m LLM accuracy drops below 50%.

#### Idea 4: Entry Source Attribution

Track PnL separately for `llm_prior`, `momentum_no_ptb`, `directional`, and `bootstrap`. If one source has negative expectancy over enough windows, reduce its budget multiplier or force it into scout-only mode.

#### Idea 5: Hedge Efficiency Score

Measure every opposite-side hedge by cost and final rescue value. If hedge windows still lose nearly the same amount, the hedge is too late or too expensive. If hedge windows reduce loss materially, keep it. D1 should optimize hedge timing from empirical rescue value, not from intuition.

#### Idea 6: Runner Preservation Score

Track how often the initially correct side was sold too early versus held to redeem. If the first side is correct and close-window probability remains high, D1 should prefer redeem over selling at mediocre bids like 0.65-0.75.

#### Idea 7: Tail Liquidation Audit

Separate weak-tail liquidation from normal profit taking. Tail sells should answer one question: did selling a weak OTM tail preserve meaningful cash, or did it sell a side that later recovered? Use this to tune final-phase salvage probability thresholds.

#### Idea 8: Slippage Robustness Test

Evaluate every run under simulated adverse slippage of `0.01` and `0.02` on both buys and sells. A strategy that is only profitable with perfect paper fills is not robust enough to scale.

## Version B: More Aggressive Agent-Debate Prompt

Use this if the form rewards direct, debate-provoking hypotheses.

### 1. Company & Product / Business Context

We are developing `GEM_RUST Dynamic Grid D1`, a Rust paper-trading strategy for Polymarket BTC UP/DOWN windows. D1 is not a classical two-sided arbitrage bot. It intentionally starts with one side near parity, tries to maximize the upside of a correct directional guess, and only hedges when the first leg becomes statistically threatened. The strategy uses ATR, PTB distance, time-to-expiry, spot velocity/acceleration, bid/ask prices, and optional Gemini/Vertex LLM forecasts.

### 2. Problem

D1 currently suffers from a decision-quality problem, not just a parameter problem. The bot sometimes buys the wrong first side, sometimes hedges in ways that reduce upside, and sometimes sells or preserves tails incorrectly. Recent tests show that LLM can be helpful in one interval and harmful in another, meaning the model needs better context and the strategy needs interval-aware trust calibration. We need the service to challenge the structure of D1, not merely suggest new thresholds.

### 3. Hypothesis

The best version of D1 is a compact adaptive controller: first choose side using current micro-momentum plus rolling outcome context; then delay hedging while the runner remains statistically alive; then buy protection only when the first side's win probability falls below a time/ATR/PTB-adjusted threshold; then preserve clear winners for redeem and liquidate only tails that are mathematically overpaid by the market bid.

### 4. Metrics

Evaluate whether D1 improves using:

1. Avg and median PnL per window.
2. Max drawdown.
3. Entry-side accuracy.
4. LLM accuracy by interval and by confidence bucket.
5. PnL by entry source.
6. Runner held-to-redeem rate.
7. Hedge cost vs hedge rescue value.
8. Tail liquidation loss/value.
9. Slippage sensitivity at `0.01` and `0.02`.
10. 15m expectancy and entry count as the primary target.

### 5. Feature Ideas

#### Idea 1: Recent-Window Regime Memory

Feed the LLM and/or strategy controller a rolling last-10-window summary so it knows whether the current run is trending, mean-reverting, noisy, LLM-friendly, or LLM-hostile.

#### Idea 2: Trust-Weighted LLM Prior

Do not blindly follow LLM. Convert LLM confidence into a small probability prior, then weight it by recent LLM accuracy and interval. If recent 15m LLM accuracy is poor, suppress LLM influence even if current confidence is high.

#### Idea 3: First-Leg Survival Model

Build a model that estimates whether the first leg should be left alone, hedged, or abandoned based on PTB percent distance, ATR-scaled expected move, seconds remaining, spot velocity, and bid/ask value.

#### Idea 4: No Early Hedge Unless Threat Is Real

Correct D1 runners make money by redeeming at `1.00`. Hedges should not be bought simply because the opposite side is affordable. Hedge only when wrong-way probability and time decay justify the cost.

#### Idea 5: Probability-Based Tail Selling

Weak tails should be sold only when bid overpays the modeled probability. If a weak side has realistic reversal probability near PTB, selling it for a low bid is a structural leak.

#### Idea 6: Entry Source Budget Tuning

Use empirical PnL by source to tune budget multiplier. `llm_prior`, `momentum_no_ptb`, and `directional` should not all receive the same budget if their expectancy differs.

#### Idea 7: Interval-Specific Rules

5m and 15m should not use identical trust logic. 15m has more time for PTB crossing and runner recovery, while 5m has faster decay and less time to repair mistakes.

## Version C: Compact Form Fill

Use this if the form fields have small text limits.

### 1. Company & Product / Business Context

We are building `GEM_RUST Dynamic Grid D1`, a Rust research engine for Polymarket BTC UP/DOWN 5m and 15m windows. D1 buys one side near parity before the window opens, then manages hedge timing, runner preservation, and tail liquidation using ATR, PTB distance, time-to-expiry, spot velocity, bid/ask prices, and optional LLM directional forecasts.

### 2. Problem

D1 currently has poor stability: wrong first-side selection, inconsistent LLM usefulness, hedges that may cost more than they rescue, and runner/tail decisions that can destroy expectancy. The strategy needs a better side-selection and post-entry control framework, especially for the priority 15m interval.

### 3. Hypothesis

If D1 gives the LLM and strategy controller useful rolling context from the last 10 windows, then side selection and hedge timing can improve. The key is to use LLM as a weighted directional prior, not as a trade permission system, and to preserve correct runners for redeem while only hedging or liquidating tails when probability and time decay justify it.

### 4. Metrics

Avg PnL/window, median PnL, max drawdown, entry-side accuracy, LLM accuracy by interval, PnL by entry source, runner held-to-redeem rate, hedge cost vs rescue value, tail liquidation loss/value, slippage sensitivity at `±0.01/±0.02`, and 15m expectancy/entry count.

### 5. Your Feature Ideas

- Rolling last-10-window context for LLM forecasts.
- Trust-weighted LLM side prior, calibrated by recent LLM accuracy.
- Entry source attribution and budget multiplier tuning.
- First-leg survival model for hold vs hedge vs abandon.
- Probability-based weak-tail liquidation.
- Hedge efficiency scoring.
- Runner redeem preservation score.
- Interval-specific 5m vs 15m thresholds.

## Extra Instruction to Paste Anywhere If Possible

Please reason concretely about the trading logic of D1. Do not produce generic AI trading advice. Focus on side selection before the window starts, LLM as a directional prior, ATR/PTB/time-decay interaction, runner preservation to 1.00 redeem, hedge cost vs rescue value, and weak-tail liquidation. Assume this is paper trading and the goal is robust positive expectancy, not just high winrate.
