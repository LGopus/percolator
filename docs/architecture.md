# Perps DEX — Architecture (v0, draft)

Status: draft for discussion. Nothing here is final.

## 1. Goals and non-goals

**Goals**
- Permissionless multi-market perps DEX on Solana mainnet.
- Built on a soft-fork of `aeyakovenko/percolator` (engine) and `aeyakovenko/percolator-prog` (wrapper), staying close to upstream so we can pull fixes/proofs.
- Manipulation-resistant oracle that we control end-to-end.
- A pluggable matcher layer; ship one matcher at launch, keep room for more.
- Eventual immutability: upgrade authority is burned once the system is hardened.

**Non-goals (v1)**
- Custom rollup / app-chain — we run on Solana mainnet.
- Cross-chain margin or cross-chain oracles.
- Building our own CLOB at launch — start with passive LP, add CLOB later if demand exists.

## 2. Component map

```
┌──────────────────────────── User space ───────────────────────────────┐
│  Frontend (web)                       CLI / SDK (TS)                   │
└──────────────┬─────────────────────────────────┬──────────────────────┘
               │ tx                              │ tx
               ▼                                 ▼
┌──────────────────────────── Solana ────────────────────────────────────┐
│                                                                         │
│  ┌────────────────────┐   CPI    ┌──────────────────────┐              │
│  │  perps-prog        │────────▶│  matcher-prog(s)      │              │
│  │  (fork of          │          │ (fork of              │              │
│  │  percolator-prog)  │          │  percolator-match     │              │
│  │                    │          │  + future CLOB)       │              │
│  │  ┌──────────────┐  │          └──────────────────────┘              │
│  │  │ percolator   │  │                                                 │
│  │  │ RiskEngine   │  │                                                 │
│  │  │ (this fork)  │  │                                                 │
│  │  └──────────────┘  │                                                 │
│  │                    │   read    ┌──────────────────────┐              │
│  │                    │──────────▶│  oracle-prog          │              │
│  │                    │           │  (our aggregator)    │              │
│  └─────────┬──────────┘           └──────────┬───────────┘              │
│            │                                  ▲                          │
│            │                                  │ pulls / pushes           │
│  ┌─────────▼─────────┐               ┌────────┴────────┐                │
│  │  Token vault     │               │  Pyth/Switchboard │               │
│  │  (SPL token PDA) │               │  + custom feeders │               │
│  └──────────────────┘               └───────────────────┘                │
└─────────────────────────────────────────────────────────────────────────┘
                              ▲
                              │
┌─────────────────────────────┴──────────────────────────────────────────┐
│  Off-chain: keeper bot (liquidations, sweeps, fee sync)                │
│             oracle feeder bots (independent operators)                 │
│             indexer / API                                               │
└────────────────────────────────────────────────────────────────────────┘
```

## 3. Trust boundaries

| Layer | Trusts | Distrusts | Why |
|---|---|---|---|
| `RiskEngine` (lib) | Its callers | Nothing — pure | Already formally verified (286 Kani proofs). No I/O. |
| `perps-prog` | Its own PDAs, signer rules | All callers, all CPI returns | Validates accounts, owns the slab, performs token transfers. |
| `matcher-prog` | Its registered LP PDA | Anyone calling without that signer | Pricing is local to the matcher. |
| `oracle-prog` | Quorum of feeder signers | Single feeder | TWAP + outlier rejection bound to staleness. |
| Keeper | Public mempool | Nothing privileged | Anyone can run keepers; only earns crank rewards. |

The engine's existing trust model (lib → prog → matcher) stays. We add `oracle-prog` as a peer service to `matcher-prog`, both called via CPI from `perps-prog`.

## 4. Repository layout

Single workspace, multiple crates, one frontend. Forks pinned by commit, not branch, so upstream churn doesn't break us.

```
perps-dex/
├── crates/
│   ├── percolator/          # this fork (engine)
│   ├── perps-prog/          # fork of percolator-prog
│   ├── matcher-passive/     # fork of percolator-match (launch matcher)
│   ├── matcher-clob/        # future
│   ├── oracle-prog/         # our oracle aggregator
│   └── keeper/              # off-chain bot
├── ts/
│   ├── sdk/                 # fork of percolator-cli
│   └── frontend/
├── docs/
└── tests/                   # cross-program integration tests
```

Upstream sync policy: weekly `git fetch upstream && cherry-pick`. Any local divergence gets a `LOCAL:` prefix in the commit subject so it's easy to audit before burning keys.

## 5. Markets — permissionless multi-market

Each market is one engine slab account, owned by `perps-prog`, parameterized by:

- `quote_mint` — **USDC or wSOL at launch** (each is a separate slab; the engine is single-quote-token per spec)
- `oracle_account` (must be of type `oracle-prog::PriceFeed`)
- `matcher_program` + `matcher_context_account`
- `risk_params` (margin, fees, liquidation, admission band)

