# Kani Proof Strength Audit Results

Generated: 2026-05-15

Source prompt: `scripts/audit-proof-strength.md`.

Kani version: `cargo-kani 0.66.0`.

## Full Kani Timing Sweep

Command:

```text
scripts/run_kani_full_audit.sh
```

The v13 cutover removed the v12 slab and retired the slab-specific proof
inventory. This sweep parses every `tests/proofs_*.rs` file and runs each
harness one-by-one with exact harness selection and a `600s` timeout.

```text
SUMMARY: 57 passed, 0 failed/timeout (0 timeout) out of 57
```

Timing artifacts:

```text
kani_audit_full.tsv
kani_audit_final.tsv
```

Aggregate timing:

| Metric | Value |
|---|---:|
| Harnesses | 57 |
| Pass | 57 |
| Fail | 0 |
| Timeout | 0 |
| Total wall-clock harness time | 2372s |
| Slowest harness | `proof_v13_bankrupt_liquidation_cannot_free_exposure_before_residual_durable` |
| Slowest harness time | 397s |

## Slowest Harnesses

All per-harness timings are recorded in `kani_audit_final.tsv`.

| Harness | Time | Status |
|---|---:|---|
| `proof_v13_bankrupt_liquidation_cannot_free_exposure_before_residual_durable` | 397s | PASS |
| `proof_v13_k_pair_mul_div_floor_matches_small_reference` | 193s | PASS |
| `proof_v13_trade_fee_conservation_and_oi_symmetry` | 160s | PASS |
| `proof_v13_sign_flip_trade_preserves_oi_symmetry_and_senior_accounting` | 150s | PASS |
| `proof_v13_account_b_chunk_either_advances_or_fails_closed` | 125s | PASS |
| `proof_v13_rebalance_reduce_position_preserves_senior_claims_and_reduces_risk` | 115s | PASS |
| `proof_v13_hlock_allows_pure_risk_reducing_trade_with_principal_margin` | 109s | PASS |
| `proof_v13_resolved_close_partial_b_settlement_makes_progress_without_closing` | 96s | PASS |
| `proof_v13_risk_increasing_trade_requires_initial_health_before_mutation` | 82s | PASS |
| `proof_v13_resolved_profit_close_pays_snapshot_residual_and_clears_claim` | 81s | PASS |
| `proof_v13_bankrupt_liquidation_excludes_fee_from_residual_and_spends_insurance_once` | 70s | PASS |
| `proof_v13_partial_liquidation_can_reduce_risk_without_forcing_full_close` | 64s | PASS |
| `proof_v13_bankrupt_liquidation_consumes_insurance_before_social_loss` | 59s | PASS |
| `proof_v13_permissionless_refresh_returns_partial_b_progress_without_accrual` | 50s | PASS |
| `proof_v13_funding_accrual_refresh_matches_sign_and_floor` | 47s | PASS |
| `proof_v13_price_accrual_refresh_matches_eager_mark_pnl` | 47s | PASS |
| `proof_v13_wide_signed_mul_div_floor_matches_small_reference` | 47s | PASS |
| `proof_v13_attach_then_clear_leg_restores_account_local_counters_for_long` | 44s | PASS |
| `proof_v13_mul_div_ceil_u256_is_floor_plus_remainder_indicator` | 40s | PASS |
| `proof_v13_b_residual_booking_makes_durable_progress_or_fails_closed` | 35s | PASS |

## V12 Property Migration

The old v12 proof inventory had 416 Kani harnesses. Many were intentionally not
ported because v13 removed the slab, fixed account capacity, full-market cursor
scan, v12 reserve queues, and wrapper-era entrypoints. The applicable properties
were migrated to v13 production-code tests/proofs.

Migrated property families covered in the v13 suite:

