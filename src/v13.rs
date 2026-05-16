//! v13 account-local risk engine.
//!
//! This module implements the v13 slab-free engine surface: authenticated
//! portfolio accounts, bounded per-account refresh, lazy A/K/F/B settlement,
//! loss-senior fee handling, account-local cranks, residual B booking, dynamic
//! trade fees, liquidation progress checks, and resolved account close.

use crate::wide_math::{
    checked_mul_div_ceil_u256, floor_div_signed_conservative_i128, mul_div_floor_u256_with_rem,
    wide_mul_div_floor_u128, wide_signed_mul_div_floor_from_k_pair, U256,
};
use crate::{
    ADL_ONE, FUNDING_DEN, MAX_ACCOUNT_NOTIONAL, MAX_MARGIN_BPS, MAX_ORACLE_PRICE,
    MAX_POSITION_ABS_Q, MAX_PROTOCOL_FEE_ABS, MAX_TRADE_SIZE_Q, MAX_VAULT_TVL, MIN_A_SIDE,
    POS_SCALE, SOCIAL_LOSS_DEN, SOCIAL_WEIGHT_SCALE,
};

pub const V13_MAX_PORTFOLIO_ASSETS_N: usize = 16;
pub const V13_LAYOUT_DISCRIMINATOR: u16 = 13;
pub const V13_ACCOUNT_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V13Error {
    InvalidConfig,
    ArithmeticOverflow,
    ProvenanceMismatch,
    HiddenLeg,
    InvalidLeg,
    Stale,
    BStale,
    LockActive,
    NonProgress,
    RecoveryRequired,
    CounterOverflow,
    CounterUnderflow,
}

