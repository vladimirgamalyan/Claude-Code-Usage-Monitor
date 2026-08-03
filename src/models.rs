use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
}

/// Codex rate limit reset credits: one entry per credit that can still be
/// redeemed, holding when it expires (`None` when it never does).
#[derive(Clone, Debug, Default)]
pub struct CodexResetCredits {
    pub expiries: Vec<Option<SystemTime>>,
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
    pub antigravity: Option<UsageData>,
    /// Reset credits in hand, as reported by the Codex usage response itself.
    /// A change here is what prompts a fresh look at the credit list.
    pub codex_reset_credits_available: Option<usize>,
}