| v12 property family | v13 coverage |
|---|---|
| Deposit/withdraw accounting roundtrip | `proof_v13_deposit_then_withdraw_roundtrip_preserves_accounting`, `proof_v13_partial_withdraw_can_leave_small_remainder` |
| Multiple deposits aggregate into senior totals | `proof_v13_multiple_deposits_aggregate_c_tot_and_vault` |
| Account close/reclaim requires clean local state | `proof_v13_close_portfolio_account_requires_clean_local_state` |
| Malformed signed fee-credit and PnL state fails closed | `proof_v13_account_equity_rejects_malformed_fee_credits`, `proof_v13_account_equity_rejects_i128_min_persistent_pnl` |
| Conservative risk-notional arithmetic | `proof_v13_risk_notional_flat_zero_and_monotone_in_price` |
| Shared wide arithmetic floor/ceil/K-diff semantics | `tests/proofs_v13_arithmetic.rs` |
| Position bounds reject before OI mutation | `proof_v13_oversize_position_rejected_before_oi_mutation` |
| Price/funding accrual matches eager account settlement | `proof_v13_price_accrual_refresh_matches_eager_mark_pnl`, `proof_v13_funding_accrual_refresh_matches_sign_and_floor` |
| Same-slot exposed price move cannot mutate state | `proof_v13_same_slot_exposed_price_move_rejects_before_mutation` |
| Funding cap rejects before state mutation | `proof_v13_funding_rate_above_cap_rejects_before_mutation` |
| Dynamic trade-fee cap, conservation, and OI symmetry | `proof_v13_trade_dynamic_fee_cap_is_enforced_before_mutation`, `proof_v13_trade_fee_conservation_and_oi_symmetry` |
| Invalid/risk-increasing trade rejects before mutation | `proof_v13_invalid_trade_request_rejects_before_any_mutation`, `proof_v13_risk_increasing_trade_requires_initial_health_before_mutation` |
| Sign-flip trades preserve OI symmetry and senior totals | `proof_v13_sign_flip_trade_preserves_oi_symmetry_and_senior_accounting` |
| Released PnL conversion cannot mint beyond residual | `proof_v13_released_pnl_conversion_is_residual_bounded_and_conserves_vault` |
| Permissionless refresh must return partial B progress | `proof_v13_permissionless_refresh_returns_partial_b_progress_without_accrual` |
| Public user-fund config must keep recovery/profile guarantees enabled | `proof_v13_public_config_rejects_invalid_user_fund_shapes` |
| Liquidation must strictly improve account risk and preserve residual durability | `proof_v13_liquidation_progress_rejects_non_reducing_scores`, `proof_v13_partial_liquidation_can_reduce_risk_without_forcing_full_close`, bankrupt-liquidation proofs |
| Resolved close payout/progress behavior | resolved flat, positive, partial-B, and active-position progress proofs |

## Static Strength Scan

Inventory by file:

| File | Harnesses |
|---|---:|
| `tests/proofs_v13.rs` | 50 |
| `tests/proofs_v13_arithmetic.rs` | 7 |

Strength indicators:

| Check | Result |
|---|---:|
| Harnesses over v13 production transitions | 50 |
| Harnesses over shared production arithmetic helpers | 7 |
| Harnesses with symbolic inputs or symbolic branch choices | 28 |
| Harnesses with `kani::cover!` reachability checks | 28 |
| Explicit `kani::assume(false)` / `assume(false)` findings | 0 |
| Confirmed vacuous harnesses | 0 |
| Confirmed weak harnesses | 0 |

Current classification:

| Classification | Status |
|---|---|
| Non-vacuity | No confirmed vacuous harnesses found. Cover checks exercise h-min/h-max, stale set/clear, hidden-leg rejection, B-chunk progress paths, malformed fee-credit states, invalid config branches, aggregate deposit branches, arithmetic floor/ceil branches, positive/negative K-diff branches, bankrupt residual recovery, zero/partial insurance paths, permissionless partial-B refresh, released-PnL zero/positive conversion paths, resolved partial-B close progress, and rebalance reduction paths. |
| Weak proofs | No confirmed weak proofs in the v13 inventory. Concrete-branch harnesses are intentional regression proofs over production methods, and symbolic arithmetic/transition harnesses cover the remaining branch families. |
| Inductive strength | The stale-counter and arithmetic helper proofs are closest to local inductive transition proofs. The overall suite is a strong production-code safety/liveness harness set, not a complete arbitrary-state inductive proof of the whole engine. |
| Practical proof boundary | The suite proves key v13 account-local invariants over real production methods: h-lock selection, provenance/hidden-leg fail-closed behavior, stale counter idempotence and refresh clearing, malformed signed state rejection, deposit/withdraw accounting, aggregate senior accounting, close-account local-state gating, risk-notional monotonicity, position-bound fail-before-mutation, B-chunk progress/fail-closed behavior, full-refresh gating, monotonic liquidation-score rejection, loss-before-fee ordering, account-free equity-active accrual protective-progress gating, one-segment bounded catchup, funding-rate cap fail-before-mutation, dynamic trade-fee enforcement, trade conservation/OI symmetry, target/effective lag risk-increase rejection, h-lock risk-increase rejection, h-lock risk-reducing liveness under no-positive-credit margin, h-lock withdrawal no-positive-credit gating, released-PnL conversion bounded by residual, loss-stale nonflat withdrawal blocking, bankrupt liquidation insurance-before-social-loss ordering, bankrupt residual durability before exposure release, uncollectible liquidation-fee exclusion from residual loss, resolved close liveness and payout shape, durable B residual booking, prior-epoch reset clearing, quantity-ADL OI symmetry, rebalance strict risk-progress, price/funding settlement, invalid trade rollback, partial liquidation, and shared wide arithmetic semantics. |

## Rust Test Matrix

| Command | Result |
|---|---|
| `cargo test --features test` | PASS |

The Rust suite currently includes 60 v13 spec tests in
`tests/v13_spec_tests.rs`.

## Audit Conclusion

All v13 Kani proofs pass within the 10-minute per-harness cap. No weak or
vacuous proof was identified in this pass. Applicable v12 property families
have been reviewed and either ported to v13 production-code tests/proofs or
retired as slab/wrapper/v12-queue-specific.
