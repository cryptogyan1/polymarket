#[derive(Debug, Clone)]
pub enum TradeMode {
    Fixed,
    Percentage,
    Dynamic,
    Free,
}

#[derive(Debug, Clone)]
pub struct PositionSizing {
    pub mode: TradeMode,
    pub fixed_usdc: Option<f64>,
    pub percentage: Option<f64>,
    pub max_risk_percent: Option<f64>,
}
