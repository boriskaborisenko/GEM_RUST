# GEM_RUST — Strategy J (Endgame)

Paper-trading бот для Polymarket crypto UP/DOWN окон (BTC/ETH, 5m).  
Активная стратегия: **`j_endgame`** — одна нога на победителя, redeem @ $1, без продаж.

Цель окна: **+$1 redeem PnL** при минимальном риске. Не «угадать сторону на старте», а **зайти поздно, когда рынок и spot уже показывают победителя**, и дожать exposure динамически.

---

## Идея (магия)

Polymarket 5m окно — это гонка к PTB (price to beat). В последние 2 минуты:

1. **Spot vs PTB** говорит, кто ITM (in the money).
2. **Стакан** (mid-cross lead) показывает, куда тянет flow.
3. **Tape** (покупки на Polymarket) и **CEX micro** подтверждают направление.
4. **gap_z** нормализует «насколько spot далеко от PTB» относительно оставшегося времени и ATR.

J не ставит фиксированное число BUY. Она считает **composite confidence** `C ∈ [0,1]` и из него — **target exposure** (сколько USD хотим на победителе). Каждый SpotTick покупает только **дельту** до target. Отсюда emergent N buys: иногда 3, иногда 12 — зависит от сигнала, не от расписания.

**Прибыль:** купить winner дёшево (88–99¢), держать до redeem $1.  
**Риск:** wrong side near PTB + late reversal = почти полный spent.

---

## Таймлайн окна (5m)

```
0–8%     WARMUP      — mid-cross tracker вооружается, BUY нет
8–50%    MID         — ждём, BUY нет
50–120s  ACCUMULATE  — composite endgame: probe → ramp clips на winner
≤25s     LATE        — (legacy tier; composite доминирует)
≤20s     RESCUE      — profit-gap sizing + flip-hedge приоритет
≤5s      FINAL SEAL  — последние секунды
```

Insurance (ранний $1 на андердога) **выключен** (`insuranceEnabled: false`).

---

## Три движка (каждый SpotTick)

Планировщик: `src/j_controller.rs` → `plan_j_window()`.

| Приоритет | Движок | Когда | Что делает |
|-----------|--------|-------|------------|
| 1 | **Flip hedge** | Есть primary exposure, thesis сломалась | Покупает **противоположную** сторону до `flipTierUsd` |
| 2 | **Composite** | Confidence ≥ порога | Target-exposure на **текущего winner** |
| — | Insurance | `insuranceEnabled` | Сейчас off |

Flip hedge проверяется **до** composite. Если spot пересёк PTB против нашей стороны — hedge может выстрелить раньше, чем мы нарастим loser.

---

## Composite confidence

Функция: `endgame_confidence()` в `j_controller.rs`.

Взвешенная смесь (defaults):

| Сигнал | Вес | Смысл |
|--------|-----|-------|
| **gap_z** | 55% | `(spot − PTB) / expected_move(ATR, secs_left)` |
| **book** | 20% | mid-cross lead на winner, штраф за chop |
| **momentum** | 10% | smoothed spot velocity toward winner |
| **flow** | 15% | tape imbalance + CEX buy/sell imbalance |

**Hard veto (C = 0):**

- `|gap_z| < finalSealMinGapZ` (~0.8) — coin flip, не торгуем
- book **уверенно** ведёт **против** winner (`bookContradictGap`)

**Boost:** сильный gap_z поднимает C даже если book/flow отстают — чтобы покупать @88¢ до репрайса книги к 99¢.

---

## Target exposure & sizing

Функция: `plan_endgame_composite()`.

```
enter     = effective_conf_enter(ask, gap_z)   // ниже при cheap ask / safe gap
eff       = ramp(confidence, enter, 1.0)
conf_target = eff × maxRescueUsd                 // до $75
profit_target = USD чтобы redeem PnL ≥ targetProfitUsd
                (только если уже есть exposure — не на пустом окне!)
target    = min(max(conf_target, profit_target), maxRescueUsd)
increment = target − rescue_spent_usd
clip      = increment capped by effective_max_clip
```

### Ramp clip (не $35 с первого тика)

| Этап | Правило |
|------|---------|
| **Первый BUY** | max `firstClipUsd` ($8) |
| **Follow-up** | ramp по gap_z + % окна + cheap ask |
| **Потолок** | `maxClipUsd` ($35) |
| **Anti-spam** | `minIncrementUsd` ($5), `minBuyIntervalMs` (3000 ms) |

### Дорогой ask @ слабом gap

Свежий вход **запрещён**, если:

- ask > `expensiveAskThreshold` (0.94) **и**
- `|gap_z| < expensiveMinGapZ` (1.35)

Защита от coin-flip @ 95–99¢.

---

## Flip hedge

Функция: `flip_hedge_triggered()` в `strategy_j.rs`.

Arms когда:

- `has_primary_exposure()` — есть `primary_side` **и** deployed USD (composite → `rescue_spent_usd`, не только cheap/late clips)
- Spot **или** mid lead **против** нашей стороны
- Достаточно evidence: significant crosses / cross count / `|gap_z| ≥ flipMinGapZ`

Покупает opposite side taker до **`flipTierUsd`** ($12), max ask **`flipMaxAsk`** (0.99).

> **Важно:** flip hedge — страховка, не полный offset. При $72 на DOWN и $12 hedge UP потеря всё равно большая, но без hedge — total loss.

---

## Данные, которые ест J

