//! Pre-trade limits use persisted exposure, including orders with an unknown outcome.
#[derive(Debug, Default)]
pub struct RiskSnapshot {
    pub daily_pnl: f64,
    pub open_cost: f64,
    pub consecutive_losses: i64,
    pub ambiguous_orders: i64,
    pub stale_positions: i64,
    pub window_already_traded: bool,
}

impl RiskSnapshot {
    pub fn check(
        &self,
        proposed_cost: f64,
        max_trade_cost: f64,
        max_open_cost: f64,
        daily_loss_limit: f64,
        max_consecutive_losses: i64,
    ) -> Result<(), String> {
        if [proposed_cost, max_trade_cost, max_open_cost, daily_loss_limit]
            .iter().any(|v| !v.is_finite() || *v <= 0.0)
            || !self.daily_pnl.is_finite() || !self.open_cost.is_finite()
            || self.open_cost < 0.0 || max_consecutive_losses <= 0
        {
            return Err("invalid_risk_input".into());
        }
        if self.ambiguous_orders > 0 { return Err("order_reconciliation_required".into()); }
        if self.stale_positions > 0 { return Err("position_reconciliation_required".into()); }
        if self.window_already_traded { return Err("window_already_traded".into()); }
        if self.consecutive_losses >= max_consecutive_losses {
            return Err("consecutive_loss_limit".into());
        }
        if proposed_cost > max_trade_cost + 1e-9 { return Err("trade_cost_limit".into()); }
        if self.open_cost + proposed_cost > max_open_cost + 1e-9 {
            return Err("open_exposure_limit".into());
        }
        // Reserve the full possible loss before entry. Unsettled positions
        // cannot silently spend the same remaining daily budget twice.
        if self.daily_pnl - self.open_cost - proposed_cost < -daily_loss_limit - 1e-9 {
            return Err("daily_loss_budget".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_unsettled_losses_before_another_order() {
        let s = RiskSnapshot { daily_pnl: -12.0, open_cost: 5.0, ..Default::default() };
        assert_eq!(s.check(4.0, 5.0, 10.0, 20.0, 3).unwrap_err(), "daily_loss_budget");
        assert!(s.check(3.0, 5.0, 10.0, 20.0, 3).is_ok());
    }

    #[test]
    fn unknown_orders_and_stale_positions_block_even_with_large_budget() {
        for s in [
            RiskSnapshot { ambiguous_orders: 1, ..Default::default() },
            RiskSnapshot { stale_positions: 1, ..Default::default() },
            RiskSnapshot { window_already_traded: true, ..Default::default() },
        ] {
            assert!(s.check(1.0, 1000.0, 1000.0, 1000.0, 3).is_err());
        }
    }

    #[test]
    fn invalid_numbers_cannot_disable_limits() {
        assert!(RiskSnapshot::default().check(f64::NAN, 5.0, 10.0, 20.0, 3).is_err());
        assert!(RiskSnapshot::default().check(1.0, f64::INFINITY, 10.0, 20.0, 3).is_err());
    }
}
