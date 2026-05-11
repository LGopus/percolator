# Kani Proof Strength Audit Results

Generated: 2026-05-11

Source prompt: `scripts/audit-proof-strength.md`.

Kani version: `cargo-kani 0.66.0`.

## Full Kani Timing Sweep

Command:

```text
scripts/run_kani_full_audit.sh
```

The sweep script parsed every standard `#[kani::proof]` harness in
`tests/proofs_*.rs` and ran each harness one-by-one with exact harness
selection and a `600s` timeout.

```text
SUMMARY: 416 passed, 0 failed/timeout (0 timeout) out of 416
```

Timing artifacts:

```text
kani_audit_full.tsv
kani_audit_final.tsv
```

`kani_audit_full.tsv` contains the raw per-proof timing/status rows.
`kani_audit_final.tsv` contains the same rows plus the sweep note
`overnight-2026-05-11`.

Aggregate timing:

| Metric | Value |
|---|---:|
| Harnesses | 416 |
| Pass | 416 |
| Fail | 0 |
| Timeout | 0 |
| Total wall-clock harness time | 10,876s |
| Slowest harness | `proof_force_close_resolved_pos_count_decrements` |
| Slowest harness time | 474s |

## Slowest Passing Harnesses

| Harness | Time |
|---|---:|
| `proof_force_close_resolved_pos_count_decrements` | 474s |
| `proof_validate_hint_preflight_oracle_shift` | 466s |
| `proof_permissionless_account_b_dispatch_returns_progress_for_hinted_blocker_on_prod_code` | 386s |
| `t3_14_epoch_mismatch_forces_terminal_close` | 373s |
| `proof_blocked_segment_recovery_rejects_when_bounded_accrual_can_progress_on_prod_code` | 308s |
| `t3_16b_reset_counter_with_nonzero_k_diff` | 297s |
| `t3_16_reset_pending_counter_invariant` | 286s |
| `t6_26_full_drain_reset_regression` | 283s |
| `proof_force_close_resolved_with_position_conserves` | 279s |
| `proof_force_close_resolved_position_conservation` | 266s |
| `v19_cascade_safety_gate_disabled_preserves_invariants` | 239s |
| `proof_b7_oi_balance_after_trade` | 207s |
| `proof_validate_hint_preflight_conservative` | 207s |
| `proof_liquidation_policy_validity` | 168s |
| `proof_bilateral_oi_decomposition` | 139s |
| `t14_63_dust_bound_position_reattach_remainder` | 135s |
| `proof_permissionless_account_b_progress_reduces_hinted_account_b_rank_on_prod_code` | 132s |
| `proof_execute_trade_full_margin_enforcement` | 120s |
| `t2_12_floor_shift_lemma` | 119s |
| `proof_force_close_resolved_with_profit_conserves` | 114s |
| `bounded_trade_conservation_symbolic_size` | 108s |
| `proof_goal27_finalize_path_independent` | 100s |
| `t3_14b_epoch_mismatch_with_nonzero_k_diff` | 82s |
| `proof_account_b_recovery_rejects_when_production_chunk_advances` | 81s |
| `bounded_trade_conservation` | 80s |

## Static Strength Scan

The static pass scanned the same 416-harness inventory for common weakness and
vacuity patterns from `scripts/audit-proof-strength.md`.

Inventory by file:

| File | Harnesses |
|---|---:|
| `tests/proofs_admission.rs` | 63 |
| `tests/proofs_arithmetic.rs` | 19 |
| `tests/proofs_audit.rs` | 33 |
| `tests/proofs_b_index.rs` | 21 |
| `tests/proofs_checklist.rs` | 16 |
| `tests/proofs_instructions.rs` | 58 |
| `tests/proofs_invariants.rs` | 26 |
| `tests/proofs_lazy_ak.rs` | 15 |
| `tests/proofs_liveness.rs` | 41 |
| `tests/proofs_safety.rs` | 99 |
| `tests/proofs_v1131.rs` | 25 |

Strength indicators:

| Check | Result |
|---|---:|
| Harnesses with symbolic inputs or symbolic helper construction | 233 |
| Constructed concrete production-code scenario harnesses | 183 |
| Harnesses with `kani::cover!` reachability checks | 140 |
| Harnesses without `kani::cover!` | 276 |
| Explicit `kani::assume(false)` / `assume(false)` findings | 0 |
| Heuristic Ok-gated assertion findings without cover/unwrap/expect | 0 |
| Heuristic Err-early-return findings | 0 |

Five harnesses did not contain ordinary assertion/unwrap/cover patterns, but
all five are intentional `#[kani::should_panic]` invalid-configuration proofs:

```text
proof_config_rejects_oversized_max_accounts
proof_config_rejects_zero_max_accounts
proof_config_rejects_invalid_bps
proof_config_rejects_liq_fee_inversion
proof_config_rejects_fee_cap_exceeds_max
```

Current classification:

| Classification | Status |
|---|---|
| Non-vacuity | No confirmed vacuous harnesses found in this static pass. |
| Weak proofs | No confirmed weak proofs found by the static patterns checked. |
| Inductive strength | The suite is not a fully inductive proof system over arbitrary symbolic `RiskEngine` states. Many harnesses are constructed production-code regression/spec proofs. |
| Practical proof boundary | The current suite is strong as a production-code safety/liveness regression suite, with many symbolic inputs and reachability checks, but it should not be described as proving every invariant for all possible corrupt arbitrary states. |

## Rust Test Matrix

Plain `cargo test` currently fails before executing tests because of the
pre-existing untracked scratch file `examples/dump_layout.rs`, which references
the removed `trading_fee_bps` field. That file is not part of the tracked audit
artifacts and was not modified by this run.

The tracked lib/test suite was run directly across the supported feature
profiles:

| Command | Result | Time |
|---|---|---:|
| `cargo test --lib --tests` | PASS | 0.070s |
| `cargo test --lib --tests --features small -q` | PASS | 0.583s |
| `cargo test --lib --tests --features medium -q` | PASS | 0.578s |
| `cargo test --lib --tests --features test -q` | PASS | 0.406s |

The `test` feature run executed the larger tracked test set:

```text
50 lib tests passed
3 AMM tests passed
368 unit tests passed
```

## Audit Conclusion

All 416 Kani proofs passed within the 10-minute per-harness cap, with timings
recorded in the checked-in TSV artifacts. The static proof-strength scan found
no confirmed vacuous or weak harnesses under the checked patterns.

The main limitation is proof style, not current pass/fail health: a substantial
part of the suite is made of concrete or semi-concrete production-code scenario
proofs. Those are useful and non-vacuous for the intended spec regressions, but
they are not equivalent to fully inductive arbitrary-state invariant proofs.