| Источник | Для чего |
|----------|----------|
| Polymarket CLOB WS/REST | UP/DOWN bid/ask, book depth, paper fills |
| Chainlink spot WS | spot vs PTB, gap_z, winner |
| Bybit/Binance | ATR, CEX micro imbalance |
| Trade tape tracker | $ BUY flow на winner за `tapeWindowMs` |
| Mid-cross tracker | lead side, chop (significant crosses) |

Stale PM data → no trade. BUY intent ≠ fill: depth, budget, gates могут заблокировать.

---

## Конфиг (`config.json` → `jEndgame`)

Ключевые поля (текущие defaults):

```json
{
  "strategy": "j_endgame",
  "jEndgame": {
    "endgameSecs": 120,
    "cheapMinElapsedPct": 50.0,
    "targetProfitUsd": 1.0,
    "maxRescueUsd": 75.0,
    "maxUsdPerWindow": 80.0,
    "firstClipUsd": 8.0,
    "maxClipUsd": 35.0,
    "minIncrementUsd": 5.0,
    "minBuyIntervalMs": 3000,
    "expensiveAskThreshold": 0.94,
    "expensiveMinGapZ": 1.35,
    "confEnter": 0.58,
    "fullSizeGapZ": 1.8,
    "finalSealMinGapZ": 0.8,
    "flipHedgeEnabled": true,
    "flipTierUsd": 12.0,
    "insuranceEnabled": false,
    "takerMode": true,
    "takerMaxAsk": 0.99,
    "maxSigCrossesDirectional": 3,
    "minPtbDistPct": 0.05
  }
}
```

Полный список полей — `src/config.rs` (`JEndgameConfig`).

---

## Запуск

```bash
cd GEM_RUST
cargo build --release
cargo run --release -- BTC 5m
cargo run --release -- ETH 5m
```

`config.json` должен содержать `"strategy": "j_endgame"`.

Session budget: `session.maxWindowBudget` × clamp — сейчас до **$80/окно** при bank $500.

---

## Логи

Каждый run:

```text
logs/runs/<YYYYMMDD_HHMMSS>_<asset>_<interval>_j_endgame/
```

| Файл | Содержимое |
|------|------------|
| `window_summary.csv` | PnL, winner, PTB, close spot, entry_side, mid-cross counts |
| `strategy_signals.csv` | Каждый BUY signal: reason, gap_z, phase, tape, executed |
| `trade_events.csv` | Исполненные BUY / EXPIRED / REDEEM |
| `mid_cross_events.csv` | Переключения book lead |
| `lifecycle_events.csv` | promote/skip, WS events |

### Reason string (расшифровка)

Пример:

```text
j_final_seal_taker_down_fill_0.89_ask_0.90_gap_z_-1.71_phase_accumulate_pnl_proj_+1.25_tape_$466/39_xc0
```

| Часть | Значение |
|-------|----------|
| `j_final_seal` | tier = composite endgame |
| `taker` | покупка @ ask |
| `down` | сторона |
| `gap_z_-1.71` | spot ниже PTB, ~1.7 expected moves |
| `phase_accumulate` | фаза окна |
| `pnl_proj_+1.25` | projected redeem PnL если DOWN wins |
| `tape_$466/39` | $466 buy flow / 39 prints на winner за 5s |
| `xc0` | mid-cross count в момент сигнала |

Flip hedge: `j_flip_hedge_taker_...`

---

## Как читать run (не обманываться winrate)

**Не смотреть только winrate.** J может быть 90%+ wins и одним loss съесть неделю.

| Метрика | Здорово | Тревога |
|---------|---------|---------|
| avg PnL / window | ~$1–4 | << $0 |
| spent / window | стабильный | $70+ при target $1 |
| first clip | ~$8 | $35 @ 0.98 |
| loss window | редкий, малый | full spent, wrong side |
| `j_flip_hedge_*` в loss | был hedge | 0 hedge при reversal |
| sig mid-crosses at entry | 0–2 | 10+ только в последние 5s |

Типичный **win**: spent ~$68–74, PnL +$1–6.  
Типичный **loss**: spent ~$72, PnL −$72 — wrong side, late spot flip.

---

## Карта кода (J)

```text
src/
├── main.rs                 # runtime loop, dashboard, logging
├── j_controller.rs         # phases, confidence, composite planner, flip plan
├── strategy/strategy_j.rs  # TradeStrategy impl, flip_hedge_triggered, fills
├── config.rs               # JEndgameConfig from config.json
├── mid_cross_tracker.rs    # book lead / chop
├── trade_tape.rs           # Polymarket tape window
├── cex_micro.rs            # CEX imbalance
├── orderbook.rs            # paper taker fill simulation
├── j_fees.rs               # crypto fee model
└── trader.rs               # portfolio, redeem, CSV logs
```

---

## Known sharp edges

- **~$70 deploy за ~$1 target** — structural: один reversal = большой loss. Tuning `maxRescueUsd` / ramp — осознанный trade-off.
- **Cheap ask trap** — DOWN @86¢ выглядит как value, пока spot не перескочит PTB за 30s.
- **Chop filter слепой до конца** — `maxSigCrossesDirectional` считает crosses **на момент входа**; panic chop в последние 5s не блокирует уже deployed position.
- **Flip hedge cap $12** — не нейтрализует $72 exposure.
- BUY amount = **USD**, не shares. Min notional ~$1.
- `cargo test` — 70 unit tests на planner/sizing/flip; live paper ≠ live CLOB.

---

## Verify

```bash
cargo fmt
cargo check
cargo test
```

Runtime paper — оператор запускает вручную, смотрит `logs/runs/`.
