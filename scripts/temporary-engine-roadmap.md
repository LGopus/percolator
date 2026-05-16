# Temporary V14 Proof Roadmap

Updated: 2026-05-16.

This is a working roadmap, not a certification artifact. It should stay
uncommitted unless explicitly promoted. The goal is to strengthen safety and
liveness claims with Kani proofs over production `src/v14.rs` code, not models.

## Current Baseline

- The current v14 proof suite has 113 Kani proofs after the latest incremental
  additions.
- The suite executes production transitions and production wire conversion code.
- The main remaining weakness is scope: many proofs use one asset, one account,
  or one transition branch so Kani can finish.

## P0: Broader Safety Slices

1. Add multi-asset full-refresh proofs.
   - Prove all active legs are settled and scored.
   - Prove a toxic second asset cannot be omitted by a hinted first-asset action.
   - Keep domains small, but use real `full_account_refresh` and trade paths.

2. Add an insurance-boundary proof family.
   - Public value-moving paths must not spend insurance except for
     post-principal negative PnL or explicit authorized withdrawal.
   - Fees must not be paid from insurance or socialized through B.

3. Add favorable-action lock composition proofs.
   - Withdraw, convert, close, and risk-increasing trade should reject or use
     no-positive-credit lanes under stale, B-stale, h-lock, loss-stale,
     target/effective lag, and recovery mode.

## P1: Broader Liveness Slices

1. Add repeated progress ranking proofs.
   - Repeated B settlement chunks complete or route to recovery.
   - Active bankrupt close and resolved partial close should similarly reduce a
     bounded rank each call.

2. Add crank progress composition proofs.
   - `permissionless_crank_not_atomic` should return bounded progress, recovery,
     or fail before value-moving mutation for each valid hinted action family.

3. Add recovery matrix proofs.
   - Every conservative failure class exposed by the engine should set a
     canonical recovery reason or return `RecoveryRequired` before mutation.

## P2: Boundary And Persistence

1. Extend persisted wire proofs.
   - Provenance mismatch, active bitmap mismatch, hidden legs, stale flags, and
     invalid encodings should fail closed from raw account state.

2. Add multi-account aggregate composition proofs.
   - Independent account-local actions should not drift `vault`, `c_tot`,
     `insurance`, positive PnL totals, or stale counters.

## Execution Log

- 2026-05-16: Added market/account persisted wire roundtrip proofs, persisted
  `i128::MIN` rejection proof, and repeated small B-chunk completion proof.
- 2026-05-16: Added `proof_v14_full_refresh_settles_and_scores_two_active_assets`
  over production `full_account_refresh`. The first broad symbolic-price version
  exceeded the ten-minute target, so the passing proof uses fixed distinct
  prices and symbolic capital to keep the composition claim tractable.
- 2026-05-16: Added `proof_v14_non_deficit_public_paths_do_not_decrease_insurance`
  over deposit, withdraw, direct fee charge, released-PnL conversion, and
  non-deficit resolved close. This complements the existing bankrupt-liquidation
  proofs that cover legitimate insurance consumption.
- 2026-05-16: Added
  `proof_v14_favorable_locks_block_released_pnl_conversion_before_mutation`
  over production released-PnL conversion. It proves threshold stress,
  bankruptcy h-lock, loss-stale, active bankrupt close, stale account, B-stale
  account, and target/effective lag all fail before moving PnL/capital/vault.
- 2026-05-16: Added
  `proof_v14_persisted_wire_rejects_provenance_and_hidden_leg_smuggling` over
  production persisted-account decoding and market validation. It proves wrong
  market, wrong owner, bitmap-only, hidden-active, and out-of-config raw legs
  are rejected before reaching runtime account state.
- 2026-05-16: Added
  `proof_v14_b_stale_trade_preflight_rolls_back_partial_side_effects` over the
  public staged trade API. It proves partial B settlement discovered during
  trade preflight rolls back and cannot leak into the real account or market.
- 2026-05-16: Added
  `proof_v14_deposit_into_stale_or_b_stale_account_does_not_unlock_favorable_actions`.
  It proves deposits into locked accounts preserve stale/B-stale counters,
  invalidate health certificates, and do not make favorable actions eligible.
