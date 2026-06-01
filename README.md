# GEM_RUST

`GEM_RUST` is an advanced, asynchronous, event-driven trading engine built in **Rust** designed to harvest volatility on **Polymarket binary options contracts** (UP/DOWN short-term options, typically on crypto prices like BTC, ETH, and SOL on 5-minute or 15-minute intervals).

Featuring a reactive architecture powered by **Tokio**, the bot incorporates multi-dimensional risk filters, dynamic capital management, real-time volatility tracking via Bybit WebSockets, and off-line AI auditing.

---

## 🚀 Architectural Map of Modules

The project is decoupled into isolated modules to ensure high throughput, zero-allocation data paths, and safe memory management:

```text
GEM_RUST/src/
├── main.rs              # Orchestrator. Manages AppState, the central mpsc event loop, and the UI.
├── client.rs            # WebSockets: Chainlink (spot prices) & Polymarket CLOB (YES/NO bids/asks).
├── trader.rs            # PaperTrader. Maintains Portfolio, WindowState, and handles CSV trade logging.
├── config.rs            # Configuration parser using Serde for strong-typed config.json loading.
├── volatility.rs        # Bybit WS Client. Real-time kline stream handling and RMA-smoothed ATR(14) calculation.
├── analytics.rs         # Offline AI Auditor. GCP Vertex AI integration for Gemini 2.5 Pro strategy audits.
└── strategy/            # Plugin-driven Strategy Engine (stratBox)
    ├── mod.rs           # Plugin dispatcher. Defines the TradeStrategy trait and StrategyEngine router.
    ├── strategy_a.rs    # Strategy A (Simple Both): Symmetric entry, fixed take-profit exit.
    ├── strategy_b.rs    # Strategy B (Asymmetric Ladder): Asymmetry, ladder exits, Theta Decay shrinkage.
    ├── strategy_c.rs    # Strategy C (Dynamic Break-Even): Direct target exit 1, break-even exit 2.
    └── strategy_d.rs    # Strategy D (Dynamic Grid): 3-tier Grid, Adaptive TVDS Matrix, Volatility-Aware Buy.
```

---

## 📈 Detailed Walkthrough of the 4 Trading Strategies

The strategy engine is completely polymorphic and built upon a unified `TradeStrategy` trait interface:

### Strategy A: "Simple Both" (Baseline Arbitrage)
* **Configuration Name:** `"simple_both"`
* **Entrance Logic:** Purchases UP and DOWN YES contracts exactly 1 minute before the market start time. It triggers only when contract prices are perfectly balanced (e.g., Ask price is between `0.48$` and `0.52$`).
* **Exit Logic:** Monitors orderbook Bid prices on the Polymarket CLOB via WebSockets. Once any contract's Bid goes `>= 0.65$`, the bot executes a market sell on 100% of that contract's shares. Both legs exit independently.

### Strategy B: "Asymmetric Ladder" (Multi-Step Volatility Scaling)
* **Configuration Name:** `"asymmetric_ladder"`
* **Entrance Logic:** Enters the market asymmetrically, leaning heavier on the prevailing trend direction (e.g., allocating more budget to the stronger contract).
* **Exit Logic:** Liquidates positions in multiple steps (e.g., selling 50% on reaching Target 1, and the remaining 50% on reaching Target 2).
* **Theta Decay (Time Decay) Filter:** If `decayEnabled: true`, as the option approaches expiration, the target thresholds contract according to the remaining time:
  * `Time elapsed < 50.0%` (0–7.5 mins): exit targets remain at `1.0x` (e.g., `0.62/0.72` and `0.70/0.85`).
  * `50.0% <= Time elapsed < 80.0%` (7.5–12 mins): targets are multiplied by `0.90x` (shrunk by 10%).
  * `Time elapsed >= 80.0%` (12+ mins): targets are multiplied by `0.80x` (shrunk by 20% to exit quickly before time decay destroys premium).