pub type V13Result<T> = core::result::Result<T, V13Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HLockLaneV13 {
    HMin,
    HMax,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideV13 {
    Long,
    Short,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideModeV13 {
    Normal,
    DrainOnly,
    ResetPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketModeV13 {
    Live,
    Resolved,
    Recovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionlessRecoveryReasonV13 {
    BelowProgressFloor,
    BlockedSegmentHeadroomOrRepresentability,
    AccountBSettlementCannotProgress,
    BIndexHeadroomExhausted,
    ActiveBankruptCloseCannotProgress,
    ExplicitLossOrDustAuditOverflow,
    OracleOrTargetUnavailableByAuthenticatedPolicy,
    CounterOrEpochOverflowDeclaredRecovery,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProvenanceHeaderV13 {
    pub market_group_id: [u8; 32],
    pub portfolio_account_id: [u8; 32],
    pub owner: [u8; 32],
    pub version: u16,
    pub layout_discriminator: u16,
}

impl ProvenanceHeaderV13 {
    pub const fn new(
        market_group_id: [u8; 32],
        portfolio_account_id: [u8; 32],
        owner: [u8; 32],
    ) -> Self {
        Self {
            market_group_id,
            portfolio_account_id,
            owner,
            version: V13_ACCOUNT_VERSION,
            layout_discriminator: V13_LAYOUT_DISCRIMINATOR,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V13Config {
    pub max_portfolio_assets: u8,
    pub min_nonzero_mm_req: u128,
    pub min_nonzero_im_req: u128,
    pub h_min: u64,
    pub h_max: u64,
    pub maintenance_margin_bps: u64,
    pub initial_margin_bps: u64,
    pub max_trading_fee_bps: u64,
    pub liquidation_fee_bps: u64,
    pub liquidation_fee_cap: u128,
    pub min_liquidation_abs: u128,
    pub max_accrual_dt_slots: u64,
    pub max_abs_funding_e9_per_slot: u64,
    pub min_funding_lifetime_slots: u64,
    pub max_price_move_bps_per_slot: u64,
    pub max_account_b_settlement_chunks: u64,
    pub max_bankrupt_close_chunks: u64,
    pub public_b_chunk_atoms: u128,
    pub permissionless_recovery_enabled: bool,
    pub stale_certificate_penalty_enabled: bool,
    pub full_refresh_required_for_favorable_actions: bool,
    pub public_liveness_profile_crank_forward: bool,
}

impl V13Config {
    pub const fn public_user_fund(max_portfolio_assets: u8, h_min: u64, h_max: u64) -> Self {
        Self {
            max_portfolio_assets,
            min_nonzero_mm_req: 1,
            min_nonzero_im_req: 2,
            h_min,
            h_max,
            maintenance_margin_bps: 10_000,
            initial_margin_bps: 10_000,
            max_trading_fee_bps: 0,
            liquidation_fee_bps: 0,
            liquidation_fee_cap: 0,
            min_liquidation_abs: 0,
            max_accrual_dt_slots: 1,
            max_abs_funding_e9_per_slot: 0,
            min_funding_lifetime_slots: 1,
            max_price_move_bps_per_slot: 10_000,
            max_account_b_settlement_chunks: 1,
            max_bankrupt_close_chunks: 1,
            public_b_chunk_atoms: MAX_VAULT_TVL,
            permissionless_recovery_enabled: true,
            stale_certificate_penalty_enabled: true,
            full_refresh_required_for_favorable_actions: true,
            public_liveness_profile_crank_forward: true,
        }
    }

    fn ceil_div_u256_to_u128(n: U256, d: U256) -> V13Result<u128> {
        if d.is_zero() {
            return Err(V13Error::InvalidConfig);
        }
        let q = n.checked_div(d).ok_or(V13Error::InvalidConfig)?;
        let r = n.checked_rem(d).ok_or(V13Error::InvalidConfig)?;
        let q = if r.is_zero() {
            q
        } else {
            q.checked_add(U256::ONE).ok_or(V13Error::InvalidConfig)?
        };
        q.try_into_u128().ok_or(V13Error::InvalidConfig)
    }

    fn checked_mul_div_ceil_to_u128(a: u128, b: u128, d: u128) -> V13Result<u128> {
        checked_mul_div_ceil_u256(U256::from_u128(a), U256::from_u128(b), U256::from_u128(d))
            .and_then(|v| v.try_into_u128())
            .ok_or(V13Error::InvalidConfig)
    }

    fn solvency_envelope_total_for_notional(
        &self,
        n: u128,
        loss_budget_num: u128,
        loss_budget_den: u128,
        price_budget_bps: u128,
    ) -> V13Result<u128> {
        let loss = Self::checked_mul_div_ceil_to_u128(n, loss_budget_num, loss_budget_den)?;

        let worst_liq_multiplier = 10_000u128
            .checked_add(price_budget_bps)
            .ok_or(V13Error::InvalidConfig)?;
        let worst_liq_notional =
            Self::checked_mul_div_ceil_to_u128(n, worst_liq_multiplier, 10_000)?;
        let liq_fee_raw = Self::checked_mul_div_ceil_to_u128(
            worst_liq_notional,
            self.liquidation_fee_bps as u128,
            10_000,
        )?;
        let liq_fee = core::cmp::min(
            core::cmp::max(liq_fee_raw, self.min_liquidation_abs),
            self.liquidation_fee_cap,
        );

        loss.checked_add(liq_fee).ok_or(V13Error::InvalidConfig)
    }

    fn maintenance_requirement_for_notional(&self, n: u128) -> V13Result<u128> {
        let mm_prop = U256::from_u128(n)
            .checked_mul(U256::from_u128(self.maintenance_margin_bps as u128))
            .and_then(|v| v.checked_div(U256::from_u128(10_000)))
            .and_then(|v| v.try_into_u128())
            .ok_or(V13Error::InvalidConfig)?;
        Ok(core::cmp::max(mm_prop, self.min_nonzero_mm_req))
    }

    fn solvency_envelope_holds_for_notional(
        &self,
        n: u128,
        loss_budget_num: u128,
        loss_budget_den: u128,
        price_budget_bps: u128,
    ) -> V13Result<bool> {
        let total = self.solvency_envelope_total_for_notional(
            n,
            loss_budget_num,
            loss_budget_den,
            price_budget_bps,
        )?;
        let mm_req = self.maintenance_requirement_for_notional(n)?;
        Ok(total <= mm_req)
    }

    fn solvency_envelope_interval_certifies(
        &self,
        lo: u128,
        hi: u128,
        loss_budget_num: u128,
        loss_budget_den: u128,
        price_budget_bps: u128,
    ) -> V13Result<bool> {
        let total_hi = self.solvency_envelope_total_for_notional(
            hi,
            loss_budget_num,
            loss_budget_den,
            price_budget_bps,
        )?;
        let mm_lo = self.maintenance_requirement_for_notional(lo)?;
        Ok(total_hi <= mm_lo)
    }

    fn validate_solvency_envelope_range(
        &self,
        lo: u128,
        hi: u128,
        loss_budget_num: u128,
        loss_budget_den: u128,
        price_budget_bps: u128,
    ) -> V13Result<()> {
        if lo > hi {
            return Ok(());
        }

        const MAX_SOLVENCY_INTERVALS: usize = 96;
        const MAX_SOLVENCY_STEPS: usize = 4096;
        const EXACT_CHUNK: u128 = 64;

        let mut stack = [(0u128, 0u128); MAX_SOLVENCY_INTERVALS];
        let mut len = 1usize;
        let mut steps = 0usize;
        stack[0] = (lo, hi);

        while len != 0 {
            steps = steps.checked_add(1).ok_or(V13Error::InvalidConfig)?;
            if steps > MAX_SOLVENCY_STEPS {
                return Err(V13Error::InvalidConfig);
            }

            len -= 1;
            let (range_lo, range_hi) = stack[len];

            if self.solvency_envelope_interval_certifies(
                range_lo,
                range_hi,
                loss_budget_num,
                loss_budget_den,
                price_budget_bps,
            )? {
                continue;
            }

            if range_hi == range_lo || range_hi - range_lo <= EXACT_CHUNK {
                let mut n = range_lo;
                loop {
                    if !self.solvency_envelope_holds_for_notional(
                        n,
                        loss_budget_num,
                        loss_budget_den,
                        price_budget_bps,
                    )? {
                        return Err(V13Error::InvalidConfig);
                    }
                    if n == range_hi {
                        break;
                    }
                    n = n.checked_add(1).ok_or(V13Error::InvalidConfig)?;
                }
                continue;
            }

            let mid = range_lo + (range_hi - range_lo) / 2;
            if len + 2 > MAX_SOLVENCY_INTERVALS {
                return Err(V13Error::InvalidConfig);
            }
            stack[len] = (mid.checked_add(1).ok_or(V13Error::InvalidConfig)?, range_hi);
            stack[len + 1] = (range_lo, mid);
            len += 2;
        }

        Ok(())
    }

    fn validate_funding_headroom(&self, slots: u64) -> V13Result<()> {
        let max_signed = U256::from_u128(i128::MAX as u128);
        let headroom = U256::from_u128(ADL_ONE)
            .checked_mul(U256::from_u128(MAX_ORACLE_PRICE as u128))
            .and_then(|v| v.checked_mul(U256::from_u128(self.max_abs_funding_e9_per_slot as u128)))
            .and_then(|v| v.checked_mul(U256::from_u128(slots as u128)))
            .ok_or(V13Error::InvalidConfig)?;
        if headroom <= max_signed {
            Ok(())
        } else {
            Err(V13Error::InvalidConfig)
        }
    }

    fn validate_exact_solvency_envelope(&self) -> V13Result<()> {
        let price_budget_fast = (self.max_price_move_bps_per_slot as u128)
            .checked_mul(self.max_accrual_dt_slots as u128)
            .ok_or(V13Error::InvalidConfig)?;
        if self.maintenance_margin_bps == 10_000
            && price_budget_fast <= 10_000
            && self.max_abs_funding_e9_per_slot == 0
            && self.liquidation_fee_bps == 0
            && self.min_liquidation_abs == 0
        {
            return Ok(());
        }

        self.validate_funding_headroom(self.max_accrual_dt_slots)?;
        self.validate_funding_headroom(self.min_funding_lifetime_slots)?;

        let move_cap = U256::from_u128(self.max_price_move_bps_per_slot as u128);
        let dt = U256::from_u128(self.max_accrual_dt_slots as u128);
        let rate = U256::from_u128(self.max_abs_funding_e9_per_slot as u128);
        let ten_thousand = U256::from_u128(10_000);
        let funding_den = U256::from_u128(FUNDING_DEN);

        let price_budget_bps = move_cap
            .checked_mul(dt)
            .and_then(|v| v.try_into_u128())
            .ok_or(V13Error::InvalidConfig)?;
        let funding_budget_num = rate
            .checked_mul(dt)
            .and_then(|v| v.checked_mul(ten_thousand))
            .ok_or(V13Error::InvalidConfig)?;
        let loss_budget_num_wide = U256::from_u128(price_budget_bps)
            .checked_mul(funding_den)
            .and_then(|v| v.checked_add(funding_budget_num))
            .ok_or(V13Error::InvalidConfig)?;
        let loss_budget_den_wide = ten_thousand
            .checked_mul(funding_den)
            .ok_or(V13Error::InvalidConfig)?;

        let funding_budget_bps_ceil = Self::ceil_div_u256_to_u128(funding_budget_num, funding_den)?;
        let loss_budget_bps_ceil = price_budget_bps
            .checked_add(funding_budget_bps_ceil)
            .ok_or(V13Error::InvalidConfig)?;
        let worst_liq_budget_bps_ceil = Self::ceil_div_u256_to_u128(
            U256::from_u128(
                10_000u128
                    .checked_add(price_budget_bps)
                    .ok_or(V13Error::InvalidConfig)?,
            )
            .checked_mul(U256::from_u128(self.liquidation_fee_bps as u128))
            .ok_or(V13Error::InvalidConfig)?,
            ten_thousand,
        )?;
        let linear_budget_bps = loss_budget_bps_ceil
            .checked_add(worst_liq_budget_bps_ceil)
            .ok_or(V13Error::InvalidConfig)?;

        if self.maintenance_margin_bps == 10_000
            && loss_budget_bps_ceil == 10_000
            && worst_liq_budget_bps_ceil == 0
            && self.min_liquidation_abs == 0
        {
            return Ok(());
        }

        let loss_budget_num = loss_budget_num_wide
            .try_into_u128()
            .ok_or(V13Error::InvalidConfig)?;
        let loss_budget_den = loss_budget_den_wide
            .try_into_u128()
            .ok_or(V13Error::InvalidConfig)?;
        let domain_max = MAX_ACCOUNT_NOTIONAL;

        if self.maintenance_margin_bps == 0 {
            if self.solvency_envelope_holds_for_notional(
                domain_max,
                loss_budget_num,
                loss_budget_den,
                price_budget_bps,
            )? {
                return Ok(());
            }
            return Err(V13Error::InvalidConfig);
        }

        let floor_region_max = U256::from_u128(
            self.min_nonzero_mm_req
                .checked_add(1)
                .ok_or(V13Error::InvalidConfig)?,
        )
        .checked_mul(ten_thousand)
        .and_then(|v| v.checked_sub(U256::ONE))
        .and_then(|v| v.checked_div(U256::from_u128(self.maintenance_margin_bps as u128)))
        .and_then(|v| v.try_into_u128())
        .ok_or(V13Error::InvalidConfig)?;
        let floor_region_end = core::cmp::min(floor_region_max, domain_max);
        if floor_region_end != 0
            && !self.solvency_envelope_holds_for_notional(
                floor_region_end,
                loss_budget_num,
                loss_budget_den,
                price_budget_bps,
            )?
        {
            return Err(V13Error::InvalidConfig);
        }
        if floor_region_max >= domain_max {
            return Ok(());
        }

        let exact_start = floor_region_end
            .checked_add(1)
            .ok_or(V13Error::InvalidConfig)?;

        if linear_budget_bps < self.maintenance_margin_bps as u128 {
            let slope_gap = (self.maintenance_margin_bps as u128) - linear_budget_bps;
            let tail_for_linear = Self::ceil_div_u256_to_u128(
                U256::from_u128(3 * 10_000),
                U256::from_u128(slope_gap),
            )?;

            let loss_gap = (self.maintenance_margin_bps as u128)
                .checked_sub(loss_budget_bps_ceil)
                .ok_or(V13Error::InvalidConfig)?;
            let floor_fee_slack = self
                .min_liquidation_abs
                .checked_add(2)
                .ok_or(V13Error::InvalidConfig)?;
            let tail_for_fee_floor = Self::ceil_div_u256_to_u128(
                U256::from_u128(floor_fee_slack)
                    .checked_mul(ten_thousand)
                    .ok_or(V13Error::InvalidConfig)?,
                U256::from_u128(loss_gap),
            )?;

            let exact_tail = core::cmp::max(tail_for_linear, tail_for_fee_floor);
            if exact_tail <= exact_start {
                return Ok(());
            }
            let exact_end = core::cmp::min(exact_tail.saturating_sub(1), domain_max);
            return self.validate_solvency_envelope_range(
                exact_start,
                exact_end,
                loss_budget_num,
                loss_budget_den,
                price_budget_bps,
            );
        }

        if loss_budget_bps_ceil >= self.maintenance_margin_bps as u128 {
            return self.validate_solvency_envelope_range(
                exact_start,
                domain_max,
                loss_budget_num,
                loss_budget_den,
                price_budget_bps,
            );
        }

        let slope_gap = (self.maintenance_margin_bps as u128) - loss_budget_bps_ceil;
        let capped_fee_slack = self
            .liquidation_fee_cap
            .checked_add(3)
            .ok_or(V13Error::InvalidConfig)?;
        let exact_tail = Self::ceil_div_u256_to_u128(
            U256::from_u128(capped_fee_slack)
                .checked_mul(ten_thousand)
                .ok_or(V13Error::InvalidConfig)?,
            U256::from_u128(slope_gap),
        )?;

        if exact_tail <= exact_start {
            return Ok(());
        }

        let exact_end = core::cmp::min(exact_tail.saturating_sub(1), domain_max);
        self.validate_solvency_envelope_range(
            exact_start,
            exact_end,
            loss_budget_num,
            loss_budget_den,
            price_budget_bps,
        )
    }

    pub fn validate_public_user_fund(&self) -> V13Result<()> {
        if self.max_portfolio_assets == 0
            || self.max_portfolio_assets as usize > V13_MAX_PORTFOLIO_ASSETS_N
        {
            return Err(V13Error::InvalidConfig);
        }
        if self.h_max == 0 || self.h_min > self.h_max {
            return Err(V13Error::InvalidConfig);
        }
        if self.min_nonzero_mm_req == 0 || self.min_nonzero_mm_req >= self.min_nonzero_im_req {
            return Err(V13Error::InvalidConfig);
        }
        if self.maintenance_margin_bps > self.initial_margin_bps
            || self.initial_margin_bps > MAX_MARGIN_BPS
            || self.max_trading_fee_bps > MAX_MARGIN_BPS
            || self.liquidation_fee_bps > MAX_MARGIN_BPS
            || self.min_liquidation_abs > self.liquidation_fee_cap
            || self.liquidation_fee_cap > MAX_PROTOCOL_FEE_ABS
            || self.max_accrual_dt_slots == 0
            || self.min_funding_lifetime_slots < self.max_accrual_dt_slots
            || self.max_abs_funding_e9_per_slot > 10_000
            || self.max_price_move_bps_per_slot == 0
            || self.max_account_b_settlement_chunks == 0
            || self.max_bankrupt_close_chunks == 0
            || self.public_b_chunk_atoms == 0
        {
            return Err(V13Error::InvalidConfig);
        }
        if !self.permissionless_recovery_enabled
            || !self.stale_certificate_penalty_enabled
            || !self.full_refresh_required_for_favorable_actions
            || !self.public_liveness_profile_crank_forward
        {
            return Err(V13Error::InvalidConfig);
        }
        self.validate_exact_solvency_envelope()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetStateV13 {
    pub raw_oracle_target_price: u64,
    pub effective_price: u64,
    pub fund_px_last: u64,
    pub a_long: u128,
    pub a_short: u128,
    pub k_long: i128,
    pub k_short: i128,
    pub f_long_num: i128,
    pub f_short_num: i128,
    pub k_epoch_start_long: i128,
    pub k_epoch_start_short: i128,
    pub f_epoch_start_long_num: i128,
    pub f_epoch_start_short_num: i128,
    pub b_long_num: u128,
    pub b_short_num: u128,
    pub b_epoch_start_long_num: u128,
    pub b_epoch_start_short_num: u128,
    pub oi_eff_long_q: u128,
    pub oi_eff_short_q: u128,
    pub stored_pos_count_long: u64,
    pub stored_pos_count_short: u64,
    pub stale_account_count_long: u64,
    pub stale_account_count_short: u64,
    pub loss_weight_sum_long: u128,
    pub loss_weight_sum_short: u128,
    pub social_loss_remainder_long_num: u128,
    pub social_loss_remainder_short_num: u128,
    pub social_loss_dust_long_num: u128,
    pub social_loss_dust_short_num: u128,
    pub explicit_unallocated_loss_long: u128,
    pub explicit_unallocated_loss_short: u128,
    pub epoch_long: u64,
    pub epoch_short: u64,
    pub mode_long: SideModeV13,
    pub mode_short: SideModeV13,
}

impl Default for AssetStateV13 {
    fn default() -> Self {
        Self {
            raw_oracle_target_price: 1,
            effective_price: 1,
            fund_px_last: 1,
            a_long: ADL_ONE,
            a_short: ADL_ONE,
            k_long: 0,
            k_short: 0,
            f_long_num: 0,
            f_short_num: 0,
            k_epoch_start_long: 0,
            k_epoch_start_short: 0,
            f_epoch_start_long_num: 0,
            f_epoch_start_short_num: 0,
            b_long_num: 0,
            b_short_num: 0,
            b_epoch_start_long_num: 0,
            b_epoch_start_short_num: 0,
            oi_eff_long_q: 0,
            oi_eff_short_q: 0,
            stored_pos_count_long: 0,
            stored_pos_count_short: 0,
            stale_account_count_long: 0,
            stale_account_count_short: 0,
            loss_weight_sum_long: 0,
            loss_weight_sum_short: 0,
            social_loss_remainder_long_num: 0,
            social_loss_remainder_short_num: 0,
            social_loss_dust_long_num: 0,
            social_loss_dust_short_num: 0,
            explicit_unallocated_loss_long: 0,
            explicit_unallocated_loss_short: 0,
            epoch_long: 0,
            epoch_short: 0,
            mode_long: SideModeV13::Normal,
            mode_short: SideModeV13::Normal,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortfolioLegV13 {
    pub active: bool,
    pub side: SideV13,
    pub basis_pos_q: i128,
    pub a_basis: u128,
    pub k_snap: i128,
    pub f_snap: i128,
    pub epoch_snap: u64,
    pub loss_weight: u128,
    pub b_snap: u128,
    pub b_rem: u128,
    pub b_epoch_snap: u64,
    pub b_stale: bool,
    pub stale: bool,
}

impl PortfolioLegV13 {
    pub const EMPTY: Self = Self {
        active: false,
        side: SideV13::Long,
        basis_pos_q: 0,
        a_basis: ADL_ONE,
        k_snap: 0,
        f_snap: 0,
        epoch_snap: 0,
        loss_weight: 0,
        b_snap: 0,
        b_rem: 0,
        b_epoch_snap: 0,
        b_stale: false,
        stale: false,
    };
}

impl Default for PortfolioLegV13 {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HealthCertV13 {
    pub certified_equity: i128,
    pub certified_initial_req: u128,
    pub certified_maintenance_req: u128,
    pub certified_liq_deficit: u128,
    pub certified_worst_case_loss: u128,
    pub cert_oracle_epoch: u64,
    pub cert_funding_epoch: u64,
    pub cert_risk_epoch: u64,
    pub active_bitmap_at_cert: u32,
    pub valid: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortfolioAccountV13 {
    pub provenance_header: ProvenanceHeaderV13,
    pub owner: [u8; 32],
    pub capital: u128,
    pub pnl: i128,
    pub reserved_pnl: u128,
    pub fee_credits: i128,
    pub last_fee_slot: u64,
    pub active_bitmap: u32,
    pub legs: [PortfolioLegV13; V13_MAX_PORTFOLIO_ASSETS_N],
    pub health_cert: HealthCertV13,
    pub stale_state: bool,
    pub b_stale_state: bool,
    pub rebalance_lock: bool,
    pub liquidation_lock: bool,
}

impl PortfolioAccountV13 {
    pub const fn empty(header: ProvenanceHeaderV13) -> Self {
        Self {
            provenance_header: header,
            owner: header.owner,
            capital: 0,
            pnl: 0,
            reserved_pnl: 0,
            fee_credits: 0,
            last_fee_slot: 0,
            active_bitmap: 0,
            legs: [PortfolioLegV13::EMPTY; V13_MAX_PORTFOLIO_ASSETS_N],
            health_cert: HealthCertV13 {
                certified_equity: 0,
                certified_initial_req: 0,
                certified_maintenance_req: 0,
                certified_liq_deficit: 0,
                certified_worst_case_loss: 0,
                cert_oracle_epoch: 0,
                cert_funding_epoch: 0,
                cert_risk_epoch: 0,
                active_bitmap_at_cert: 0,
                valid: false,
            },
            stale_state: false,
            b_stale_state: false,
            rebalance_lock: false,
            liquidation_lock: false,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketGroupV13 {
    pub market_group_id: [u8; 32],
    pub config: V13Config,
    pub vault: u128,
    pub insurance: u128,
    pub c_tot: u128,
    pub pnl_pos_tot: u128,
    pub pnl_matured_pos_tot: u128,
    pub materialized_portfolio_count: u64,
    pub stale_certificate_count: u64,
    pub b_stale_account_count: u64,
    pub negative_pnl_account_count: u64,
    pub risk_epoch: u64,
    pub oracle_epoch: u64,
    pub funding_epoch: u64,
    pub slot_last: u64,
    pub current_slot: u64,
    pub assets: [AssetStateV13; V13_MAX_PORTFOLIO_ASSETS_N],
    pub bankruptcy_hlock_active: bool,
    pub threshold_stress_active: bool,
    pub active_bankrupt_close_present: bool,
    pub loss_stale_active: bool,
    pub recovery_reason: Option<PermissionlessRecoveryReasonV13>,
    pub mode: MarketModeV13,
    pub resolved_slot: u64,
    pub payout_snapshot: u128,
    pub payout_snapshot_pnl_pos_tot: u128,
    pub payout_snapshot_captured: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccrueAssetOutcomeV13 {
    pub dt: u64,
    pub price_move_active: bool,
    pub funding_active: bool,
    pub equity_active: bool,
    pub loss_stale_after: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TradeRequestV13 {
    pub asset_index: usize,
    pub size_q: u128,
    pub exec_price: u64,
    pub fee_bps: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TradeOutcomeV13 {
    pub fee_a: u128,
    pub fee_b: u128,
    pub notional: u128,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiquidationRequestV13 {
    pub asset_index: usize,
    pub close_q: u128,
    pub fee_bps: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiquidationOutcomeV13 {
    pub closed_q: u128,
    pub insurance_used: u128,
    pub residual_booked: u128,
    pub explicit_loss: u128,
    pub fee_charged: u128,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebalanceRequestV13 {
    pub asset_index: usize,
    pub reduce_q: u128,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebalanceOutcomeV13 {
    pub reduced_q: u128,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BResidualBookingOutcomeV13 {
    pub booked_loss: u128,
    pub explicit_loss: u128,
    pub delta_b: u128,
    pub remaining_after: u128,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuantityAdlOutcomeV13 {
    pub closed_q: u128,
    pub opposite_a_after: u128,
    pub reset_started: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionlessCrankActionV13 {
    Refresh,
    SettleB { asset_index: usize },
    Liquidate(LiquidationRequestV13),
    Recover(PermissionlessRecoveryReasonV13),
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermissionlessCrankRequestV13 {
    pub now_slot: u64,
    pub asset_index: usize,
    pub effective_price: u64,
    pub funding_rate_e9: i128,
    pub action: PermissionlessCrankActionV13,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedCloseOutcomeV13 {
    ProgressOnly,
    Closed { payout: u128 },
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct V13PodU16 {
    pub bytes: [u8; 2],
}

impl V13PodU16 {
    pub fn new(value: u16) -> Self {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    pub fn get(self) -> u16 {
        u16::from_le_bytes(self.bytes)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct V13PodU32 {
    pub bytes: [u8; 4],
}

impl V13PodU32 {
    pub fn new(value: u32) -> Self {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    pub fn get(self) -> u32 {
        u32::from_le_bytes(self.bytes)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct V13PodU64 {
    pub bytes: [u8; 8],
}

impl V13PodU64 {
    pub fn new(value: u64) -> Self {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    pub fn get(self) -> u64 {
        u64::from_le_bytes(self.bytes)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct V13PodU128 {
    pub bytes: [u8; 16],
}

impl V13PodU128 {
    pub fn new(value: u128) -> Self {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    pub fn get(self) -> u128 {
        u128::from_le_bytes(self.bytes)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct V13PodI128 {
    pub bytes: [u8; 16],
}

impl V13PodI128 {
    pub fn new(value: i128) -> Self {
        Self {
            bytes: value.to_le_bytes(),
        }
    }

    pub fn get(self) -> i128 {
        i128::from_le_bytes(self.bytes)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct V13OptionalRecoveryReasonAccount {
    pub present: u8,
    pub value: u8,
}

impl V13OptionalRecoveryReasonAccount {
    pub fn from_runtime(value: Option<PermissionlessRecoveryReasonV13>) -> Self {
        match value {
            Some(reason) => Self {
                present: 1,
                value: encode_recovery_reason(reason),
            },
            None => Self {
                present: 0,
                value: 0,
            },
        }
    }

    pub fn try_to_runtime(self) -> V13Result<Option<PermissionlessRecoveryReasonV13>> {
        match self.present {
            0 if self.value == 0 => Ok(None),
            1 => Ok(Some(decode_recovery_reason(self.value)?)),
            _ => Err(V13Error::InvalidConfig),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct ProvenanceHeaderV13Account {
    pub market_group_id: [u8; 32],
    pub portfolio_account_id: [u8; 32],
    pub owner: [u8; 32],
    pub version: V13PodU16,
    pub layout_discriminator: V13PodU16,
}

impl ProvenanceHeaderV13Account {
    pub fn from_runtime(value: &ProvenanceHeaderV13) -> Self {
        Self {
            market_group_id: value.market_group_id,
            portfolio_account_id: value.portfolio_account_id,
            owner: value.owner,
            version: V13PodU16::new(value.version),
            layout_discriminator: V13PodU16::new(value.layout_discriminator),
        }
    }

    pub fn try_to_runtime(&self) -> V13Result<ProvenanceHeaderV13> {
        let out = ProvenanceHeaderV13 {
            market_group_id: self.market_group_id,
            portfolio_account_id: self.portfolio_account_id,
            owner: self.owner,
            version: self.version.get(),
            layout_discriminator: self.layout_discriminator.get(),
        };
        if out.version != V13_ACCOUNT_VERSION
            || out.layout_discriminator != V13_LAYOUT_DISCRIMINATOR
        {
            return Err(V13Error::ProvenanceMismatch);
        }
        Ok(out)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct V13ConfigAccount {
    pub max_portfolio_assets: u8,
    pub min_nonzero_mm_req: V13PodU128,
    pub min_nonzero_im_req: V13PodU128,
    pub h_min: V13PodU64,
    pub h_max: V13PodU64,
    pub maintenance_margin_bps: V13PodU64,
    pub initial_margin_bps: V13PodU64,
    pub max_trading_fee_bps: V13PodU64,
    pub liquidation_fee_bps: V13PodU64,
    pub liquidation_fee_cap: V13PodU128,
    pub min_liquidation_abs: V13PodU128,
    pub max_accrual_dt_slots: V13PodU64,
    pub max_abs_funding_e9_per_slot: V13PodU64,
    pub min_funding_lifetime_slots: V13PodU64,
    pub max_price_move_bps_per_slot: V13PodU64,
    pub max_account_b_settlement_chunks: V13PodU64,
    pub max_bankrupt_close_chunks: V13PodU64,
    pub public_b_chunk_atoms: V13PodU128,
    pub permissionless_recovery_enabled: u8,
    pub stale_certificate_penalty_enabled: u8,
    pub full_refresh_required_for_favorable_actions: u8,
    pub public_liveness_profile_crank_forward: u8,
}

impl V13ConfigAccount {
    pub fn from_runtime(value: &V13Config) -> Self {
        Self {
            max_portfolio_assets: value.max_portfolio_assets,
            min_nonzero_mm_req: V13PodU128::new(value.min_nonzero_mm_req),
            min_nonzero_im_req: V13PodU128::new(value.min_nonzero_im_req),
            h_min: V13PodU64::new(value.h_min),
            h_max: V13PodU64::new(value.h_max),
            maintenance_margin_bps: V13PodU64::new(value.maintenance_margin_bps),
            initial_margin_bps: V13PodU64::new(value.initial_margin_bps),
            max_trading_fee_bps: V13PodU64::new(value.max_trading_fee_bps),
            liquidation_fee_bps: V13PodU64::new(value.liquidation_fee_bps),
            liquidation_fee_cap: V13PodU128::new(value.liquidation_fee_cap),
            min_liquidation_abs: V13PodU128::new(value.min_liquidation_abs),
            max_accrual_dt_slots: V13PodU64::new(value.max_accrual_dt_slots),
            max_abs_funding_e9_per_slot: V13PodU64::new(value.max_abs_funding_e9_per_slot),
            min_funding_lifetime_slots: V13PodU64::new(value.min_funding_lifetime_slots),
            max_price_move_bps_per_slot: V13PodU64::new(value.max_price_move_bps_per_slot),
            max_account_b_settlement_chunks: V13PodU64::new(value.max_account_b_settlement_chunks),
            max_bankrupt_close_chunks: V13PodU64::new(value.max_bankrupt_close_chunks),
            public_b_chunk_atoms: V13PodU128::new(value.public_b_chunk_atoms),
            permissionless_recovery_enabled: encode_bool(value.permissionless_recovery_enabled),
            stale_certificate_penalty_enabled: encode_bool(value.stale_certificate_penalty_enabled),
            full_refresh_required_for_favorable_actions: encode_bool(
                value.full_refresh_required_for_favorable_actions,
            ),
            public_liveness_profile_crank_forward: encode_bool(
                value.public_liveness_profile_crank_forward,
            ),
        }
    }

    pub fn try_to_runtime(&self) -> V13Result<V13Config> {
        let out = V13Config {
            max_portfolio_assets: self.max_portfolio_assets,
            min_nonzero_mm_req: self.min_nonzero_mm_req.get(),
            min_nonzero_im_req: self.min_nonzero_im_req.get(),
            h_min: self.h_min.get(),
            h_max: self.h_max.get(),
            maintenance_margin_bps: self.maintenance_margin_bps.get(),
            initial_margin_bps: self.initial_margin_bps.get(),
            max_trading_fee_bps: self.max_trading_fee_bps.get(),
            liquidation_fee_bps: self.liquidation_fee_bps.get(),
            liquidation_fee_cap: self.liquidation_fee_cap.get(),
            min_liquidation_abs: self.min_liquidation_abs.get(),
            max_accrual_dt_slots: self.max_accrual_dt_slots.get(),
            max_abs_funding_e9_per_slot: self.max_abs_funding_e9_per_slot.get(),
            min_funding_lifetime_slots: self.min_funding_lifetime_slots.get(),
            max_price_move_bps_per_slot: self.max_price_move_bps_per_slot.get(),
            max_account_b_settlement_chunks: self.max_account_b_settlement_chunks.get(),
            max_bankrupt_close_chunks: self.max_bankrupt_close_chunks.get(),
            public_b_chunk_atoms: self.public_b_chunk_atoms.get(),
            permissionless_recovery_enabled: decode_bool(self.permissionless_recovery_enabled)?,
            stale_certificate_penalty_enabled: decode_bool(self.stale_certificate_penalty_enabled)?,
            full_refresh_required_for_favorable_actions: decode_bool(
                self.full_refresh_required_for_favorable_actions,
            )?,
            public_liveness_profile_crank_forward: decode_bool(
                self.public_liveness_profile_crank_forward,
            )?,
        };
        out.validate_public_user_fund()?;
        Ok(out)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct AssetStateV13Account {
    pub raw_oracle_target_price: V13PodU64,
    pub effective_price: V13PodU64,
    pub fund_px_last: V13PodU64,
    pub a_long: V13PodU128,
    pub a_short: V13PodU128,
    pub k_long: V13PodI128,
    pub k_short: V13PodI128,
    pub f_long_num: V13PodI128,
    pub f_short_num: V13PodI128,
    pub k_epoch_start_long: V13PodI128,
    pub k_epoch_start_short: V13PodI128,
    pub f_epoch_start_long_num: V13PodI128,
    pub f_epoch_start_short_num: V13PodI128,
    pub b_long_num: V13PodU128,
    pub b_short_num: V13PodU128,
    pub b_epoch_start_long_num: V13PodU128,
    pub b_epoch_start_short_num: V13PodU128,
    pub oi_eff_long_q: V13PodU128,
    pub oi_eff_short_q: V13PodU128,
    pub stored_pos_count_long: V13PodU64,
    pub stored_pos_count_short: V13PodU64,
    pub stale_account_count_long: V13PodU64,
    pub stale_account_count_short: V13PodU64,
    pub loss_weight_sum_long: V13PodU128,
    pub loss_weight_sum_short: V13PodU128,
    pub social_loss_remainder_long_num: V13PodU128,
    pub social_loss_remainder_short_num: V13PodU128,
    pub social_loss_dust_long_num: V13PodU128,
    pub social_loss_dust_short_num: V13PodU128,
    pub explicit_unallocated_loss_long: V13PodU128,
    pub explicit_unallocated_loss_short: V13PodU128,
    pub epoch_long: V13PodU64,
    pub epoch_short: V13PodU64,
    pub mode_long: u8,
    pub mode_short: u8,
}

impl AssetStateV13Account {
    pub fn from_runtime(value: &AssetStateV13) -> Self {
        Self {
            raw_oracle_target_price: V13PodU64::new(value.raw_oracle_target_price),
            effective_price: V13PodU64::new(value.effective_price),
            fund_px_last: V13PodU64::new(value.fund_px_last),
            a_long: V13PodU128::new(value.a_long),
            a_short: V13PodU128::new(value.a_short),
            k_long: V13PodI128::new(value.k_long),
            k_short: V13PodI128::new(value.k_short),
            f_long_num: V13PodI128::new(value.f_long_num),
            f_short_num: V13PodI128::new(value.f_short_num),
            k_epoch_start_long: V13PodI128::new(value.k_epoch_start_long),
            k_epoch_start_short: V13PodI128::new(value.k_epoch_start_short),
            f_epoch_start_long_num: V13PodI128::new(value.f_epoch_start_long_num),
            f_epoch_start_short_num: V13PodI128::new(value.f_epoch_start_short_num),
            b_long_num: V13PodU128::new(value.b_long_num),
            b_short_num: V13PodU128::new(value.b_short_num),
            b_epoch_start_long_num: V13PodU128::new(value.b_epoch_start_long_num),
            b_epoch_start_short_num: V13PodU128::new(value.b_epoch_start_short_num),
            oi_eff_long_q: V13PodU128::new(value.oi_eff_long_q),
            oi_eff_short_q: V13PodU128::new(value.oi_eff_short_q),
            stored_pos_count_long: V13PodU64::new(value.stored_pos_count_long),
            stored_pos_count_short: V13PodU64::new(value.stored_pos_count_short),
            stale_account_count_long: V13PodU64::new(value.stale_account_count_long),
            stale_account_count_short: V13PodU64::new(value.stale_account_count_short),
            loss_weight_sum_long: V13PodU128::new(value.loss_weight_sum_long),
            loss_weight_sum_short: V13PodU128::new(value.loss_weight_sum_short),
            social_loss_remainder_long_num: V13PodU128::new(value.social_loss_remainder_long_num),
            social_loss_remainder_short_num: V13PodU128::new(value.social_loss_remainder_short_num),
            social_loss_dust_long_num: V13PodU128::new(value.social_loss_dust_long_num),
            social_loss_dust_short_num: V13PodU128::new(value.social_loss_dust_short_num),
            explicit_unallocated_loss_long: V13PodU128::new(value.explicit_unallocated_loss_long),
            explicit_unallocated_loss_short: V13PodU128::new(value.explicit_unallocated_loss_short),
            epoch_long: V13PodU64::new(value.epoch_long),
            epoch_short: V13PodU64::new(value.epoch_short),
            mode_long: encode_side_mode(value.mode_long),
            mode_short: encode_side_mode(value.mode_short),
        }
    }

    pub fn try_to_runtime(&self) -> V13Result<AssetStateV13> {
        let out = AssetStateV13 {
            raw_oracle_target_price: self.raw_oracle_target_price.get(),
            effective_price: self.effective_price.get(),
            fund_px_last: self.fund_px_last.get(),
            a_long: self.a_long.get(),
            a_short: self.a_short.get(),
            k_long: self.k_long.get(),
            k_short: self.k_short.get(),
            f_long_num: self.f_long_num.get(),
            f_short_num: self.f_short_num.get(),
            k_epoch_start_long: self.k_epoch_start_long.get(),
            k_epoch_start_short: self.k_epoch_start_short.get(),
            f_epoch_start_long_num: self.f_epoch_start_long_num.get(),
            f_epoch_start_short_num: self.f_epoch_start_short_num.get(),
            b_long_num: self.b_long_num.get(),
            b_short_num: self.b_short_num.get(),
            b_epoch_start_long_num: self.b_epoch_start_long_num.get(),
            b_epoch_start_short_num: self.b_epoch_start_short_num.get(),
            oi_eff_long_q: self.oi_eff_long_q.get(),
            oi_eff_short_q: self.oi_eff_short_q.get(),
            stored_pos_count_long: self.stored_pos_count_long.get(),
            stored_pos_count_short: self.stored_pos_count_short.get(),
            stale_account_count_long: self.stale_account_count_long.get(),
            stale_account_count_short: self.stale_account_count_short.get(),
            loss_weight_sum_long: self.loss_weight_sum_long.get(),
            loss_weight_sum_short: self.loss_weight_sum_short.get(),
            social_loss_remainder_long_num: self.social_loss_remainder_long_num.get(),
            social_loss_remainder_short_num: self.social_loss_remainder_short_num.get(),
            social_loss_dust_long_num: self.social_loss_dust_long_num.get(),
            social_loss_dust_short_num: self.social_loss_dust_short_num.get(),
            explicit_unallocated_loss_long: self.explicit_unallocated_loss_long.get(),
            explicit_unallocated_loss_short: self.explicit_unallocated_loss_short.get(),
            epoch_long: self.epoch_long.get(),
            epoch_short: self.epoch_short.get(),
            mode_long: decode_side_mode(self.mode_long)?,
            mode_short: decode_side_mode(self.mode_short)?,
        };
        validate_non_min_i128(out.k_long)?;
        validate_non_min_i128(out.k_short)?;
        validate_non_min_i128(out.f_long_num)?;
        validate_non_min_i128(out.f_short_num)?;
        validate_non_min_i128(out.k_epoch_start_long)?;
        validate_non_min_i128(out.k_epoch_start_short)?;
        validate_non_min_i128(out.f_epoch_start_long_num)?;
        validate_non_min_i128(out.f_epoch_start_short_num)?;
        Ok(out)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct PortfolioLegV13Account {
    pub active: u8,
    pub side: u8,
    pub basis_pos_q: V13PodI128,
    pub a_basis: V13PodU128,
    pub k_snap: V13PodI128,
    pub f_snap: V13PodI128,
    pub epoch_snap: V13PodU64,
    pub loss_weight: V13PodU128,
    pub b_snap: V13PodU128,
    pub b_rem: V13PodU128,
    pub b_epoch_snap: V13PodU64,
    pub b_stale: u8,
    pub stale: u8,
}

impl PortfolioLegV13Account {
    pub fn from_runtime(value: &PortfolioLegV13) -> Self {
        Self {
            active: encode_bool(value.active),
            side: encode_side(value.side),
            basis_pos_q: V13PodI128::new(value.basis_pos_q),
            a_basis: V13PodU128::new(value.a_basis),
            k_snap: V13PodI128::new(value.k_snap),
            f_snap: V13PodI128::new(value.f_snap),
            epoch_snap: V13PodU64::new(value.epoch_snap),
            loss_weight: V13PodU128::new(value.loss_weight),
            b_snap: V13PodU128::new(value.b_snap),
            b_rem: V13PodU128::new(value.b_rem),
            b_epoch_snap: V13PodU64::new(value.b_epoch_snap),
            b_stale: encode_bool(value.b_stale),
            stale: encode_bool(value.stale),
        }
    }

    pub fn try_to_runtime(&self) -> V13Result<PortfolioLegV13> {
        let out = PortfolioLegV13 {
            active: decode_bool(self.active)?,
            side: decode_side(self.side)?,
            basis_pos_q: self.basis_pos_q.get(),
            a_basis: self.a_basis.get(),
            k_snap: self.k_snap.get(),
            f_snap: self.f_snap.get(),
            epoch_snap: self.epoch_snap.get(),
            loss_weight: self.loss_weight.get(),
            b_snap: self.b_snap.get(),
            b_rem: self.b_rem.get(),
            b_epoch_snap: self.b_epoch_snap.get(),
            b_stale: decode_bool(self.b_stale)?,
            stale: decode_bool(self.stale)?,
        };
        if out.active {
            validate_active_leg(out)?;
        } else if out != PortfolioLegV13::EMPTY {
            return Err(V13Error::HiddenLeg);
        }
        Ok(out)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct HealthCertV13Account {
    pub certified_equity: V13PodI128,
    pub certified_initial_req: V13PodU128,
    pub certified_maintenance_req: V13PodU128,
    pub certified_liq_deficit: V13PodU128,
    pub certified_worst_case_loss: V13PodU128,
    pub cert_oracle_epoch: V13PodU64,
    pub cert_funding_epoch: V13PodU64,
    pub cert_risk_epoch: V13PodU64,
    pub active_bitmap_at_cert: V13PodU32,
    pub valid: u8,
}

impl HealthCertV13Account {
    pub fn from_runtime(value: &HealthCertV13) -> Self {
        Self {
            certified_equity: V13PodI128::new(value.certified_equity),
            certified_initial_req: V13PodU128::new(value.certified_initial_req),
            certified_maintenance_req: V13PodU128::new(value.certified_maintenance_req),
            certified_liq_deficit: V13PodU128::new(value.certified_liq_deficit),
            certified_worst_case_loss: V13PodU128::new(value.certified_worst_case_loss),
            cert_oracle_epoch: V13PodU64::new(value.cert_oracle_epoch),
            cert_funding_epoch: V13PodU64::new(value.cert_funding_epoch),
            cert_risk_epoch: V13PodU64::new(value.cert_risk_epoch),
            active_bitmap_at_cert: V13PodU32::new(value.active_bitmap_at_cert),
            valid: encode_bool(value.valid),
        }
    }

    pub fn try_to_runtime(&self) -> V13Result<HealthCertV13> {
        let out = HealthCertV13 {
            certified_equity: self.certified_equity.get(),
            certified_initial_req: self.certified_initial_req.get(),
            certified_maintenance_req: self.certified_maintenance_req.get(),
            certified_liq_deficit: self.certified_liq_deficit.get(),
            certified_worst_case_loss: self.certified_worst_case_loss.get(),
            cert_oracle_epoch: self.cert_oracle_epoch.get(),
            cert_funding_epoch: self.cert_funding_epoch.get(),
            cert_risk_epoch: self.cert_risk_epoch.get(),
            active_bitmap_at_cert: self.active_bitmap_at_cert.get(),
            valid: decode_bool(self.valid)?,
        };
        validate_non_min_i128(out.certified_equity)?;
        Ok(out)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct PortfolioAccountV13Account {
    pub provenance_header: ProvenanceHeaderV13Account,
    pub owner: [u8; 32],
    pub capital: V13PodU128,
    pub pnl: V13PodI128,
    pub reserved_pnl: V13PodU128,
    pub fee_credits: V13PodI128,
    pub last_fee_slot: V13PodU64,
    pub active_bitmap: V13PodU32,
    pub legs: [PortfolioLegV13Account; V13_MAX_PORTFOLIO_ASSETS_N],
    pub health_cert: HealthCertV13Account,
    pub stale_state: u8,
    pub b_stale_state: u8,
    pub rebalance_lock: u8,
    pub liquidation_lock: u8,
}

impl PortfolioAccountV13Account {
    #[cfg(not(target_os = "solana"))]
    pub fn try_empty(header: ProvenanceHeaderV13Account) -> V13Result<Self> {
        Ok(Self::from_runtime(&PortfolioAccountV13::empty(
            header.try_to_runtime()?,
        )))
    }

    #[cfg(not(target_os = "solana"))]
    pub fn from_runtime(value: &PortfolioAccountV13) -> Self {
        let mut legs = [PortfolioLegV13Account::default(); V13_MAX_PORTFOLIO_ASSETS_N];
        let mut i = 0;
        while i < V13_MAX_PORTFOLIO_ASSETS_N {
            legs[i] = PortfolioLegV13Account::from_runtime(&value.legs[i]);
            i += 1;
        }
        Self {
            provenance_header: ProvenanceHeaderV13Account::from_runtime(&value.provenance_header),
            owner: value.owner,
            capital: V13PodU128::new(value.capital),
            pnl: V13PodI128::new(value.pnl),
            reserved_pnl: V13PodU128::new(value.reserved_pnl),
            fee_credits: V13PodI128::new(value.fee_credits),
            last_fee_slot: V13PodU64::new(value.last_fee_slot),
            active_bitmap: V13PodU32::new(value.active_bitmap),
            legs,
            health_cert: HealthCertV13Account::from_runtime(&value.health_cert),
            stale_state: encode_bool(value.stale_state),
            b_stale_state: encode_bool(value.b_stale_state),
            rebalance_lock: encode_bool(value.rebalance_lock),
            liquidation_lock: encode_bool(value.liquidation_lock),
        }
    }

    #[cfg(not(target_os = "solana"))]
    pub fn try_to_runtime(&self) -> V13Result<PortfolioAccountV13> {
        let mut legs = [PortfolioLegV13::EMPTY; V13_MAX_PORTFOLIO_ASSETS_N];
        let mut i = 0;
        while i < V13_MAX_PORTFOLIO_ASSETS_N {
            legs[i] = self.legs[i].try_to_runtime()?;
            i += 1;
        }
        let out = PortfolioAccountV13 {
            provenance_header: self.provenance_header.try_to_runtime()?,
            owner: self.owner,
            capital: self.capital.get(),
            pnl: self.pnl.get(),
            reserved_pnl: self.reserved_pnl.get(),
            fee_credits: self.fee_credits.get(),
            last_fee_slot: self.last_fee_slot.get(),
            active_bitmap: self.active_bitmap.get(),
            legs,
            health_cert: self.health_cert.try_to_runtime()?,
            stale_state: decode_bool(self.stale_state)?,
            b_stale_state: decode_bool(self.b_stale_state)?,
            rebalance_lock: decode_bool(self.rebalance_lock)?,
            liquidation_lock: decode_bool(self.liquidation_lock)?,
        };
        if out.provenance_header.owner != out.owner {
            return Err(V13Error::ProvenanceMismatch);
        }
        validate_non_min_i128(out.pnl)?;
        validate_fee_credits(out.fee_credits)?;
        if out.reserved_pnl > out.pnl.max(0) as u128 {
            return Err(V13Error::InvalidLeg);
        }
        Ok(out)
    }

    #[cfg(not(target_os = "solana"))]
    pub fn validate_with_market(&self, market: &MarketGroupV13) -> V13Result<PortfolioAccountV13> {
        let out = self.try_to_runtime()?;
        market.validate_account_shape(&out)?;
        Ok(out)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, bytemuck::Zeroable, bytemuck::Pod)]
pub struct MarketGroupV13Account {
    pub market_group_id: [u8; 32],
    pub config: V13ConfigAccount,
    pub vault: V13PodU128,
    pub insurance: V13PodU128,
    pub c_tot: V13PodU128,
    pub pnl_pos_tot: V13PodU128,
    pub pnl_matured_pos_tot: V13PodU128,
    pub materialized_portfolio_count: V13PodU64,
    pub stale_certificate_count: V13PodU64,
    pub b_stale_account_count: V13PodU64,
    pub negative_pnl_account_count: V13PodU64,
    pub risk_epoch: V13PodU64,
    pub oracle_epoch: V13PodU64,
    pub funding_epoch: V13PodU64,
    pub slot_last: V13PodU64,
    pub current_slot: V13PodU64,
    pub assets: [AssetStateV13Account; V13_MAX_PORTFOLIO_ASSETS_N],
    pub bankruptcy_hlock_active: u8,
    pub threshold_stress_active: u8,
    pub active_bankrupt_close_present: u8,
    pub loss_stale_active: u8,
    pub recovery_reason: V13OptionalRecoveryReasonAccount,
    pub mode: u8,
    pub resolved_slot: V13PodU64,
    pub payout_snapshot: V13PodU128,
    pub payout_snapshot_pnl_pos_tot: V13PodU128,
    pub payout_snapshot_captured: u8,
}

impl MarketGroupV13Account {
    #[cfg(not(target_os = "solana"))]
    pub fn from_runtime(value: &MarketGroupV13) -> Self {
        let mut assets = [AssetStateV13Account::default(); V13_MAX_PORTFOLIO_ASSETS_N];
        let mut i = 0;
        while i < V13_MAX_PORTFOLIO_ASSETS_N {
            assets[i] = AssetStateV13Account::from_runtime(&value.assets[i]);
            i += 1;
        }
        Self {
            market_group_id: value.market_group_id,
            config: V13ConfigAccount::from_runtime(&value.config),
            vault: V13PodU128::new(value.vault),
            insurance: V13PodU128::new(value.insurance),
            c_tot: V13PodU128::new(value.c_tot),
            pnl_pos_tot: V13PodU128::new(value.pnl_pos_tot),
            pnl_matured_pos_tot: V13PodU128::new(value.pnl_matured_pos_tot),
            materialized_portfolio_count: V13PodU64::new(value.materialized_portfolio_count),
            stale_certificate_count: V13PodU64::new(value.stale_certificate_count),
            b_stale_account_count: V13PodU64::new(value.b_stale_account_count),
            negative_pnl_account_count: V13PodU64::new(value.negative_pnl_account_count),
            risk_epoch: V13PodU64::new(value.risk_epoch),
            oracle_epoch: V13PodU64::new(value.oracle_epoch),
            funding_epoch: V13PodU64::new(value.funding_epoch),
            slot_last: V13PodU64::new(value.slot_last),
            current_slot: V13PodU64::new(value.current_slot),
            assets,
            bankruptcy_hlock_active: encode_bool(value.bankruptcy_hlock_active),
            threshold_stress_active: encode_bool(value.threshold_stress_active),
            active_bankrupt_close_present: encode_bool(value.active_bankrupt_close_present),
            loss_stale_active: encode_bool(value.loss_stale_active),
            recovery_reason: V13OptionalRecoveryReasonAccount::from_runtime(value.recovery_reason),
            mode: encode_market_mode(value.mode),
            resolved_slot: V13PodU64::new(value.resolved_slot),
            payout_snapshot: V13PodU128::new(value.payout_snapshot),
            payout_snapshot_pnl_pos_tot: V13PodU128::new(value.payout_snapshot_pnl_pos_tot),
            payout_snapshot_captured: encode_bool(value.payout_snapshot_captured),
        }
    }

    #[cfg(not(target_os = "solana"))]
    pub fn try_to_runtime(&self) -> V13Result<MarketGroupV13> {
        let mut assets = [AssetStateV13::default(); V13_MAX_PORTFOLIO_ASSETS_N];
        let mut i = 0;
        while i < V13_MAX_PORTFOLIO_ASSETS_N {
            assets[i] = self.assets[i].try_to_runtime()?;
            i += 1;
        }
        let out = MarketGroupV13 {
            market_group_id: self.market_group_id,
            config: self.config.try_to_runtime()?,
            vault: self.vault.get(),
            insurance: self.insurance.get(),
            c_tot: self.c_tot.get(),
            pnl_pos_tot: self.pnl_pos_tot.get(),
            pnl_matured_pos_tot: self.pnl_matured_pos_tot.get(),
            materialized_portfolio_count: self.materialized_portfolio_count.get(),
            stale_certificate_count: self.stale_certificate_count.get(),
            b_stale_account_count: self.b_stale_account_count.get(),
            negative_pnl_account_count: self.negative_pnl_account_count.get(),
            risk_epoch: self.risk_epoch.get(),
            oracle_epoch: self.oracle_epoch.get(),
            funding_epoch: self.funding_epoch.get(),
            slot_last: self.slot_last.get(),
            current_slot: self.current_slot.get(),
            assets,
            bankruptcy_hlock_active: decode_bool(self.bankruptcy_hlock_active)?,
            threshold_stress_active: decode_bool(self.threshold_stress_active)?,
            active_bankrupt_close_present: decode_bool(self.active_bankrupt_close_present)?,
            loss_stale_active: decode_bool(self.loss_stale_active)?,
            recovery_reason: self.recovery_reason.try_to_runtime()?,
            mode: decode_market_mode(self.mode)?,
            resolved_slot: self.resolved_slot.get(),
            payout_snapshot: self.payout_snapshot.get(),
            payout_snapshot_pnl_pos_tot: self.payout_snapshot_pnl_pos_tot.get(),
            payout_snapshot_captured: decode_bool(self.payout_snapshot_captured)?,
        };
        out.assert_public_invariants()?;
        Ok(out)
    }

    #[cfg(not(target_os = "solana"))]
    pub fn validate(&self) -> V13Result<MarketGroupV13> {
        self.try_to_runtime()
    }
}

impl MarketGroupV13 {
    #[cfg(not(target_os = "solana"))]
    pub fn new(market_group_id: [u8; 32], config: V13Config) -> V13Result<Self> {
        config.validate_public_user_fund()?;
        Ok(Self {
            market_group_id,
            config,
            vault: 0,
            insurance: 0,
            c_tot: 0,
            pnl_pos_tot: 0,
            pnl_matured_pos_tot: 0,
            materialized_portfolio_count: 0,
            stale_certificate_count: 0,
            b_stale_account_count: 0,
            negative_pnl_account_count: 0,
            risk_epoch: 0,
            oracle_epoch: 0,
            funding_epoch: 0,
            slot_last: 0,
            current_slot: 0,
            assets: [AssetStateV13::default(); V13_MAX_PORTFOLIO_ASSETS_N],
            bankruptcy_hlock_active: false,
            threshold_stress_active: false,
            active_bankrupt_close_present: false,
            loss_stale_active: false,
            recovery_reason: None,
            mode: MarketModeV13::Live,
            resolved_slot: 0,
            payout_snapshot: 0,
            payout_snapshot_pnl_pos_tot: 0,
            payout_snapshot_captured: false,
        })
    }

    pub fn validate_portfolio_account_provenance(
        &self,
        account: &PortfolioAccountV13,
    ) -> V13Result<()> {
        let h = account.provenance_header;
        if h.market_group_id != self.market_group_id
            || h.owner != account.owner
            || h.version != V13_ACCOUNT_VERSION
            || h.layout_discriminator != V13_LAYOUT_DISCRIMINATOR
        {
            return Err(V13Error::ProvenanceMismatch);
        }
        Ok(())
    }

    pub fn validate_account_shape(&self, account: &PortfolioAccountV13) -> V13Result<()> {
        self.validate_portfolio_account_provenance(account)?;
        validate_non_min_i128(account.pnl)?;
        validate_fee_credits(account.fee_credits)?;
        if account.reserved_pnl > account.pnl.max(0) as u128 {
            return Err(V13Error::InvalidLeg);
        }

        let n = self.config.max_portfolio_assets as usize;
        for i in 0..V13_MAX_PORTFOLIO_ASSETS_N {
            let bit = ((account.active_bitmap >> i) & 1) != 0;
            let leg = account.legs[i];
            if i >= n {
                if bit || leg != PortfolioLegV13::default() {
                    return Err(V13Error::HiddenLeg);
                }
                continue;
            }

            if bit != leg.active {
                return Err(V13Error::HiddenLeg);
            }
            if !leg.active {
                if leg != PortfolioLegV13::EMPTY {
                    return Err(V13Error::HiddenLeg);
                }
            } else {
                validate_active_leg(leg)?;
            }
        }
        Ok(())
    }

    pub fn create_portfolio_account(&mut self, account: &PortfolioAccountV13) -> V13Result<()> {
        self.validate_account_shape(account)?;
        self.materialized_portfolio_count = self
            .materialized_portfolio_count
            .checked_add(1)
            .ok_or(V13Error::CounterOverflow)?;
        Ok(())
    }

    pub fn close_portfolio_account(&mut self, account: &PortfolioAccountV13) -> V13Result<()> {
        self.validate_account_shape(account)?;
        if account.active_bitmap != 0
            || account.capital != 0
            || account.pnl != 0
            || account.reserved_pnl != 0
            || account.fee_credits != 0
            || account.stale_state
            || account.b_stale_state
        {
            return Err(V13Error::LockActive);
        }
        self.materialized_portfolio_count = self
            .materialized_portfolio_count
            .checked_sub(1)
            .ok_or(V13Error::CounterUnderflow)?;
        Ok(())
    }

    pub fn deposit_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        amount: u128,
    ) -> V13Result<()> {
        self.validate_account_shape(account)?;
        if amount == 0 {
            return Ok(());
        }
        account.capital = account
            .capital
            .checked_add(amount)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.c_tot = self
            .c_tot
            .checked_add(amount)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.vault = self
            .vault
            .checked_add(amount)
            .ok_or(V13Error::ArithmeticOverflow)?;
        account.health_cert.valid = false;
        self.assert_public_invariants()
    }

    pub fn settle_negative_pnl_from_principal(
        &mut self,
        account: &mut PortfolioAccountV13,
    ) -> V13Result<u128> {
        self.validate_account_shape(account)?;
        if account.pnl >= 0 {
            return Ok(0);
        }
        let loss = account.pnl.unsigned_abs();
        let paid = account.capital.min(loss);
        if paid == 0 {
            self.bankruptcy_hlock_active = true;
            return Ok(0);
        }
        account.capital -= paid;
        self.c_tot = self
            .c_tot
            .checked_sub(paid)
            .ok_or(V13Error::CounterUnderflow)?;
        let paid_i128 = i128::try_from(paid).map_err(|_| V13Error::ArithmeticOverflow)?;
        let new_pnl = account
            .pnl
            .checked_add(paid_i128)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.set_account_pnl(account, new_pnl)?;
        if account.pnl < 0 {
            self.bankruptcy_hlock_active = true;
        }
        account.health_cert.valid = false;
        self.assert_public_invariants()?;
        Ok(paid)
    }

    pub fn charge_account_fee_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        requested_fee: u128,
    ) -> V13Result<u128> {
        if self.mode != MarketModeV13::Live {
            return Err(V13Error::LockActive);
        }
        self.charge_account_fee_after_loss_settlement(account, requested_fee)
    }

    fn charge_account_fee_after_loss_settlement(
        &mut self,
        account: &mut PortfolioAccountV13,
        requested_fee: u128,
    ) -> V13Result<u128> {
        self.settle_account_side_effects_not_atomic(account, self.config.public_b_chunk_atoms)?;
        if account.b_stale_state || has_b_stale_leg(account) {
            return Err(V13Error::BStale);
        }
        self.settle_negative_pnl_from_principal(account)?;
        if requested_fee == 0 || account.pnl < 0 {
            return Ok(0);
        }
        let charged = requested_fee.min(account.capital);
        if charged == 0 {
            return Ok(0);
        }
        account.capital -= charged;
        self.c_tot = self
            .c_tot
            .checked_sub(charged)
            .ok_or(V13Error::CounterUnderflow)?;
        self.insurance = self
            .insurance
            .checked_add(charged)
            .ok_or(V13Error::ArithmeticOverflow)?;
        account.health_cert.valid = false;
        self.assert_public_invariants()?;
        Ok(charged)
    }

    pub fn sync_account_fee_to_slot_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        now_slot: u64,
        fee_rate_per_slot: u128,
    ) -> V13Result<u128> {
        self.validate_account_shape(account)?;
        if now_slot < account.last_fee_slot {
            return Err(V13Error::Stale);
        }
        let nonflat = account.active_bitmap != 0;
        let fee_anchor = if self.mode == MarketModeV13::Live && nonflat && now_slot > self.slot_last
        {
            self.slot_last
        } else if self.mode == MarketModeV13::Resolved {
            self.resolved_slot
        } else {
            now_slot
        };
        if fee_anchor <= account.last_fee_slot {
            return Ok(0);
        }
        let dt = fee_anchor - account.last_fee_slot;
        let raw_fee = U256::from_u128(fee_rate_per_slot)
            .checked_mul(U256::from_u64(dt))
            .ok_or(V13Error::ArithmeticOverflow)?;
        let requested_fee = raw_fee.try_into_u128().unwrap_or(u128::MAX);
        let charged = self.charge_account_fee_after_loss_settlement(account, requested_fee)?;
        account.last_fee_slot = fee_anchor;
        Ok(charged)
    }

    pub fn convert_released_pnl_to_capital_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
    ) -> V13Result<u128> {
        self.ensure_favorable_action_allowed(account)?;
        let pos = account.pnl.max(0) as u128;
        let released = if self.mode == MarketModeV13::Resolved {
            pos
        } else {
            pos.saturating_sub(account.reserved_pnl)
        };
        if released == 0 {
            return Ok(0);
        }
        let residual = self.residual();
        let converted = released.min(residual);
        if converted == 0 {
            return Err(V13Error::LockActive);
        }
        let converted_i128 = i128::try_from(converted).map_err(|_| V13Error::ArithmeticOverflow)?;
        let new_pnl = account
            .pnl
            .checked_sub(converted_i128)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.set_account_pnl(account, new_pnl)?;
        account.capital = account
            .capital
            .checked_add(converted)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.c_tot = self
            .c_tot
            .checked_add(converted)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.pnl_matured_pos_tot = self.pnl_matured_pos_tot.saturating_sub(converted);
        account.health_cert.valid = false;
        self.assert_public_invariants()?;
        Ok(converted)
    }

    pub fn withdraw_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        amount: u128,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<()> {
        if amount == 0 {
            return Ok(());
        }
        self.settle_account_side_effects_not_atomic(account, self.config.public_b_chunk_atoms)?;
        self.full_account_refresh(account, effective_prices)?;
        let locked = self.h_lock_lane(Some(account), false)? == HLockLaneV13::HMax;
        if self.loss_stale_active && account.active_bitmap != 0 {
            return Err(V13Error::LockActive);
        }
        if self.account_has_target_effective_lag(account)? && account.active_bitmap != 0 {
            return Err(V13Error::LockActive);
        }
        self.settle_negative_pnl_from_principal(account)?;
        if account.pnl < 0 || amount > account.capital {
            return Err(V13Error::LockActive);
        }
        let post_capital = account.capital - amount;
        let equity_after = if locked {
            account_no_positive_credit_equity_with_capital(account, post_capital)?
        } else {
            account_equity_with_capital(account, post_capital)?
        };
        if equity_after < 0 {
            return Err(V13Error::InvalidConfig);
        }
        let equity_after_u = equity_after as u128;
        if equity_after_u < account.health_cert.certified_initial_req {
            return Err(V13Error::InvalidConfig);
        }
        account.capital = post_capital;
        self.c_tot = self
            .c_tot
            .checked_sub(amount)
            .ok_or(V13Error::CounterUnderflow)?;
        self.vault = self
            .vault
            .checked_sub(amount)
            .ok_or(V13Error::CounterUnderflow)?;
        account.health_cert.valid = false;
        self.assert_public_invariants()
    }

    pub fn mark_account_stale(&mut self, account: &mut PortfolioAccountV13) -> V13Result<()> {
        self.validate_portfolio_account_provenance(account)?;
        if !account.stale_state {
            account.stale_state = true;
            account.health_cert.valid = false;
            self.stale_certificate_count = self
                .stale_certificate_count
                .checked_add(1)
                .ok_or(V13Error::CounterOverflow)?;
        }
        Ok(())
    }

    pub fn clear_account_stale(&mut self, account: &mut PortfolioAccountV13) -> V13Result<()> {
        self.validate_portfolio_account_provenance(account)?;
        if account.stale_state {
            account.stale_state = false;
            self.stale_certificate_count = self
                .stale_certificate_count
                .checked_sub(1)
                .ok_or(V13Error::CounterUnderflow)?;
        }
        Ok(())
    }

    pub fn mark_account_b_stale(&mut self, account: &mut PortfolioAccountV13) -> V13Result<()> {
        self.validate_portfolio_account_provenance(account)?;
        if !account.b_stale_state {
            account.b_stale_state = true;
            account.health_cert.valid = false;
            self.b_stale_account_count = self
                .b_stale_account_count
                .checked_add(1)
                .ok_or(V13Error::CounterOverflow)?;
        }
        Ok(())
    }

    pub fn clear_account_b_stale(&mut self, account: &mut PortfolioAccountV13) -> V13Result<()> {
        self.validate_portfolio_account_provenance(account)?;
        if has_b_stale_leg(account) {
            return Err(V13Error::BStale);
        }
        if account.b_stale_state {
            account.b_stale_state = false;
            self.b_stale_account_count = self
                .b_stale_account_count
                .checked_sub(1)
                .ok_or(V13Error::CounterUnderflow)?;
        }
        Ok(())
    }

    pub fn attach_leg(
        &mut self,
        account: &mut PortfolioAccountV13,
        asset_index: usize,
        side: SideV13,
        basis_pos_q: i128,
    ) -> V13Result<()> {
        self.validate_portfolio_account_provenance(account)?;
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        if account.legs[asset_index].active || ((account.active_bitmap >> asset_index) & 1) != 0 {
            return Err(V13Error::InvalidLeg);
        }
        validate_basis(basis_pos_q)?;

        let asset = self.assets[asset_index];
        let (a_basis, k_snap, f_snap, b_snap, epoch_snap) = match side {
            SideV13::Long => (
                asset.a_long,
                asset.k_long,
                asset.f_long_num,
                asset.b_long_num,
                asset.epoch_long,
            ),
            SideV13::Short => (
                asset.a_short,
                asset.k_short,
                asset.f_short_num,
                asset.b_short_num,
                asset.epoch_short,
            ),
        };
        if !(MIN_A_SIDE..=ADL_ONE).contains(&a_basis) {
            return Err(V13Error::InvalidLeg);
        }
        let loss_weight = loss_weight_for_basis(basis_pos_q.unsigned_abs(), a_basis)?;
        if loss_weight == 0 {
            return Err(V13Error::InvalidLeg);
        }

        let asset = &mut self.assets[asset_index];
        match side {
            SideV13::Long => {
                asset.stored_pos_count_long = asset
                    .stored_pos_count_long
                    .checked_add(1)
                    .ok_or(V13Error::CounterOverflow)?;
                asset.oi_eff_long_q = asset
                    .oi_eff_long_q
                    .checked_add(basis_pos_q.unsigned_abs())
                    .ok_or(V13Error::ArithmeticOverflow)?;
                asset.loss_weight_sum_long = asset
                    .loss_weight_sum_long
                    .checked_add(loss_weight)
                    .ok_or(V13Error::ArithmeticOverflow)?;
            }
            SideV13::Short => {
                asset.stored_pos_count_short = asset
                    .stored_pos_count_short
                    .checked_add(1)
                    .ok_or(V13Error::CounterOverflow)?;
                asset.oi_eff_short_q = asset
                    .oi_eff_short_q
                    .checked_add(basis_pos_q.unsigned_abs())
                    .ok_or(V13Error::ArithmeticOverflow)?;
                asset.loss_weight_sum_short = asset
                    .loss_weight_sum_short
                    .checked_add(loss_weight)
                    .ok_or(V13Error::ArithmeticOverflow)?;
            }
        }
        account.legs[asset_index] = PortfolioLegV13 {
            active: true,
            side,
            basis_pos_q,
            a_basis,
            k_snap,
            f_snap,
            epoch_snap,
            loss_weight,
            b_snap,
            b_rem: 0,
            b_epoch_snap: epoch_snap,
            b_stale: false,
            stale: false,
        };
        account.active_bitmap |= 1u32 << asset_index;
        account.health_cert.valid = false;
        self.validate_account_shape(account)
    }

    pub fn clear_leg(
        &mut self,
        account: &mut PortfolioAccountV13,
        asset_index: usize,
    ) -> V13Result<()> {
        self.validate_account_shape(account)?;
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        let leg = account.legs[asset_index];
        if !leg.active || leg.b_stale || leg.stale {
            return Err(V13Error::InvalidLeg);
        }
        let asset = &mut self.assets[asset_index];
        let prior_reset_epoch = match leg.side {
            SideV13::Long => {
                asset.mode_long == SideModeV13::ResetPending
                    && leg.epoch_snap.checked_add(1) == Some(asset.epoch_long)
            }
            SideV13::Short => {
                asset.mode_short == SideModeV13::ResetPending
                    && leg.epoch_snap.checked_add(1) == Some(asset.epoch_short)
            }
        };
        match leg.side {
            SideV13::Long => {
                asset.stored_pos_count_long = asset
                    .stored_pos_count_long
                    .checked_sub(1)
                    .ok_or(V13Error::CounterUnderflow)?;
                if !prior_reset_epoch {
                    asset.oi_eff_long_q = asset
                        .oi_eff_long_q
                        .checked_sub(leg.basis_pos_q.unsigned_abs())
                        .ok_or(V13Error::CounterUnderflow)?;
                    asset.loss_weight_sum_long = asset
                        .loss_weight_sum_long
                        .checked_sub(leg.loss_weight)
                        .ok_or(V13Error::CounterUnderflow)?;
                }
            }
            SideV13::Short => {
                asset.stored_pos_count_short = asset
                    .stored_pos_count_short
                    .checked_sub(1)
                    .ok_or(V13Error::CounterUnderflow)?;
                if !prior_reset_epoch {
                    asset.oi_eff_short_q = asset
                        .oi_eff_short_q
                        .checked_sub(leg.basis_pos_q.unsigned_abs())
                        .ok_or(V13Error::CounterUnderflow)?;
                    asset.loss_weight_sum_short = asset
                        .loss_weight_sum_short
                        .checked_sub(leg.loss_weight)
                        .ok_or(V13Error::CounterUnderflow)?;
                }
            }
        }
        account.legs[asset_index] = PortfolioLegV13::EMPTY;
        account.active_bitmap &= !(1u32 << asset_index);
        account.health_cert.valid = false;
        self.validate_account_shape(account)
    }

    pub fn mark_leg_b_stale(
        &mut self,
        account: &mut PortfolioAccountV13,
        asset_index: usize,
    ) -> V13Result<()> {
        self.validate_account_shape(account)?;
        if asset_index >= self.config.max_portfolio_assets as usize
            || !account.legs[asset_index].active
        {
            return Err(V13Error::InvalidLeg);
        }
        account.legs[asset_index].b_stale = true;
        self.mark_account_b_stale(account)
    }

    pub fn h_lock_lane(
        &self,
        account: Option<&PortfolioAccountV13>,
        instruction_bankruptcy_candidate: bool,
    ) -> V13Result<HLockLaneV13> {
        if let Some(account) = account {
            self.validate_portfolio_account_provenance(account)?;
            if account.stale_state || account.b_stale_state {
                return Ok(HLockLaneV13::HMax);
            }
        }

        if self.threshold_stress_active
            || self.bankruptcy_hlock_active
            || instruction_bankruptcy_candidate
            || self.loss_stale_active
            || self.active_bankrupt_close_present
        {
            return Ok(HLockLaneV13::HMax);
        }

        Ok(HLockLaneV13::HMin)
    }

    pub fn select_h_lock(
        &self,
        account: Option<&PortfolioAccountV13>,
        instruction_bankruptcy_candidate: bool,
    ) -> V13Result<u64> {
        match self.h_lock_lane(account, instruction_bankruptcy_candidate)? {
            HLockLaneV13::HMin => Ok(self.config.h_min),
            HLockLaneV13::HMax => Ok(self.config.h_max),
        }
    }

    fn asset_has_target_effective_lag(&self, asset_index: usize) -> V13Result<bool> {
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        let asset = self.assets[asset_index];
        Ok(asset.raw_oracle_target_price != asset.effective_price)
    }

    fn account_has_target_effective_lag(&self, account: &PortfolioAccountV13) -> V13Result<bool> {
        self.validate_account_shape(account)?;
        for i in 0..self.config.max_portfolio_assets as usize {
            if account.legs[i].active && self.asset_has_target_effective_lag(i)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn full_account_refresh(
        &mut self,
        account: &mut PortfolioAccountV13,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<HealthCertV13> {
        self.validate_account_shape(account)?;
        let n = self.config.max_portfolio_assets as usize;
        for i in 0..n {
            if !account.legs[i].active {
                continue;
            }
            self.settle_leg_kf_effects(account, i)?;
            if self.b_target_for_leg(i, account.legs[i])? > account.legs[i].b_snap {
                self.mark_leg_b_stale(account, i)?;
            }
        }
        if account.b_stale_state {
            return Err(V13Error::BStale);
        }
        if account.stale_state {
            self.clear_account_stale(account)?;
        }

        let mut initial_req = 0u128;
        let mut maintenance_req = 0u128;
        let mut worst_case_loss = 0u128;
        for i in 0..n {
            if !account.legs[i].active {
                continue;
            }
            let price = effective_prices[i];
            if price == 0 || price > MAX_ORACLE_PRICE {
                return Err(V13Error::InvalidConfig);
            }
            let risk_notional =
                risk_notional_ceil(account.legs[i].basis_pos_q.unsigned_abs(), price)?;
            let leg_initial = margin_requirement(
                risk_notional,
                self.config.initial_margin_bps,
                self.config.min_nonzero_im_req,
            )?;
            let leg_maintenance = margin_requirement(
                risk_notional,
                self.config.maintenance_margin_bps,
                self.config.min_nonzero_mm_req,
            )?;
            initial_req = initial_req
                .checked_add(leg_initial)
                .ok_or(V13Error::ArithmeticOverflow)?;
            maintenance_req = maintenance_req
                .checked_add(leg_maintenance)
                .ok_or(V13Error::ArithmeticOverflow)?;
            worst_case_loss = worst_case_loss
                .checked_add(risk_notional)
                .ok_or(V13Error::ArithmeticOverflow)?;
        }

        let equity = account_equity(account)?;
        let certified_liq_deficit = if equity < 0 {
            equity.unsigned_abs()
        } else {
            let e = equity as u128;
            maintenance_req.saturating_sub(e)
        };
        let cert = HealthCertV13 {
            certified_equity: equity,
            certified_initial_req: initial_req,
            certified_maintenance_req: maintenance_req,
            certified_liq_deficit,
            certified_worst_case_loss: worst_case_loss,
            cert_oracle_epoch: self.oracle_epoch,
            cert_funding_epoch: self.funding_epoch,
            cert_risk_epoch: self.risk_epoch,
            active_bitmap_at_cert: account.active_bitmap,
            valid: true,
        };
        account.health_cert = cert;
        Ok(cert)
    }

    pub fn ensure_favorable_action_allowed(&self, account: &PortfolioAccountV13) -> V13Result<()> {
        self.validate_account_shape(account)?;
        if self.h_lock_lane(Some(account), false)? == HLockLaneV13::HMax {
            return Err(V13Error::LockActive);
        }
        if !account.health_cert.valid
            || account.health_cert.cert_oracle_epoch != self.oracle_epoch
            || account.health_cert.cert_funding_epoch != self.funding_epoch
            || account.health_cert.cert_risk_epoch != self.risk_epoch
            || account.health_cert.active_bitmap_at_cert != account.active_bitmap
        {
            return Err(V13Error::Stale);
        }
        if self.account_has_target_effective_lag(account)? {
            return Err(V13Error::LockActive);
        }
        Ok(())
    }

    pub fn account_b_settlement_chunk(
        &self,
        account: &PortfolioAccountV13,
        asset_index: usize,
        endpoint_delta_budget: u128,
    ) -> V13Result<AccountBSettlementChunkV13> {
        self.validate_account_shape(account)?;
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        let leg = account.legs[asset_index];
        if !leg.active {
            return Err(V13Error::InvalidLeg);
        }
        let target = self.b_target_for_leg(asset_index, leg)?;
        if target < leg.b_snap {
            return Err(V13Error::RecoveryRequired);
        }
        let b_remaining = target - leg.b_snap;
        if b_remaining == 0 {
            return Ok(AccountBSettlementChunkV13 {
                delta_b: 0,
                loss: 0,
                new_remainder: leg.b_rem,
                remaining_after: 0,
            });
        }
        if leg.loss_weight == 0 || endpoint_delta_budget == 0 {
            return Err(V13Error::RecoveryRequired);
        }

        let limit = self.config.public_b_chunk_atoms;
        let max_num = limit
            .checked_add(1)
            .and_then(|v| v.checked_mul(SOCIAL_LOSS_DEN))
            .and_then(|v| v.checked_sub(1))
            .ok_or(V13Error::ArithmeticOverflow)?;
        if leg.b_rem > max_num {
            return Err(V13Error::RecoveryRequired);
        }
        let max_delta_by_loss = (max_num - leg.b_rem) / leg.loss_weight;
        let delta_b = b_remaining
            .min(max_delta_by_loss)
            .min(endpoint_delta_budget);
        if delta_b == 0 {
            return Err(V13Error::RecoveryRequired);
        }
        let num = leg
            .loss_weight
            .checked_mul(delta_b)
            .and_then(|v| v.checked_add(leg.b_rem))
            .ok_or(V13Error::ArithmeticOverflow)?;
        let loss = num / SOCIAL_LOSS_DEN;
        let new_remainder = num % SOCIAL_LOSS_DEN;
        Ok(AccountBSettlementChunkV13 {
            delta_b,
            loss,
            new_remainder,
            remaining_after: b_remaining - delta_b,
        })
    }

    pub fn settle_account_b_chunk(
        &mut self,
        account: &mut PortfolioAccountV13,
        asset_index: usize,
        endpoint_delta_budget: u128,
    ) -> V13Result<AccountBSettlementChunkV13> {
        let chunk = self.account_b_settlement_chunk(account, asset_index, endpoint_delta_budget)?;
        if chunk.delta_b == 0 {
            if !has_b_stale_leg(account) {
                self.clear_account_b_stale(account)?;
            }
            return Ok(chunk);
        }
        let old_pnl = account.pnl;
        let loss_i128 = i128::try_from(chunk.loss).map_err(|_| V13Error::ArithmeticOverflow)?;
        let new_pnl = old_pnl
            .checked_sub(loss_i128)
            .ok_or(V13Error::ArithmeticOverflow)?;

        {
            let leg = &mut account.legs[asset_index];
            leg.b_snap = leg
                .b_snap
                .checked_add(chunk.delta_b)
                .ok_or(V13Error::ArithmeticOverflow)?;
            leg.b_rem = chunk.new_remainder;
            leg.b_stale = chunk.remaining_after != 0;
        }
        self.set_account_pnl(account, new_pnl)?;
        if chunk.remaining_after != 0 {
            self.mark_account_b_stale(account)?;
        } else if !has_b_stale_leg(account) {
            self.clear_account_b_stale(account)?;
        }
        account.health_cert.valid = false;
        self.validate_account_shape(account)?;
        Ok(chunk)
    }

    pub fn settle_account_side_effects_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        b_delta_budget: u128,
    ) -> V13Result<PermissionlessProgressOutcomeV13> {
        self.validate_account_shape(account)?;
        let n = self.config.max_portfolio_assets as usize;
        for i in 0..n {
            if !account.legs[i].active {
                continue;
            }
            self.settle_leg_kf_effects(account, i)?;
            let target = self.b_target_for_leg(i, account.legs[i])?;
            if target > account.legs[i].b_snap {
                self.mark_leg_b_stale(account, i)?;
                let chunk = self.settle_account_b_chunk(account, i, b_delta_budget)?;
                if chunk.remaining_after != 0 {
                    return Ok(PermissionlessProgressOutcomeV13::AccountBChunk(chunk));
                }
            }
        }
        self.settle_negative_pnl_from_principal(account)?;
        account.health_cert.valid = false;
        Ok(PermissionlessProgressOutcomeV13::AccountCurrent)
    }

    pub fn accrue_asset_to_not_atomic(
        &mut self,
        asset_index: usize,
        now_slot: u64,
        effective_price: u64,
        funding_rate_e9: i128,
        protective_progress_committed: bool,
    ) -> V13Result<AccrueAssetOutcomeV13> {
        if self.mode != MarketModeV13::Live || self.active_bankrupt_close_present {
            return Err(V13Error::LockActive);
        }
        if asset_index >= self.config.max_portfolio_assets as usize
            || effective_price == 0
            || effective_price > MAX_ORACLE_PRICE
            || funding_rate_e9.unsigned_abs() > self.config.max_abs_funding_e9_per_slot as u128
            || now_slot < self.slot_last
        {
            return Err(V13Error::InvalidConfig);
        }
        let dt_total = now_slot - self.slot_last;
        let segment_dt = if dt_total > self.config.max_accrual_dt_slots {
            self.config.max_accrual_dt_slots
        } else {
            dt_total
        };
        let old = self.assets[asset_index];
        let exposed = old.oi_eff_long_q != 0 || old.oi_eff_short_q != 0;
        let balanced_exposure = old.oi_eff_long_q != 0 && old.oi_eff_short_q != 0;
        let price_move_active = effective_price != old.effective_price && exposed;
        let funding_active =
            segment_dt > 0 && funding_rate_e9 != 0 && balanced_exposure && old.fund_px_last > 0;
        let equity_active = price_move_active || funding_active;
        if equity_active {
            if segment_dt == 0 {
                return Err(V13Error::NonProgress);
            }
            let price_diff = effective_price.abs_diff(old.effective_price) as u128;
            let lhs = price_diff
                .checked_mul(MAX_MARGIN_BPS as u128)
                .ok_or(V13Error::ArithmeticOverflow)?;
            let rhs = (self.config.max_price_move_bps_per_slot as u128)
                .checked_mul(segment_dt as u128)
                .and_then(|v| v.checked_mul(old.effective_price as u128))
                .ok_or(V13Error::ArithmeticOverflow)?;
            if lhs > rhs {
                return Err(V13Error::RecoveryRequired);
            }
            if !protective_progress_committed {
                return Err(V13Error::NonProgress);
            }
        }

        let price_delta = effective_price as i128 - old.effective_price as i128;
        let k_delta = checked_i128_mul(price_delta, ADL_ONE as i128)?;
        let funding_delta = if funding_active {
            let n = funding_rate_e9
                .checked_mul(segment_dt as i128)
                .and_then(|v| v.checked_mul(effective_price as i128))
                .ok_or(V13Error::ArithmeticOverflow)?;
            floor_div_signed_conservative_i128(n, FUNDING_DEN)
                .checked_mul(ADL_ONE as i128)
                .ok_or(V13Error::ArithmeticOverflow)?
        } else {
            0
        };

        let asset = &mut self.assets[asset_index];
        asset.k_long = add_non_min_i128(asset.k_long, k_delta)?;
        asset.k_short = add_non_min_i128(asset.k_short, -k_delta)?;
        asset.f_long_num = add_non_min_i128(asset.f_long_num, -funding_delta)?;
        asset.f_short_num = add_non_min_i128(asset.f_short_num, funding_delta)?;
        asset.effective_price = effective_price;
        asset.fund_px_last = effective_price;
        self.current_slot = now_slot;
        self.slot_last = self
            .slot_last
            .checked_add(segment_dt)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.loss_stale_active = self.slot_last < now_slot;
        if price_move_active {
            self.oracle_epoch = self
                .oracle_epoch
                .checked_add(1)
                .ok_or(V13Error::CounterOverflow)?;
        }
        if funding_active {
            self.funding_epoch = self
                .funding_epoch
                .checked_add(1)
                .ok_or(V13Error::CounterOverflow)?;
        }
        self.assert_public_invariants()?;
        Ok(AccrueAssetOutcomeV13 {
            dt: segment_dt,
            price_move_active,
            funding_active,
            equity_active,
            loss_stale_after: self.loss_stale_active,
        })
    }

    #[cfg(not(target_os = "solana"))]
    pub fn execute_trade_with_fee_not_atomic(
        &mut self,
        long_account: &mut PortfolioAccountV13,
        short_account: &mut PortfolioAccountV13,
        request: TradeRequestV13,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<TradeOutcomeV13> {
        let mut staged_group = *self;
        let mut staged_long = *long_account;
        let mut staged_short = *short_account;
        let outcome = staged_group.execute_trade_with_fee_inner(
            &mut staged_long,
            &mut staged_short,
            request,
            effective_prices,
        )?;
        *self = staged_group;
        *long_account = staged_long;
        *short_account = staged_short;
        Ok(outcome)
    }

    pub fn execute_trade_with_fee_in_place_not_atomic(
        &mut self,
        long_account: &mut PortfolioAccountV13,
        short_account: &mut PortfolioAccountV13,
        request: TradeRequestV13,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<TradeOutcomeV13> {
        self.execute_trade_with_fee_inner(long_account, short_account, request, effective_prices)
    }

    fn execute_trade_with_fee_inner(
        &mut self,
        long_account: &mut PortfolioAccountV13,
        short_account: &mut PortfolioAccountV13,
        request: TradeRequestV13,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<TradeOutcomeV13> {
        if request.asset_index >= self.config.max_portfolio_assets as usize
            || request.size_q == 0
            || request.size_q > MAX_TRADE_SIZE_Q
            || request.exec_price == 0
            || request.exec_price > MAX_ORACLE_PRICE
            || request.fee_bps > self.config.max_trading_fee_bps
        {
            return Err(V13Error::InvalidConfig);
        }
        if self.mode != MarketModeV13::Live {
            return Err(V13Error::LockActive);
        }
        self.settle_account_side_effects_not_atomic(
            long_account,
            self.config.public_b_chunk_atoms,
        )?;
        self.settle_account_side_effects_not_atomic(
            short_account,
            self.config.public_b_chunk_atoms,
        )?;
        self.full_account_refresh(long_account, effective_prices)?;
        self.full_account_refresh(short_account, effective_prices)?;

        let long_delta =
            i128::try_from(request.size_q).map_err(|_| V13Error::ArithmeticOverflow)?;
        let short_delta = long_delta
            .checked_neg()
            .ok_or(V13Error::ArithmeticOverflow)?;
        let locked = self.h_lock_lane(Some(long_account), false)? == HLockLaneV13::HMax
            || self.h_lock_lane(Some(short_account), false)? == HLockLaneV13::HMax;
        let risk_increasing =
            position_delta_increases_risk(long_account, request.asset_index, long_delta)?
                || position_delta_increases_risk(short_account, request.asset_index, short_delta)?;
        let target_effective_lag = self.asset_has_target_effective_lag(request.asset_index)?;
        if risk_increasing && (locked || target_effective_lag) {
            return Err(V13Error::LockActive);
        }

        let notional = trade_notional_floor(request.size_q, request.exec_price)?;
        let fee = checked_fee_bps(notional, request.fee_bps)?;
        self.charge_account_fee_not_atomic(long_account, fee)?;
        self.charge_account_fee_not_atomic(short_account, fee)?;
        self.apply_position_delta(long_account, request.asset_index, long_delta)?;
        self.apply_position_delta(short_account, request.asset_index, short_delta)?;
        self.full_account_refresh(long_account, effective_prices)?;
        self.full_account_refresh(short_account, effective_prices)?;
        ensure_initial_margin(long_account)?;
        ensure_initial_margin(short_account)?;
        if locked {
            ensure_no_positive_credit_initial_margin(long_account)?;
            ensure_no_positive_credit_initial_margin(short_account)?;
        }
        self.assert_public_invariants()?;
        Ok(TradeOutcomeV13 {
            fee_a: fee,
            fee_b: fee,
            notional,
        })
    }

    pub fn liquidate_account_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        request: LiquidationRequestV13,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<LiquidationOutcomeV13> {
        if request.asset_index >= self.config.max_portfolio_assets as usize
            || request.close_q == 0
            || request.fee_bps
                > self
                    .config
                    .liquidation_fee_bps
                    .max(self.config.max_trading_fee_bps)
        {
            return Err(V13Error::InvalidConfig);
        }
        self.settle_account_side_effects_not_atomic(account, self.config.public_b_chunk_atoms)?;
        self.full_account_refresh(account, effective_prices)?;
        if account.health_cert.certified_liq_deficit == 0 {
            return Err(V13Error::NonProgress);
        }
        let before = *account;
        let leg = account.legs[request.asset_index];
        if !leg.active {
            return Err(V13Error::InvalidLeg);
        }
        let close_q = request.close_q.min(leg.basis_pos_q.unsigned_abs());
        self.preflight_liquidation_residual_durability(request.asset_index, leg.side, account)?;
        let fee_notional = risk_notional_ceil(close_q, effective_prices[request.asset_index])?;
        let fee = checked_fee_bps(fee_notional, request.fee_bps)?
            .max(self.config.min_liquidation_abs)
            .min(self.config.liquidation_fee_cap);
        let charged_fee = self.charge_account_fee_not_atomic(account, fee)?;
        self.reduce_position(account, request.asset_index, close_q)?;
        self.settle_negative_pnl_from_principal(account)?;
        let insurance_used = self.consume_insurance_for_negative_pnl(account)?;
        let residual = if account.pnl < 0 {
            account.pnl.unsigned_abs()
        } else {
            0
        };
        let mut booked = 0u128;
        let mut explicit = 0u128;
        if residual != 0 {
            let bankrupt_side = leg.side;
            let outcome =
                self.book_bankruptcy_residual_chunk(request.asset_index, bankrupt_side, residual)?;
            booked = outcome.booked_loss;
            explicit = outcome.explicit_loss;
            let cleared = booked
                .checked_add(explicit)
                .ok_or(V13Error::ArithmeticOverflow)?
                .min(residual);
            let cleared_i128 = i128::try_from(cleared).map_err(|_| V13Error::ArithmeticOverflow)?;
            self.set_account_pnl(
                account,
                account
                    .pnl
                    .checked_add(cleared_i128)
                    .ok_or(V13Error::ArithmeticOverflow)?,
            )?;
            self.bankruptcy_hlock_active = true;
        }
        self.full_account_refresh(account, effective_prices)?;
        self.validate_liquidation_progress(&before, account)?;
        self.assert_public_invariants()?;
        Ok(LiquidationOutcomeV13 {
            closed_q: close_q,
            insurance_used,
            residual_booked: booked,
            explicit_loss: explicit,
            fee_charged: charged_fee,
        })
    }

    pub fn rebalance_reduce_position_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        request: RebalanceRequestV13,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<RebalanceOutcomeV13> {
        if request.asset_index >= self.config.max_portfolio_assets as usize || request.reduce_q == 0
        {
            return Err(V13Error::InvalidConfig);
        }
        self.settle_account_side_effects_not_atomic(account, self.config.public_b_chunk_atoms)?;
        self.full_account_refresh(account, effective_prices)?;
        let before = *account;
        let leg = account.legs[request.asset_index];
        if !leg.active {
            return Err(V13Error::InvalidLeg);
        }
        let reduce_q = request.reduce_q.min(leg.basis_pos_q.unsigned_abs());
        if reduce_q == 0 {
            return Err(V13Error::NonProgress);
        }
        self.reduce_position(account, request.asset_index, reduce_q)?;
        self.settle_negative_pnl_from_principal(account)?;
        self.full_account_refresh(account, effective_prices)?;
        self.validate_liquidation_progress(&before, account)?;
        self.assert_public_invariants()?;
        Ok(RebalanceOutcomeV13 {
            reduced_q: reduce_q,
        })
    }

    pub fn permissionless_crank_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        request: PermissionlessCrankRequestV13,
        effective_prices: &[u64; V13_MAX_PORTFOLIO_ASSETS_N],
    ) -> V13Result<PermissionlessProgressOutcomeV13> {
        self.validate_account_shape(account)?;
        let protective_progress = match request.action {
            PermissionlessCrankActionV13::Refresh => {
                if let PermissionlessProgressOutcomeV13::AccountBChunk(out) = self
                    .settle_account_side_effects_not_atomic(
                        account,
                        self.config.public_b_chunk_atoms,
                    )?
                {
                    self.assert_public_invariants()?;
                    return Ok(PermissionlessProgressOutcomeV13::AccountBChunk(out));
                }
                self.full_account_refresh(account, effective_prices)?;
                true
            }
            PermissionlessCrankActionV13::SettleB { asset_index } => {
                let out = self.settle_account_b_chunk(
                    account,
                    asset_index,
                    self.config.public_b_chunk_atoms,
                )?;
                return Ok(PermissionlessProgressOutcomeV13::AccountBChunk(out));
            }
            PermissionlessCrankActionV13::Liquidate(liq) => {
                self.liquidate_account_not_atomic(account, liq, effective_prices)?;
                true
            }
            PermissionlessCrankActionV13::Recover(reason) => {
                return self.declare_permissionless_recovery(reason);
            }
        };
        self.accrue_asset_to_not_atomic(
            request.asset_index,
            request.now_slot,
            request.effective_price,
            request.funding_rate_e9,
            protective_progress,
        )?;
        Ok(PermissionlessProgressOutcomeV13::AccountCurrent)
    }

    pub fn resolve_market_not_atomic(&mut self, resolved_slot: u64) -> V13Result<()> {
        if resolved_slot < self.current_slot {
            return Err(V13Error::Stale);
        }
        self.mode = MarketModeV13::Resolved;
        self.resolved_slot = resolved_slot;
        self.current_slot = resolved_slot;
        self.loss_stale_active = false;
        self.assert_public_invariants()
    }

    pub fn close_resolved_account_not_atomic(
        &mut self,
        account: &mut PortfolioAccountV13,
        fee_rate_per_slot: u128,
    ) -> V13Result<ResolvedCloseOutcomeV13> {
        if self.mode != MarketModeV13::Resolved {
            return Err(V13Error::LockActive);
        }
        if let PermissionlessProgressOutcomeV13::AccountBChunk(_) =
            self.settle_account_side_effects_not_atomic(account, self.config.public_b_chunk_atoms)?
        {
            self.assert_public_invariants()?;
            return Ok(ResolvedCloseOutcomeV13::ProgressOnly);
        }
        self.sync_account_fee_to_slot_not_atomic(account, self.resolved_slot, fee_rate_per_slot)?;
        self.settle_negative_pnl_from_principal(account)?;
        self.consume_insurance_for_negative_pnl(account)?;
        if account.active_bitmap != 0
            || account.pnl < 0
            || account.b_stale_state
            || account.stale_state
        {
            return Ok(ResolvedCloseOutcomeV13::ProgressOnly);
        }
        if account.pnl > 0 && !self.resolved_positive_payout_ready() {
            return Ok(ResolvedCloseOutcomeV13::ProgressOnly);
        }
        if !self.payout_snapshot_captured {
            self.payout_snapshot = self.residual();
            self.payout_snapshot_pnl_pos_tot = self.pnl_pos_tot;
            self.payout_snapshot_captured = true;
        }
        let pnl_payout = if account.pnl > 0 && self.payout_snapshot_pnl_pos_tot != 0 {
            wide_mul_div_floor_u128(
                account.pnl as u128,
                self.payout_snapshot,
                self.payout_snapshot_pnl_pos_tot,
            )
        } else {
            0
        };
        let payout = account
            .capital
            .checked_add(pnl_payout)
            .ok_or(V13Error::ArithmeticOverflow)?
            .min(self.vault);
        self.vault = self
            .vault
            .checked_sub(payout)
            .ok_or(V13Error::CounterUnderflow)?;
        self.c_tot = self.c_tot.saturating_sub(account.capital.min(self.c_tot));
        self.set_account_pnl(account, 0)?;
        account.capital = 0;
        account.reserved_pnl = 0;
        account.fee_credits = 0;
        account.health_cert.valid = false;
        self.assert_public_invariants()?;
        Ok(ResolvedCloseOutcomeV13::Closed { payout })
    }

    pub fn book_bankruptcy_residual_chunk(
        &mut self,
        asset_index: usize,
        bankrupt_side: SideV13,
        residual_remaining: u128,
    ) -> V13Result<BResidualBookingOutcomeV13> {
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        if residual_remaining == 0 {
            return Ok(BResidualBookingOutcomeV13 {
                booked_loss: 0,
                explicit_loss: 0,
                delta_b: 0,
                remaining_after: 0,
            });
        }
        let opp = opposite_side(bankrupt_side);
        let asset = self.assets[asset_index];
        let (b_now, weight_sum, rem) = match opp {
            SideV13::Long => (
                asset.b_long_num,
                asset.loss_weight_sum_long,
                asset.social_loss_remainder_long_num,
            ),
            SideV13::Short => (
                asset.b_short_num,
                asset.loss_weight_sum_short,
                asset.social_loss_remainder_short_num,
            ),
        };
        if weight_sum == 0 {
            let explicit_headroom = match opp {
                SideV13::Long => u128::MAX - asset.explicit_unallocated_loss_long,
                SideV13::Short => u128::MAX - asset.explicit_unallocated_loss_short,
            };
            if residual_remaining > explicit_headroom {
                self.declare_permissionless_recovery(
                    PermissionlessRecoveryReasonV13::ExplicitLossOrDustAuditOverflow,
                )?;
                return Err(V13Error::RecoveryRequired);
            }
            let asset = &mut self.assets[asset_index];
            match opp {
                SideV13::Long => {
                    asset.explicit_unallocated_loss_long = asset
                        .explicit_unallocated_loss_long
                        .checked_add(residual_remaining)
                        .ok_or(V13Error::ArithmeticOverflow)?;
                }
                SideV13::Short => {
                    asset.explicit_unallocated_loss_short = asset
                        .explicit_unallocated_loss_short
                        .checked_add(residual_remaining)
                        .ok_or(V13Error::ArithmeticOverflow)?;
                }
            }
            self.bankruptcy_hlock_active = true;
            return Ok(BResidualBookingOutcomeV13 {
                booked_loss: 0,
                explicit_loss: residual_remaining,
                delta_b: 0,
                remaining_after: 0,
            });
        }
        let engine_chunk = self.bankruptcy_residual_single_step_capacity(
            asset_index,
            bankrupt_side,
            residual_remaining,
        )?;
        if engine_chunk == 0 {
            self.declare_permissionless_recovery(
                PermissionlessRecoveryReasonV13::BIndexHeadroomExhausted,
            )?;
            return Err(V13Error::RecoveryRequired);
        }
        let numerator = engine_chunk
            .checked_mul(SOCIAL_LOSS_DEN)
            .and_then(|v| v.checked_add(rem))
            .ok_or(V13Error::ArithmeticOverflow)?;
        let delta_b = numerator / weight_sum;
        let new_rem = numerator % weight_sum;
        if delta_b == 0 || b_now.checked_add(delta_b).is_none() {
            self.declare_permissionless_recovery(
                PermissionlessRecoveryReasonV13::BIndexHeadroomExhausted,
            )?;
            return Err(V13Error::RecoveryRequired);
        }
        let asset = &mut self.assets[asset_index];
        match opp {
            SideV13::Long => {
                asset.b_long_num = asset
                    .b_long_num
                    .checked_add(delta_b)
                    .ok_or(V13Error::ArithmeticOverflow)?;
                asset.social_loss_remainder_long_num = new_rem;
            }
            SideV13::Short => {
                asset.b_short_num = asset
                    .b_short_num
                    .checked_add(delta_b)
                    .ok_or(V13Error::ArithmeticOverflow)?;
                asset.social_loss_remainder_short_num = new_rem;
            }
        }
        self.bankruptcy_hlock_active = true;
        Ok(BResidualBookingOutcomeV13 {
            booked_loss: engine_chunk,
            explicit_loss: 0,
            delta_b,
            remaining_after: residual_remaining - engine_chunk,
        })
    }

    pub fn apply_quantity_adl_after_residual_not_atomic(
        &mut self,
        asset_index: usize,
        bankrupt_side: SideV13,
        close_q: u128,
    ) -> V13Result<QuantityAdlOutcomeV13> {
        if asset_index >= self.config.max_portfolio_assets as usize || close_q == 0 {
            return Err(V13Error::InvalidLeg);
        }
        let opp = opposite_side(bankrupt_side);
        let asset = self.assets[asset_index];
        let (liq_oi_before, opp_oi_before, opp_a_before) = match (bankrupt_side, opp) {
            (SideV13::Long, SideV13::Short) => {
                (asset.oi_eff_long_q, asset.oi_eff_short_q, asset.a_short)
            }
            (SideV13::Short, SideV13::Long) => {
                (asset.oi_eff_short_q, asset.oi_eff_long_q, asset.a_long)
            }
            _ => unreachable!(),
        };
        if close_q > liq_oi_before || close_q > opp_oi_before {
            return Err(V13Error::InvalidLeg);
        }
        let liq_oi_after = liq_oi_before - close_q;
        let opp_oi_after = opp_oi_before - close_q;
        let mut reset_started = false;
        let mut opposite_a_after = if opp_oi_after == 0 {
            ADL_ONE
        } else {
            wide_mul_div_floor_u128(opp_a_before, opp_oi_after, opp_oi_before)
        };

        let force_full_reset = opp_oi_after != 0 && opposite_a_after == 0;
        let final_liq_oi_after = if force_full_reset { 0 } else { liq_oi_after };
        let final_opp_oi_after = if force_full_reset { 0 } else { opp_oi_after };
        if force_full_reset {
            opposite_a_after = ADL_ONE;
        }

        {
            let asset = &mut self.assets[asset_index];
            match bankrupt_side {
                SideV13::Long => asset.oi_eff_long_q = final_liq_oi_after,
                SideV13::Short => asset.oi_eff_short_q = final_liq_oi_after,
            }
            match opp {
                SideV13::Long => {
                    asset.oi_eff_long_q = final_opp_oi_after;
                    asset.a_long =
                        opposite_a_after.max(if final_opp_oi_after == 0 { ADL_ONE } else { 1 });
                    if final_opp_oi_after != 0 && asset.a_long < MIN_A_SIDE {
                        asset.mode_long = SideModeV13::DrainOnly;
                    }
                }
                SideV13::Short => {
                    asset.oi_eff_short_q = final_opp_oi_after;
                    asset.a_short =
                        opposite_a_after.max(if final_opp_oi_after == 0 { ADL_ONE } else { 1 });
                    if final_opp_oi_after != 0 && asset.a_short < MIN_A_SIDE {
                        asset.mode_short = SideModeV13::DrainOnly;
                    }
                }
            }
        }

        if final_liq_oi_after == 0 {
            self.begin_full_drain_reset(asset_index, bankrupt_side)?;
            reset_started = true;
        }
        if final_opp_oi_after == 0 {
            self.begin_full_drain_reset(asset_index, opp)?;
            reset_started = true;
        }
        self.assert_public_invariants()?;
        Ok(QuantityAdlOutcomeV13 {
            closed_q: close_q,
            opposite_a_after,
            reset_started,
        })
    }

    pub fn begin_full_drain_reset(&mut self, asset_index: usize, side: SideV13) -> V13Result<()> {
        if self.active_bankrupt_close_present
            || asset_index >= self.config.max_portfolio_assets as usize
        {
            return Err(V13Error::LockActive);
        }
        let asset = &mut self.assets[asset_index];
        match side {
            SideV13::Long => {
                if asset.oi_eff_long_q != 0 {
                    return Err(V13Error::InvalidLeg);
                }
                quarantine_remainder(
                    &mut asset.social_loss_remainder_long_num,
                    &mut asset.social_loss_dust_long_num,
                )?;
                asset.k_epoch_start_long = asset.k_long;
                asset.f_epoch_start_long_num = asset.f_long_num;
                asset.b_epoch_start_long_num = asset.b_long_num;
                asset.k_long = 0;
                asset.f_long_num = 0;
                asset.b_long_num = 0;
                asset.loss_weight_sum_long = 0;
                asset.a_long = ADL_ONE;
                asset.epoch_long = asset
                    .epoch_long
                    .checked_add(1)
                    .ok_or(V13Error::CounterOverflow)?;
                asset.mode_long = SideModeV13::ResetPending;
            }
            SideV13::Short => {
                if asset.oi_eff_short_q != 0 {
                    return Err(V13Error::InvalidLeg);
                }
                quarantine_remainder(
                    &mut asset.social_loss_remainder_short_num,
                    &mut asset.social_loss_dust_short_num,
                )?;
                asset.k_epoch_start_short = asset.k_short;
                asset.f_epoch_start_short_num = asset.f_short_num;
                asset.b_epoch_start_short_num = asset.b_short_num;
                asset.k_short = 0;
                asset.f_short_num = 0;
                asset.b_short_num = 0;
                asset.loss_weight_sum_short = 0;
                asset.a_short = ADL_ONE;
                asset.epoch_short = asset
                    .epoch_short
                    .checked_add(1)
                    .ok_or(V13Error::CounterOverflow)?;
                asset.mode_short = SideModeV13::ResetPending;
            }
        }
        self.risk_epoch = self
            .risk_epoch
            .checked_add(1)
            .ok_or(V13Error::CounterOverflow)?;
        self.assert_public_invariants()
    }

    pub fn finalize_ready_reset_side(
        &mut self,
        asset_index: usize,
        side: SideV13,
    ) -> V13Result<()> {
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        let asset = &mut self.assets[asset_index];
        match side {
            SideV13::Long => {
                if asset.mode_long != SideModeV13::ResetPending {
                    return Ok(());
                }
                if asset.stored_pos_count_long != 0 || asset.stale_account_count_long != 0 {
                    return Err(V13Error::Stale);
                }
                asset.mode_long = SideModeV13::Normal;
            }
            SideV13::Short => {
                if asset.mode_short != SideModeV13::ResetPending {
                    return Ok(());
                }
                if asset.stored_pos_count_short != 0 || asset.stale_account_count_short != 0 {
                    return Err(V13Error::Stale);
                }
                asset.mode_short = SideModeV13::Normal;
            }
        }
        self.assert_public_invariants()
    }

    pub fn risk_score(&self, account: &PortfolioAccountV13) -> V13Result<RiskScoreV13> {
        self.validate_account_shape(account)?;
        if !account.health_cert.valid {
            return Err(V13Error::Stale);
        }
        Ok(RiskScoreV13 {
            certified_liq_deficit: account.health_cert.certified_liq_deficit,
            unsettled_b_loss_bound: account_b_loss_bound(account)?,
            stale_loss_bound: if account.stale_state { 1 } else { 0 },
            gross_risk_notional: account.health_cert.certified_worst_case_loss,
            active_leg_count: account.active_bitmap.count_ones(),
        })
    }

    pub fn validate_liquidation_progress(
        &self,
        before: &PortfolioAccountV13,
        after: &PortfolioAccountV13,
    ) -> V13Result<()> {
        let before_score = self.risk_score(before)?;
        let after_score = self.risk_score(after)?;
        if after_score.strictly_reduces_from(before_score)
            || after_score.certified_liq_deficit < before_score.certified_liq_deficit
        {
            Ok(())
        } else {
            Err(V13Error::NonProgress)
        }
    }

    pub fn declare_permissionless_recovery(
        &mut self,
        reason: PermissionlessRecoveryReasonV13,
    ) -> V13Result<PermissionlessProgressOutcomeV13> {
        if !self.config.permissionless_recovery_enabled {
            return Err(V13Error::InvalidConfig);
        }
        self.recovery_reason = Some(reason);
        Ok(PermissionlessProgressOutcomeV13::RecoveryDeclared(reason))
    }

    pub fn assert_public_invariants(&self) -> V13Result<()> {
        if self.vault > MAX_VAULT_TVL {
            return Err(V13Error::InvalidConfig);
        }
        let senior = self
            .c_tot
            .checked_add(self.insurance)
            .ok_or(V13Error::ArithmeticOverflow)?;
        if self.c_tot > self.vault || self.insurance > self.vault || senior > self.vault {
            return Err(V13Error::InvalidConfig);
        }
        if self.pnl_matured_pos_tot > self.pnl_pos_tot {
            return Err(V13Error::InvalidConfig);
        }
        if self.slot_last > self.current_slot {
            return Err(V13Error::InvalidConfig);
        }
        for i in 0..self.config.max_portfolio_assets as usize {
            let asset = self.assets[i];
            if asset.effective_price == 0
                || asset.effective_price > MAX_ORACLE_PRICE
                || asset.raw_oracle_target_price == 0
                || asset.raw_oracle_target_price > MAX_ORACLE_PRICE
                || asset.fund_px_last == 0
                || asset.fund_px_last > MAX_ORACLE_PRICE
                || asset.k_long == i128::MIN
                || asset.k_short == i128::MIN
                || asset.f_long_num == i128::MIN
                || asset.f_short_num == i128::MIN
                || asset.k_epoch_start_long == i128::MIN
                || asset.k_epoch_start_short == i128::MIN
                || asset.f_epoch_start_long_num == i128::MIN
                || asset.f_epoch_start_short_num == i128::MIN
                || asset.oi_eff_long_q > crate::MAX_OI_SIDE_Q
                || asset.oi_eff_short_q > crate::MAX_OI_SIDE_Q
                || asset.loss_weight_sum_long > SOCIAL_LOSS_DEN
                || asset.loss_weight_sum_short > SOCIAL_LOSS_DEN
                || asset.social_loss_remainder_long_num >= SOCIAL_LOSS_DEN
                || asset.social_loss_remainder_short_num >= SOCIAL_LOSS_DEN
                || asset.social_loss_dust_long_num >= SOCIAL_LOSS_DEN
                || asset.social_loss_dust_short_num >= SOCIAL_LOSS_DEN
            {
                return Err(V13Error::InvalidConfig);
            }
        }
        Ok(())
    }

    fn b_target_for_leg(&self, asset_index: usize, leg: PortfolioLegV13) -> V13Result<u128> {
        let asset = self.assets[asset_index];
        let (current_b, epoch_start_b, side_epoch, mode) = match leg.side {
            SideV13::Long => (
                asset.b_long_num,
                asset.b_epoch_start_long_num,
                asset.epoch_long,
                asset.mode_long,
            ),
            SideV13::Short => (
                asset.b_short_num,
                asset.b_epoch_start_short_num,
                asset.epoch_short,
                asset.mode_short,
            ),
        };
        if leg.b_epoch_snap == side_epoch {
            Ok(current_b)
        } else if mode == SideModeV13::ResetPending
            && leg.b_epoch_snap.checked_add(1) == Some(side_epoch)
        {
            Ok(epoch_start_b)
        } else {
            Err(V13Error::InvalidLeg)
        }
    }

    fn kf_target_for_leg(
        &self,
        asset_index: usize,
        leg: PortfolioLegV13,
    ) -> V13Result<(i128, i128)> {
        let asset = self.assets[asset_index];
        let (current_k, current_f, epoch_start_k, epoch_start_f, side_epoch, mode) = match leg.side
        {
            SideV13::Long => (
                asset.k_long,
                asset.f_long_num,
                asset.k_epoch_start_long,
                asset.f_epoch_start_long_num,
                asset.epoch_long,
                asset.mode_long,
            ),
            SideV13::Short => (
                asset.k_short,
                asset.f_short_num,
                asset.k_epoch_start_short,
                asset.f_epoch_start_short_num,
                asset.epoch_short,
                asset.mode_short,
            ),
        };
        if leg.epoch_snap == side_epoch {
            Ok((current_k, current_f))
        } else if mode == SideModeV13::ResetPending
            && leg.epoch_snap.checked_add(1) == Some(side_epoch)
        {
            Ok((epoch_start_k, epoch_start_f))
        } else {
            Err(V13Error::InvalidLeg)
        }
    }

    fn residual(&self) -> u128 {
        self.vault
            .saturating_sub(self.c_tot.saturating_add(self.insurance))
    }

    fn consume_insurance_for_negative_pnl(
        &mut self,
        account: &mut PortfolioAccountV13,
    ) -> V13Result<u128> {
        if account.pnl >= 0 {
            return Ok(0);
        }
        self.bankruptcy_hlock_active = true;
        let residual = account.pnl.unsigned_abs();
        let used = residual.min(self.insurance);
        if used == 0 {
            return Ok(0);
        }
        self.insurance = self
            .insurance
            .checked_sub(used)
            .ok_or(V13Error::CounterUnderflow)?;
        let used_i128 = i128::try_from(used).map_err(|_| V13Error::ArithmeticOverflow)?;
        let new_pnl = account
            .pnl
            .checked_add(used_i128)
            .ok_or(V13Error::ArithmeticOverflow)?;
        self.set_account_pnl(account, new_pnl)?;
        account.health_cert.valid = false;
        Ok(used)
    }

    fn preflight_liquidation_residual_durability(
        &mut self,
        asset_index: usize,
        bankrupt_side: SideV13,
        account: &PortfolioAccountV13,
    ) -> V13Result<()> {
        let residual_after_principal_and_insurance = if account.pnl < 0 {
            account
                .pnl
                .unsigned_abs()
                .saturating_sub(account.capital)
                .saturating_sub(self.insurance)
        } else {
            0
        };
        if residual_after_principal_and_insurance == 0 {
            return Ok(());
        }
        let capacity = self.bankruptcy_residual_single_step_capacity(
            asset_index,
            bankrupt_side,
            residual_after_principal_and_insurance,
        )?;
        if capacity < residual_after_principal_and_insurance {
            self.declare_permissionless_recovery(
                PermissionlessRecoveryReasonV13::ActiveBankruptCloseCannotProgress,
            )?;
            return Err(V13Error::RecoveryRequired);
        }
        Ok(())
    }

    fn bankruptcy_residual_single_step_capacity(
        &self,
        asset_index: usize,
        bankrupt_side: SideV13,
        residual_remaining: u128,
    ) -> V13Result<u128> {
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        if residual_remaining == 0 {
            return Ok(0);
        }

        let opp = opposite_side(bankrupt_side);
        let asset = self.assets[asset_index];
        let (b_now, weight_sum, rem, explicit_used) = match opp {
            SideV13::Long => (
                asset.b_long_num,
                asset.loss_weight_sum_long,
                asset.social_loss_remainder_long_num,
                asset.explicit_unallocated_loss_long,
            ),
            SideV13::Short => (
                asset.b_short_num,
                asset.loss_weight_sum_short,
                asset.social_loss_remainder_short_num,
                asset.explicit_unallocated_loss_short,
            ),
        };
        if weight_sum == 0 {
            return Ok(residual_remaining.min(u128::MAX - explicit_used));
        }

        let headroom_plus_one = U256::from_u128(u128::MAX - b_now)
            .checked_add(U256::ONE)
            .ok_or(V13Error::ArithmeticOverflow)?;
        let max_scaled = headroom_plus_one
            .checked_mul(U256::from_u128(weight_sum))
            .and_then(|v| v.checked_sub(U256::ONE))
            .ok_or(V13Error::ArithmeticOverflow)?;
        if U256::from_u128(rem) > max_scaled {
            return Ok(0);
        }
        let max_chunk_by_b_wide = max_scaled
            .checked_sub(U256::from_u128(rem))
            .and_then(|v| v.checked_div(U256::from_u128(SOCIAL_LOSS_DEN)))
            .ok_or(V13Error::ArithmeticOverflow)?;
        let max_chunk_by_b = max_chunk_by_b_wide
            .try_into_u128()
            .unwrap_or(residual_remaining);
        Ok(residual_remaining
            .min(max_chunk_by_b)
            .min(self.config.public_b_chunk_atoms))
    }

    fn resolved_positive_payout_ready(&self) -> bool {
        if self.active_bankrupt_close_present
            || self.b_stale_account_count != 0
            || self.stale_certificate_count != 0
            || self.negative_pnl_account_count != 0
        {
            return false;
        }
        for i in 0..self.config.max_portfolio_assets as usize {
            let asset = self.assets[i];
            if asset.stored_pos_count_long != 0
                || asset.stored_pos_count_short != 0
                || asset.stale_account_count_long != 0
                || asset.stale_account_count_short != 0
            {
                return false;
            }
        }
        true
    }

    fn settle_leg_kf_effects(
        &mut self,
        account: &mut PortfolioAccountV13,
        asset_index: usize,
    ) -> V13Result<()> {
        let leg = account.legs[asset_index];
        if !leg.active {
            return Ok(());
        }
        let (k_now, f_now) = self.kf_target_for_leg(asset_index, leg)?;
        let den = leg
            .a_basis
            .checked_mul(POS_SCALE)
            .ok_or(V13Error::ArithmeticOverflow)?;
        let k_delta = wide_signed_mul_div_floor_from_k_pair(
            leg.basis_pos_q.unsigned_abs(),
            leg.k_snap,
            k_now,
            den,
        );
        let f_delta = wide_signed_mul_div_floor_from_k_pair(
            leg.basis_pos_q.unsigned_abs(),
            leg.f_snap,
            f_now,
            den,
        );
        let net = k_delta
            .checked_add(f_delta)
            .ok_or(V13Error::ArithmeticOverflow)?;
        if net != 0 {
            let new_pnl = account
                .pnl
                .checked_add(net)
                .ok_or(V13Error::ArithmeticOverflow)?;
            self.set_account_pnl(account, new_pnl)?;
        }
        account.legs[asset_index].k_snap = k_now;
        account.legs[asset_index].f_snap = f_now;
        account.health_cert.valid = false;
        Ok(())
    }

    fn apply_position_delta(
        &mut self,
        account: &mut PortfolioAccountV13,
        asset_index: usize,
        delta_q: i128,
    ) -> V13Result<()> {
        if delta_q == 0 {
            return Ok(());
        }
        if asset_index >= self.config.max_portfolio_assets as usize {
            return Err(V13Error::InvalidLeg);
        }
        self.settle_leg_kf_effects(account, asset_index)?;
        let current = signed_position(account.legs[asset_index]);
        let new = current
            .checked_add(delta_q)
            .ok_or(V13Error::ArithmeticOverflow)?;
        validate_basis_or_zero(new)?;
        if current == 0 {
            let side = if new > 0 {
                SideV13::Long
            } else {
                SideV13::Short
            };
            return self.attach_leg(account, asset_index, side, new);
        }
        if new == 0 {
            return self.clear_leg(account, asset_index);
        }
        if current.signum() != new.signum() {
            self.clear_leg(account, asset_index)?;
            let side = if new > 0 {
                SideV13::Long
            } else {
                SideV13::Short
            };
            return self.attach_leg(account, asset_index, side, new);
        }

        let old_leg = account.legs[asset_index];
        let old_abs = old_leg.basis_pos_q.unsigned_abs();
        let new_abs = new.unsigned_abs();
        let new_weight = loss_weight_for_basis(new_abs, old_leg.a_basis)?;
        let asset = &mut self.assets[asset_index];
        match old_leg.side {
            SideV13::Long => {
                asset.oi_eff_long_q = adjust_u128(asset.oi_eff_long_q, old_abs, new_abs)?;
                asset.loss_weight_sum_long =
                    adjust_u128(asset.loss_weight_sum_long, old_leg.loss_weight, new_weight)?;
            }
            SideV13::Short => {
                asset.oi_eff_short_q = adjust_u128(asset.oi_eff_short_q, old_abs, new_abs)?;
                asset.loss_weight_sum_short =
                    adjust_u128(asset.loss_weight_sum_short, old_leg.loss_weight, new_weight)?;
            }
        }
        account.legs[asset_index].basis_pos_q = new;
        account.legs[asset_index].loss_weight = new_weight;
        account.health_cert.valid = false;
        self.validate_account_shape(account)
    }

    fn reduce_position(
        &mut self,
        account: &mut PortfolioAccountV13,
        asset_index: usize,
        close_q: u128,
    ) -> V13Result<()> {
        if close_q == 0 {
            return Ok(());
        }
        let leg = account.legs[asset_index];
        if !leg.active {
            return Err(V13Error::InvalidLeg);
        }
        let close_i128 = i128::try_from(close_q).map_err(|_| V13Error::ArithmeticOverflow)?;
        let delta = match leg.side {
            SideV13::Long => close_i128
                .checked_neg()
                .ok_or(V13Error::ArithmeticOverflow)?,
            SideV13::Short => close_i128,
        };
        self.apply_position_delta(account, asset_index, delta)
    }

    fn set_account_pnl(
        &mut self,
        account: &mut PortfolioAccountV13,
        new_pnl: i128,
    ) -> V13Result<()> {
        validate_non_min_i128(new_pnl)?;
        let old_pos = account.pnl.max(0) as u128;
        let new_pos = new_pnl.max(0) as u128;
        if new_pos >= old_pos {
            self.pnl_pos_tot = self
                .pnl_pos_tot
                .checked_add(new_pos - old_pos)
                .ok_or(V13Error::ArithmeticOverflow)?;
        } else {
            self.pnl_pos_tot = self
                .pnl_pos_tot
                .checked_sub(old_pos - new_pos)
                .ok_or(V13Error::CounterUnderflow)?;
            self.pnl_matured_pos_tot = self.pnl_matured_pos_tot.min(self.pnl_pos_tot);
        }

        let old_negative = account.pnl < 0;
        let new_negative = new_pnl < 0;
        match (old_negative, new_negative) {
            (false, true) => {
                self.negative_pnl_account_count = self
                    .negative_pnl_account_count
                    .checked_add(1)
                    .ok_or(V13Error::CounterOverflow)?;
            }
            (true, false) => {
                self.negative_pnl_account_count = self
                    .negative_pnl_account_count
                    .checked_sub(1)
                    .ok_or(V13Error::CounterUnderflow)?;
            }
            _ => {}
        }
        account.pnl = new_pnl;
        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountBSettlementChunkV13 {
    pub delta_b: u128,
    pub loss: u128,
    pub new_remainder: u128,
    pub remaining_after: u128,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RiskScoreV13 {
    pub certified_liq_deficit: u128,
    pub unsettled_b_loss_bound: u128,
    pub stale_loss_bound: u128,
    pub gross_risk_notional: u128,
    pub active_leg_count: u32,
}

impl RiskScoreV13 {
    pub fn strictly_reduces_from(self, before: Self) -> bool {
        self < before
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionlessProgressOutcomeV13 {
    AccountCurrent,
    AccountBChunk(AccountBSettlementChunkV13),
    ResidualBooked(BResidualBookingOutcomeV13),
    RecoveryDeclared(PermissionlessRecoveryReasonV13),
}

pub fn risk_notional_ceil(abs_pos_q: u128, price: u64) -> V13Result<u128> {
    if abs_pos_q == 0 {
        return Ok(0);
    }
    checked_mul_div_ceil_u256(
        U256::from_u128(abs_pos_q),
        U256::from_u128(price as u128),
        U256::from_u128(POS_SCALE),
    )
    .and_then(|v| v.try_into_u128())
    .ok_or(V13Error::ArithmeticOverflow)
}

pub fn account_equity(account: &PortfolioAccountV13) -> V13Result<i128> {
    validate_non_min_i128(account.pnl)?;
    validate_fee_credits(account.fee_credits)?;
    let capital = i128::try_from(account.capital).map_err(|_| V13Error::ArithmeticOverflow)?;
    let fee_debt =
        i128::try_from(fee_debt_u128(account)?).map_err(|_| V13Error::ArithmeticOverflow)?;
    capital
        .checked_add(account.pnl)
        .and_then(|v| v.checked_sub(fee_debt))
        .ok_or(V13Error::ArithmeticOverflow)
}

fn account_no_positive_credit_equity(account: &PortfolioAccountV13) -> V13Result<i128> {
    validate_non_min_i128(account.pnl)?;
    validate_fee_credits(account.fee_credits)?;
    let capital = i128::try_from(account.capital).map_err(|_| V13Error::ArithmeticOverflow)?;
    let fee_debt =
        i128::try_from(fee_debt_u128(account)?).map_err(|_| V13Error::ArithmeticOverflow)?;
    capital
        .checked_add(account.pnl.min(0))
        .and_then(|v| v.checked_sub(fee_debt))
        .ok_or(V13Error::ArithmeticOverflow)
}

fn account_no_positive_credit_equity_with_capital(
    account: &PortfolioAccountV13,
    capital_override: u128,
) -> V13Result<i128> {
    validate_non_min_i128(account.pnl)?;
    validate_fee_credits(account.fee_credits)?;
    let capital = i128::try_from(capital_override).map_err(|_| V13Error::ArithmeticOverflow)?;
    let fee_debt =
        i128::try_from(fee_debt_u128(account)?).map_err(|_| V13Error::ArithmeticOverflow)?;
    capital
        .checked_add(account.pnl.min(0))
        .and_then(|v| v.checked_sub(fee_debt))
        .ok_or(V13Error::ArithmeticOverflow)
}

fn ensure_initial_margin(account: &PortfolioAccountV13) -> V13Result<()> {
    let equity = account_equity(account)?;
    if equity < 0 || (equity as u128) < account.health_cert.certified_initial_req {
        return Err(V13Error::InvalidConfig);
    }
    Ok(())
}

fn ensure_no_positive_credit_initial_margin(account: &PortfolioAccountV13) -> V13Result<()> {
    let equity = account_no_positive_credit_equity(account)?;
    if equity < 0 || (equity as u128) < account.health_cert.certified_initial_req {
        return Err(V13Error::LockActive);
    }
    Ok(())
}

fn account_equity_with_capital(
    account: &PortfolioAccountV13,
    capital_override: u128,
) -> V13Result<i128> {
    validate_non_min_i128(account.pnl)?;
    validate_fee_credits(account.fee_credits)?;
    let capital = i128::try_from(capital_override).map_err(|_| V13Error::ArithmeticOverflow)?;
    let fee_debt =
        i128::try_from(fee_debt_u128(account)?).map_err(|_| V13Error::ArithmeticOverflow)?;
    capital
        .checked_add(account.pnl)
        .and_then(|v| v.checked_sub(fee_debt))
        .ok_or(V13Error::ArithmeticOverflow)
}

fn position_delta_increases_risk(
    account: &PortfolioAccountV13,
    asset_index: usize,
    delta_q: i128,
) -> V13Result<bool> {
    let current = signed_position(account.legs[asset_index]);
    let next = current
        .checked_add(delta_q)
        .ok_or(V13Error::ArithmeticOverflow)?;
    validate_basis_or_zero(next)?;
    Ok(next.unsigned_abs() > current.unsigned_abs())
}

fn margin_requirement(notional: u128, bps: u64, floor: u128) -> V13Result<u128> {
    if notional == 0 {
        return Ok(0);
    }
    let raw = wide_mul_div_floor_u128(notional, bps as u128, MAX_MARGIN_BPS as u128);
    Ok(raw.max(floor))
}

fn trade_notional_floor(size_q: u128, exec_price: u64) -> V13Result<u128> {
    if size_q == 0 {
        return Ok(0);
    }
    let (q, _) = mul_div_floor_u256_with_rem(
        U256::from_u128(size_q),
        U256::from_u128(exec_price as u128),
        U256::from_u128(POS_SCALE),
    );
    q.try_into_u128().ok_or(V13Error::ArithmeticOverflow)
}

fn checked_fee_bps(notional: u128, fee_bps: u64) -> V13Result<u128> {
    if notional == 0 || fee_bps == 0 {
        return Ok(0);
    }
    checked_mul_div_ceil_u256(
        U256::from_u128(notional),
        U256::from_u128(fee_bps as u128),
        U256::from_u128(MAX_MARGIN_BPS as u128),
    )
    .and_then(|v| v.try_into_u128())
    .ok_or(V13Error::ArithmeticOverflow)
}

fn checked_i128_mul(a: i128, b: i128) -> V13Result<i128> {
    let out = a.checked_mul(b).ok_or(V13Error::ArithmeticOverflow)?;
    validate_non_min_i128(out)?;
    Ok(out)
}

fn add_non_min_i128(a: i128, b: i128) -> V13Result<i128> {
    let out = a.checked_add(b).ok_or(V13Error::ArithmeticOverflow)?;
    validate_non_min_i128(out)?;
    Ok(out)
}

fn adjust_u128(current: u128, old: u128, new: u128) -> V13Result<u128> {
    if new >= old {
        current
            .checked_add(new - old)
            .ok_or(V13Error::ArithmeticOverflow)
    } else {
        current
            .checked_sub(old - new)
            .ok_or(V13Error::CounterUnderflow)
    }
}

fn encode_bool(value: bool) -> u8 {
    if value {
        1
    } else {
        0
    }
}

fn decode_bool(value: u8) -> V13Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(V13Error::InvalidConfig),
    }
}

fn encode_side(value: SideV13) -> u8 {
    match value {
        SideV13::Long => 0,
        SideV13::Short => 1,
    }
}

fn decode_side(value: u8) -> V13Result<SideV13> {
    match value {
        0 => Ok(SideV13::Long),
        1 => Ok(SideV13::Short),
        _ => Err(V13Error::InvalidConfig),
    }
}

fn encode_side_mode(value: SideModeV13) -> u8 {
    match value {
        SideModeV13::Normal => 0,
        SideModeV13::DrainOnly => 1,
        SideModeV13::ResetPending => 2,
    }
}

fn decode_side_mode(value: u8) -> V13Result<SideModeV13> {
    match value {
        0 => Ok(SideModeV13::Normal),
        1 => Ok(SideModeV13::DrainOnly),
        2 => Ok(SideModeV13::ResetPending),
        _ => Err(V13Error::InvalidConfig),
    }
}

fn encode_market_mode(value: MarketModeV13) -> u8 {
    match value {
        MarketModeV13::Live => 0,
        MarketModeV13::Resolved => 1,
        MarketModeV13::Recovery => 2,
    }
}

fn decode_market_mode(value: u8) -> V13Result<MarketModeV13> {
    match value {
        0 => Ok(MarketModeV13::Live),
        1 => Ok(MarketModeV13::Resolved),
        2 => Ok(MarketModeV13::Recovery),
        _ => Err(V13Error::InvalidConfig),
    }
}

fn encode_recovery_reason(value: PermissionlessRecoveryReasonV13) -> u8 {
    match value {
        PermissionlessRecoveryReasonV13::BelowProgressFloor => 0,
        PermissionlessRecoveryReasonV13::BlockedSegmentHeadroomOrRepresentability => 1,
        PermissionlessRecoveryReasonV13::AccountBSettlementCannotProgress => 2,
        PermissionlessRecoveryReasonV13::BIndexHeadroomExhausted => 3,
        PermissionlessRecoveryReasonV13::ActiveBankruptCloseCannotProgress => 4,
        PermissionlessRecoveryReasonV13::ExplicitLossOrDustAuditOverflow => 5,
        PermissionlessRecoveryReasonV13::OracleOrTargetUnavailableByAuthenticatedPolicy => 6,
        PermissionlessRecoveryReasonV13::CounterOrEpochOverflowDeclaredRecovery => 7,
    }
}

fn decode_recovery_reason(value: u8) -> V13Result<PermissionlessRecoveryReasonV13> {
    match value {
        0 => Ok(PermissionlessRecoveryReasonV13::BelowProgressFloor),
        1 => Ok(PermissionlessRecoveryReasonV13::BlockedSegmentHeadroomOrRepresentability),
        2 => Ok(PermissionlessRecoveryReasonV13::AccountBSettlementCannotProgress),
        3 => Ok(PermissionlessRecoveryReasonV13::BIndexHeadroomExhausted),
        4 => Ok(PermissionlessRecoveryReasonV13::ActiveBankruptCloseCannotProgress),
        5 => Ok(PermissionlessRecoveryReasonV13::ExplicitLossOrDustAuditOverflow),
        6 => Ok(PermissionlessRecoveryReasonV13::OracleOrTargetUnavailableByAuthenticatedPolicy),
        7 => Ok(PermissionlessRecoveryReasonV13::CounterOrEpochOverflowDeclaredRecovery),
        _ => Err(V13Error::InvalidConfig),
    }
}

fn validate_basis_or_zero(basis_pos_q: i128) -> V13Result<()> {
    if basis_pos_q == 0 {
        Ok(())
    } else {
        validate_basis(basis_pos_q)
    }
}

fn signed_position(leg: PortfolioLegV13) -> i128 {
    if !leg.active {
        0
    } else {
        match leg.side {
            SideV13::Long => leg.basis_pos_q.unsigned_abs() as i128,
            SideV13::Short => -(leg.basis_pos_q.unsigned_abs() as i128),
        }
    }
}

fn opposite_side(side: SideV13) -> SideV13 {
    match side {
        SideV13::Long => SideV13::Short,
        SideV13::Short => SideV13::Long,
    }
}

fn quarantine_remainder(remainder: &mut u128, dust: &mut u128) -> V13Result<()> {
    if *remainder == 0 {
        return Ok(());
    }
    let new_dust = dust
        .checked_add(*remainder)
        .ok_or(V13Error::ArithmeticOverflow)?;
    if new_dust >= SOCIAL_LOSS_DEN {
        return Err(V13Error::RecoveryRequired);
    }
    *dust = new_dust;
    *remainder = 0;
    Ok(())
}

fn validate_non_min_i128(v: i128) -> V13Result<()> {
    if v == i128::MIN {
        return Err(V13Error::ArithmeticOverflow);
    }
    Ok(())
}

fn validate_fee_credits(v: i128) -> V13Result<()> {
    validate_non_min_i128(v)?;
    if v > 0 {
        return Err(V13Error::InvalidLeg);
    }
    Ok(())
}

fn fee_debt_u128(account: &PortfolioAccountV13) -> V13Result<u128> {
    validate_fee_credits(account.fee_credits)?;
    Ok(account.fee_credits.unsigned_abs())
}

fn validate_basis(basis_pos_q: i128) -> V13Result<()> {
    if basis_pos_q == 0
        || basis_pos_q == i128::MIN
        || basis_pos_q.unsigned_abs() > MAX_POSITION_ABS_Q
    {
        return Err(V13Error::InvalidLeg);
    }
    Ok(())
}

fn validate_active_leg(leg: PortfolioLegV13) -> V13Result<()> {
    validate_basis(leg.basis_pos_q)?;
    validate_non_min_i128(leg.k_snap)?;
    validate_non_min_i128(leg.f_snap)?;
    if !(MIN_A_SIDE..=ADL_ONE).contains(&leg.a_basis)
        || leg.loss_weight == 0
        || leg.loss_weight != loss_weight_for_basis(leg.basis_pos_q.unsigned_abs(), leg.a_basis)?
        || leg.b_rem >= SOCIAL_LOSS_DEN
        || leg.b_epoch_snap != leg.epoch_snap
    {
        return Err(V13Error::InvalidLeg);
    }
    Ok(())
}

fn loss_weight_for_basis(abs_basis_q: u128, a_basis: u128) -> V13Result<u128> {
    if a_basis == 0 {
        return Err(V13Error::InvalidLeg);
    }
    checked_mul_div_ceil_u256(
        U256::from_u128(abs_basis_q),
        U256::from_u128(SOCIAL_WEIGHT_SCALE),
        U256::from_u128(a_basis),
    )
    .and_then(|v| v.try_into_u128())
    .ok_or(V13Error::ArithmeticOverflow)
}

fn has_b_stale_leg(account: &PortfolioAccountV13) -> bool {
    account.legs.iter().any(|leg| leg.active && leg.b_stale)
}

fn account_b_loss_bound(account: &PortfolioAccountV13) -> V13Result<u128> {
    let mut bound = 0u128;
    for leg in account.legs.iter() {
        if leg.active && leg.b_stale {
            bound = bound
                .checked_add(leg.loss_weight)
                .ok_or(V13Error::ArithmeticOverflow)?;
        }
    }
    Ok(bound)
}