**Two quote-token tracks at launch:**
- USDC-quoted markets: USD-priced perps (BTC-PERP, ETH-PERP, etc.). Familiar UX, USD-denominated PnL.
- SOL-quoted markets: native to SOL holders, fees accrue in SOL. Especially useful for SOL-quoted longtail / memecoin perps where USD oracles don't exist but a SOL-pair price does. PnL has SOL/USD FX baked in — that's a feature for SOL-natives and a footgun for everyone else; UI must surface it.

Quote asset is fixed per market at init; cannot be changed.

**Permissionless creation:** anyone can call `init_market(...)` with their own params, paying the rent. To prevent spam/grief markets dragging the namespace:

- Enforce a non-trivial creation deposit (refundable on close).
- Require `oracle_account` to be one of: (a) a feed registered in our `oracle-prog` registry, OR (b) a market in `hyperp` mode (no external oracle — see §7).
- Require `matcher_program` to be in a small allowlist initially. Plan to make this fully open after the burn.

The `cfg_max_accrual_dt_slots`, `cfg_max_abs_funding_e9_per_slot`, `cfg_max_price_move_bps_per_slot` envelopes from the engine spec are **not** wrapper-tunable post-init — they're immutable per market, which is what makes the safety boundary hold. The `init_market` instruction validates that submitted values satisfy the solvency-envelope inequality from spec §8.

## 6. Oracle — design sketch

Goal stated by you: *impossible to manipulate, most efficient option (zero ongoing infra cost)*.

Realistic restatement: *manipulating the price-of-record costs more than any single block's manipulation can pay for, and we run no servers ourselves*.

### 6.1 Cost-driven design choice

We are **not running our own feeders at v1.** Running feeders means VPS/colocation cost, key management, monitoring, on-call. That's the opposite of "most efficient." Instead, `oracle-prog` is a **pure on-chain aggregator** over price feeds that already exist on Solana:

| Source | Cost to us | Trust model |
|---|---|---|
| Pyth Pull | $0 — caller pays the update tx fee | Pyth's publisher set |
| Switchboard On-Demand | $0 — caller pays | Switchboard's oracle queues |
| (later) own feeders | ~$30/mo per node | our keys |

For v1 the only cost is Solana base fees, paid by the trader's tx (Pyth/Switchboard pull updates are bundled into the same tx as the trade). Our infra cost: $0. This is as efficient as it gets without sacrificing independence.

### 6.2 What `oracle-prog` does

Aggregator consumes already-signed feeds and produces a robust median + TWAP:

1. **Inputs each tick (32 slots ≈ 12s):**
   - Pyth aggregate price + confidence
   - Switchboard On-Demand price (where available)
   - Hooks for additional pull-feeds added later
2. **Aggregation:**
   - Drop any input older than `max_staleness_slots`.
   - Drop any input whose confidence > `max_conf_bps`.
   - Take the **median** of survivors (defeats one-sided pushes; degenerate when only 1 input survives — see quorum).
   - Quorum: require ≥2 surviving inputs, else feed goes stale and the market halts new positions (existing positions still serviceable per engine spec).
3. **Smoothing:**
   - EMA-TWAP over the last `W` ticks (e.g., 16 ≈ ~3 min).
   - Publish `spot_median` and `twap`. Engine consumes `twap` for liquidation/admission so single-block manipulation has zero payoff. Closes/withdrawals use `spot_median` gated by `cfg_max_price_move_bps_per_slot` (already in engine).
4. **No feeder registry, no slashing, no stake.** Trust is inherited from Pyth/Switchboard.

### 6.3 What we add later (cost-free upgrades)

- **Add more pull-feeds as they come online** (Pyth Lazer, Chainlink, etc.) by extending `oracle-prog`'s input list. Adds redundancy without infra.
- **Adversarial-mode markets:** for high-value markets where Pyth+Switchboard isn't enough, we can stand up our own 1-2 feeders later. Only do this when revenue justifies the ops cost.

### 6.4 Hyperp markets (no external oracle)

For longtail / memecoin perps with no good oracle, support `oracle_mode = Hyperp`: mark price is a TWAP of internal trade prices, already supported in `percolator-prog`. Funding anchors to internal mark. We gate hyperp markets with a higher creation deposit and a stricter `cfg_max_price_move_bps_per_slot`.

### 6.5 Open questions

- Quorum threshold (2-of-N vs. 3-of-N) — depends on how many independent pull-feeds we onboard. Start with 2-of-2 (Pyth+Switchboard) and tighten as we add sources.
- TWAP window length per market vs. global. Probably global default + per-market override at init.

## 7. Matcher

`percolator-match` (passive ±50 bps off oracle) is the cheapest path to launch and the one the upstream is already wired for. It's also a sane default for tokens with thin books.

**v1 plan:** fork `percolator-match` as `matcher-passive`. Adjust:
- Spread is per-market config, not a constant.
- LP can deposit/withdraw without halting the market.
- Reads price from our `oracle-prog`, not directly from Pyth.

**v2 plan:** add `matcher-clob`. Order book in a separate program; same CPI surface so `perps-prog` doesn't change. The trade path on the engine side (`TradeCpi`) is already designed to be matcher-agnostic.

A market picks its matcher at init and cannot change it (this is a safety property — switching matchers is equivalent to swapping the pricing function under live positions).