### Strategy C: "Dynamic Break-Even" (Risk-Neutral Leg Recouping)
* **Configuration Name:** `"dynamic_breakeven"`
* **Entrance Logic:** Asymmetric buy-in (e.g., 60% budget to UP, 40% to DOWN) on pre-start.
* **Exit 1 (First Leg):** The moment the first leg reaches the target (e.g., `>= 0.65$`), the bot liquidates 100% of its shares on that side, capturing profit and recouping a massive portion of the initial transaction cost ($Cash\_Returned$).
* **Exit 2 (Second Leg):** The second leg waits for a spot price crossover. Upon crossover, the bot reads the current Bid price and compares it to a mathematically computed **minimum safe break-even price** ($Min\_Safe\_Price$):
  $$Min\_Safe\_Price = \frac{Spent - Cash\_Returned}{Remaining\_Shares} + slippageBuffer$$
  If $Bid \ge Min\_Safe\_Price$, the bot instantly dumps the remaining shares, securing a risk-neutral win or slight profit for the overall round.

### Strategy D: "Dynamic Grid" (Adaptive TVDS Matrix & Volatility-Aware Buying)
* **Configuration Name:** `"dynamic_grid"`
* **Entrance Logic:** Adaptive capital budgeting with asymmetric asset allocation.
* **Grid Exits (Strong Leg):** Liquidates the winning contract in 3 tiers: 40% of shares at `0.58$`, 40% at `0.66$`, and the remaining 20% (the "Runner") at `0.75$`.
* **Adaptive Grid Optimization:**
  * If the trend is extremely aggressive (measured by the absolute spot percentage deviation from strike: `pct_abs > 0.20%` and high ATR), the third step's target dynamically shifts up to **`0.92$`** (milking the trend as the contract approaches `0.99$`).
  * If the market remains flat (`pct_abs < 0.08%`), targets automatically compress down (to `0.53/0.59/0.65$`) to dump the winning leg quickly for minor profits.
* **The TVDS (Timing × Volatility × Deviation × Share Weight) Matrix for Weak Leg:**
  * **Early Game (`Time < 30%`):** Time decay is zero. If volatility is high (`current_atr >= 30.0`), the exit target is set high to **`>= 0.65$`** (waiting for a full reversal). If sluggish, it exits at **`>= 0.40$`**.
  * **Mid Game (`30% - 60%`):** If spot is super close to strike (`pct_abs <= 0.05%`), target is **`>= 0.65$`**. If moderate (`pct_abs <= 0.15%`), target is **`>= 0.30$`**. If far and sluggish, target is **`>= 0.17$`** to minimize premium loss.
  * **Late Game (`60% - 80%`):** If close to strike, target is **`>= 0.45$`**. If moderate, target is **`>= 0.20$`**. If far, target is **`>= 0.12$`**.
  * **End Game (`80% - 90%`):** If super close to strike, target is **`>= 0.30$`**. Otherwise, emergency dumps at **`>= 0.10$`** (saving remaining premium).
  * **Expiration (`Time >= 90%`):** Unconditional liquidation if any Bid **`>= 0.08$`** is available.

---

## 🛠️ Advanced Capital & Risk Management Models

`GEM_RUST` implements strict mathematical risk modeling to ensure portfolio longevity:

### 1. Dynamic Budget Sizing
Rather than spending static cash per contract pairs, the bot dynamically scales its window budget as a percentage of the total portfolio **Equity**:
$$Budget_{raw} = Equity \times \left(\frac{windowBudgetPct}{100}\right)$$
This budget is bounded strictly between configured limits:
$$Budget = \max(minWindowBudget, \min(maxWindowBudget, Budget_{raw}))$$
If available cash is lower than the calculated budget but higher than `minWindowBudget`, the budget is clipped to available cash. If cash is below `minWindowBudget`, the entry is skipped entirely.

### 2. Strategy-Owned Budget Split
The base window budget still comes from Equity, but the final split is now supplied by the active strategy via `EntrySignal`.
For Strategy D, the split is ATR-regime aware: low ATR scouts close to neutral, normal ATR uses only a mild cheap-side tilt, and high ATR moves back toward 50/50. This avoids a misleading global `cheaperSideRatio` knob.

