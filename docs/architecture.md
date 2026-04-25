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

- `quote_mint` (USDC at launch; later, any SPL)
- `oracle_account` (must be of type `oracle-prog::PriceFeed`)
- `matcher_program` + `matcher_context_account`
- `risk_params` (margin, fees, liquidation, admission band)

**Permissionless creation:** anyone can call `init_market(...)` with their own params, paying the rent. To prevent spam/grief markets dragging the namespace:

- Enforce a non-trivial creation deposit (refundable on close).
- Require `oracle_account` to be one of: (a) a feed registered in our `oracle-prog` registry, OR (b) a market in `hyperp` mode (no external oracle — see §7).
- Require `matcher_program` to be in a small allowlist initially. Plan to make this fully open after the burn.

The `cfg_max_accrual_dt_slots`, `cfg_max_abs_funding_e9_per_slot`, `cfg_max_price_move_bps_per_slot` envelopes from the engine spec are **not** wrapper-tunable post-init — they're immutable per market, which is what makes the safety boundary hold. The `init_market` instruction validates that submitted values satisfy the solvency-envelope inequality from spec §8.

## 6. Oracle — design sketch

Goal stated by you: *impossible to manipulate*. Realistic goal: *manipulation requires compromising more independent operators than any attacker can plausibly afford, within the time window before the next update*.

### 6.1 What we are not doing
- **Not** trusting a single Pyth/Switchboard feed directly. They're inputs, not the truth.
- **Not** using on-chain DEX TWAPs as the price-of-record (Solana DEX liquidity is concentrated; cheap to push).

### 6.2 What we are doing — `oracle-prog`

Aggregator that consumes signed price observations from N independent feeders and produces a robust median with confidence:

1. **Inputs per slot window (e.g., 32 slots ≈ 12s):**
   - Pyth aggregate price + confidence
   - Switchboard On-Demand pull
   - At least 3 independent feeder signatures, each posting `(price, confidence, timestamp)` signed by their key. Feeders pull from CEX top-of-book + spot index providers.
2. **Aggregation:**
   - Drop any input older than `max_staleness_slots`.
   - Drop any input whose confidence > `max_conf_bps`.
   - Take the **median** (not mean — defeats one-sided pushes).
   - Require quorum: ≥`q` of `N` inputs survived filtering, else feed goes stale.
3. **Smoothing:**
   - Maintain an EMA-TWAP over the last `W` aggregations (e.g., 16 windows).
   - Publish both `spot_median` and `twap`. Engine consumes `twap` for liquidation/admission to make single-block manipulation worthless; closes/withdrawals can use `spot_median` gated by `cfg_max_price_move_bps_per_slot` (already in engine).
4. **Anti-grief:**
   - Feeder set is a registry. Each registered feeder posts a stake (slashable on provable malicious deviation — out of scope for v1; v1 is reputation-only with public deviation logs).
   - Anyone can read; only registered feeders can write.

### 6.3 Hyperp markets (no external oracle)
For longtail tokens with no good oracle, support `oracle_mode = Hyperp`: mark price is computed as a TWAP of internal trade prices (already supported by `percolator-prog`'s hyperp mode per their README). Funding then anchors to the protocol's own internal mark, not to spot. Riskier, but it's the only way to be truly permissionless on listing. We gate hyperp markets with a higher creation deposit and stricter `cfg_max_price_move_bps_per_slot`.

### 6.4 Open questions
- How many independent feeders do we run ourselves vs. recruit? Recommend: run 2 ourselves, recruit 3 partners pre-burn.
- Slashing — needed before burn or after?
- VRF for selecting which feeders are "primary" each window? Possibly overkill for v1.

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

## 9. Burning the upgrade authority — staged plan

```
Stage 0: launch.            Single multisig (3-of-5) is upgrade authority.
Stage 1: +1 month, no audit findings outstanding.
        → wrap multisig behind a timelock (24h).
Stage 2: +3 months, after 1 paid audit + bug bounty active.
        → 7-day timelock.
Stage 3: +6 months, after 2 audits + clean track record.
        → set upgrade authority to null. Program is immutable.
```

Pre-burn checklist (gates Stage 3):
- [ ] All Kani proofs pass against current source.
- [ ] No `pub` setters on immutable config fields (`cfg_*_max_*`).
- [ ] No outstanding P0/P1 findings.
- [ ] Insurance fund target reached.
- [ ] Oracle has ≥5 independent feeders, ≥3 of them external operators.
- [ ] Matcher allowlist has ≥2 implementations live.
- [ ] At least one stress-test event survived (volume spike, oracle outage drill).

After burn:
- New matchers can still be added per-market at init, drawn from a permissionless registry program (also burned).
- Risk params per market remain mutable per-market only at init time.
- Oracle feeder set can grow/shrink — we keep `oracle-prog` upgradable longer than `perps-prog`, since oracle ops requires more agility.

The engine itself is a library compiled in; immutability is a property of the deployed program, not the source.

## 10. What's not yet decided

1. **Quote asset(s) at launch.** USDC only, or also a yield-bearing wrapper?
2. **Insurance fund seeding.** Protocol-owned vs. fee-routed only?
3. **Fee schedule and revenue split.**
4. **Front-end hosting.** IPFS-only (matches the burn ethos) vs. centralized for UX.
5. **Funding rate policy.** Spec leaves this wrapper-owned. We need a curve.
6. **Per-market max leverage.** Permissionless implies caller-chosen, but we likely need a hard upper bound at the prog level.
7. **Slashing for oracle feeders** — pre-burn or post-burn.
8. **Matcher fee model.** LP-side fees, taker-side fees, or both.

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