## 8. Keeper

Off-chain bot. Stateless modulo recent slots. Responsibilities:

1. Watch oracle feed; for each market, detect liquidatable accounts (margin breached against `twap`).
2. Submit `liquidate(...)` / two-phase `keeper_crank(...)` per spec §9.6 / §9.7.
3. Submit `sync_account_fee_to_slot(...)` periodically.
4. Submit `garbage_collect_dust(...)` and `reclaim_empty_account(...)` after GC budget.

Crank rewards (already in engine via `keeper_priority_credit`) are paid in quote token. Anyone can run a keeper; we run a reference one.

## 9. Burning the upgrade authority — condition-based, not date-based

The engine (`percolator`) has already been audited by toly and ships with 286 passing Kani proofs, so we inherit a strong correctness baseline. The pieces *we* author — `perps-prog` deltas, `oracle-prog`, `matcher-passive` deltas — are what gate the burn.

```
Stage 0 (launch):    multisig 3-of-5 is upgrade authority.
Stage 1 (live ops):  wrap multisig in a 24h timelock once monitoring is solid.
Stage 2 (audited):   7-day timelock once perps-prog and oracle-prog have a clean
                     audit + bounty has run for ≥30 days with no critical findings.
Stage 3 (burn):      upgrade authority → null. Triggered by checklist below
                     being green for 30 consecutive days. No fixed date.
```

**Pre-burn checklist (must all be green for 30 days):**

- [ ] Engine: Kani proofs pass against the deployed binary's exact source. (Mostly inherited from upstream's audit.)
- [ ] Engine deltas: any `LOCAL:` commits diverging from upstream are individually justified and reviewed.
- [ ] `perps-prog`: independent audit completed; all critical/high findings resolved.
- [ ] `oracle-prog`: independent audit completed; all critical/high findings resolved.
- [ ] `matcher-passive`: independent audit completed; all critical/high findings resolved.
- [ ] No outstanding P0 or P1 issues in any deployed crate.
- [ ] Bounty active for ≥30 days with no critical reports.
- [ ] Insurance fund seeded above the v1 target.
- [ ] At least one real stress event survived (volume spike, oracle staleness, matcher outage).
- [ ] At least 2 matcher implementations live (passive + one alternate) so the protocol isn't dependent on a single matcher.

**After burn:**
- `perps-prog` is immutable.
- `oracle-prog` stays upgradable for one more stage — oracle ops needs agility (adding new pull-feeds, adjusting confidence thresholds). Burn it last, once the v1 feed mix is proven.
- New matchers can still be added per-market at init, from a permissionless registry. The registry program itself is burned alongside `perps-prog`.
- Risk params per market remain set-at-init-only — already an engine property.

Engine immutability is a property of the deployed binary, not the source. Source can keep evolving (and we'll keep tracking upstream for off-chain tooling).

## 10. What's not yet decided

Resolved:
- ~~Quote asset(s) at launch~~ → **USDC and wSOL** (parallel slabs per market).
- ~~Oracle cost model~~ → **on-chain aggregator over Pyth Pull + Switchboard, no own feeders at v1, $0 ongoing infra cost**.
- ~~Burn timeline~~ → **condition-based, gated by checklist, no fixed date**.

Still open:
1. **Insurance fund seeding.** Protocol-owned vs. fee-routed only? Probably both: small seed at launch, fees top up.
2. **Fee schedule and revenue split.** (Maker/taker split, protocol cut, LP share.)
3. **Funding rate policy.** Spec leaves this wrapper-owned. Linear-on-imbalance is the boring default; we may want a softer curve for low-OI markets.
4. **Per-market max leverage.** Permissionless implies caller-chosen, but `perps-prog` should enforce a hard upper bound (and probably a lower one too, to make low-leverage markets meaningful).
5. **Front-end hosting.** IPFS-only matches the burn ethos; centralized hosting matches UX expectations. Probably both — IPFS canonical, hosted convenience mirror.
6. **Matcher fee model.** Maker rebate? Taker-only? LP-receives-spread is the v1 default since `matcher-passive` is a passive LP.
7. **CI.** Currently no GitHub Actions on this repo. Recommend adding `cargo build` + `cargo test` + `cargo clippy -D warnings` on PR, and a separate scheduled workflow for the Kani proof suite.

## 11. Suggested next concrete steps

In rough order, each is a separate issue/PR:

1. Set up workspace skeleton with the four crates as empty stubs and a single `Cargo.toml` workspace; pin upstream commits.
2. Stand up `oracle-prog` v0: median + TWAP + staleness; ignore stake/slashing.
3. Wire `perps-prog` to read from `oracle-prog` instead of Pyth directly.
4. Fork `percolator-match` → `matcher-passive`; parameterize spread.
5. Integration test: deposit → trade → oracle move → liquidate, end-to-end, devnet.
6. Build keeper reference implementation.
7. Frontend stub (deposit + place trade only) + CLI parity.
8. Audit prep: freeze v1 spec deltas, regenerate Kani proofs, write threat model doc.