### 3. Volatility-Aware Dynamic BUY Filter
To prevent buying cheap contracts of the losing side (Dynamic BUY) during strong, irreversible trends, the bot calculates the maximum allowed spot price percentage deviation ($pct\_abs$) from strike based on the current **RMA-smoothed ATR**:
* **High Volatility (`current_atr >= 30.0`):** Max deviation allowed is **`pct_abs <= 0.12%`** (reversals are possible).
* **Low Volatility (`current_atr < 15.0`):** Max deviation allowed is **`pct_abs <= 0.03%`** (reversals are impossible).
* **Normal Volatility:** Max deviation allowed is **`pct_abs <= 0.08%`**.

If the spot has drifted further than these limits (e.g., $80+ spot difference on BTC during normal/low volatility), the bot **completely blocks** the Dynamic BUY, protecting your capital from expiring at 0.

---

## ⚙️ Configuration Reference (`config.json`)

All runtime options are managed in a single, clean JSON configuration file:

```json
{
  "strategy": "dynamic_grid",
  "minBtcAtr": 0.0,
  "session": {
    "startingBank": 100,
    "minWindowBudget": 30.0,
    "maxWindowBudget": 150.0,
    "windowBudgetPct": 10.0
  },
  "preStartEntry": {
    "enabled": true,
    "minSecondsBeforeStart": 5,
    "maxSecondsBeforeStart": 120,
    "minSideAsk": 0.48,
    "maxSideAsk": 0.52
  },
  "sellStrategy": {
    "exitBid": 0.65
  },
  "asymmetricLadder": {
    "strongSteps": [0.62, 0.72],
    "weakSteps": [0.70, 0.85],
    "decayEnabled": true
  },
  "exitBeforeEndSeconds": 25,
  "forceCloseAtEnd": false
}
```

### Parameter Explanations:
* `strategy`: Name of the active trading plugin (`"dynamic_grid"`, `"asymmetric_ladder"`, `"dynamic_breakeven"`, `"simple_both"`).
* `minBtcAtr`: Minimum ATR threshold required to trade. If the actual ATR is lower, the round is skipped.
* `session.startingBank`: Initial equity of the simulation.
* `session.minWindowBudget`: Minimum allowed budget per window.
* `session.maxWindowBudget`: Maximum allowed budget per window.
* `session.windowBudgetPct`: Percentage of current equity to allocate per trade.
* `preStartEntry.minSideAsk` & `maxSideAsk`: The narrow entry corridor (e.g. `0.48` and `0.52`) required to enter a trade before the market starts.
* `sellStrategy.exitBid`: Default target bid for exit-taking logic.

---

## 🛠️ Build and Execution

Ensure you have Rust and Cargo installed on your system.

### 1. Verification and Compilation
Validate the syntax, dependencies, and build performance:
```bash
# Move to the project root directory
cd GEM_RUST

# Run compilation check
cargo check

# Compile in release mode for production-grade speed
cargo build --release
```

### 2. Execution
Run the bot, specifying the asset (`BTC` / `ETH` / `SOL`) and time interval (`5m` / `15m`):
```bash
# Run for Bitcoin 5m options
cargo run -- BTC 5m

# Run for Ethereum 15m options
cargo run -- ETH 15m

# Run for Solana 5m options
cargo run -- SOL 5m
```

---

## 🧠 Off-line AI Auditing & Strategy Optimization

The system includes a state-of-the-art integration with Google Vertex AI to automate backtest audits:
1. Every completed trade is automatically appended to `logs/trades_history.csv` via the Paper Trader.
2. The `analytics.rs` module authorizes via Google GCP Service Accounts, establishes an OAuth2 connection, and sends the raw trade logs to **Gemini 2.5 Pro**.
3. The model scans for low-performing periods, evaluates Time-Decay and Dynamic Buy performance, and exports optimization reports directly to `logs/gemini_strategy_report.txt` for real-time model calibration.
