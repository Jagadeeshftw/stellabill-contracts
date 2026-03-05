//! Contract types: errors, state, and events.

use soroban_sdk::{contracterror, contracttype, Address};

pub const BILLING_SNAPSHOT_FLAG_CLOSED: u32 = 1 << 0;
pub const BILLING_SNAPSHOT_FLAG_INTERVAL_CHARGED: u32 = 1 << 1;
pub const BILLING_SNAPSHOT_FLAG_USAGE_CHARGED: u32 = 1 << 2;
pub const BILLING_SNAPSHOT_FLAG_EMPTY_PERIOD: u32 = 1 << 3;

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    MerchantSubs(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionStatus {
    Active = 0,
    Paused = 1,
    Cancelled = 2,
    InsufficientBalance = 3,
    GracePeriod = 4,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Subscription {
    pub subscriber: Address,
    pub merchant: Address,
    pub amount: i128,
    pub interval_seconds: u64,
    pub last_payment_timestamp: u64,
    pub status: SubscriptionStatus,
    pub prepaid_balance: i128,
    pub usage_enabled: bool,
    pub expiration: Option<u64>,
    pub billing_anchor_timestamp: u64,
    pub current_period_index: u32,
    pub current_period_usage_units: i128,
    pub usage_cap_units: Option<i128>,
    pub usage_rate_limit_max_calls: Option<u32>,
    pub usage_rate_window_secs: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BillingPeriodSnapshot {
    pub subscription_id: u32,
    pub period_index: u32,
    pub period_start_timestamp: u64,
    pub period_end_timestamp: u64,
    pub total_amount_charged: i128,
    pub total_usage_units: i128,
    pub status_flags: u32,
}

#[contracterror]
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    Unauthorized = 401,
    Forbidden = 403,
    NotFound = 404,
    InvalidStatusTransition = 400,
    BelowMinimumTopup = 402,
    InvalidRecoveryAmount = 405,
    SubscriptionExpired = 410,
    SubscriptionLimitReached = 429,
    IntervalNotElapsed = 1001,
    NotActive = 1002,
    InsufficientBalance = 1003,
    UsageNotEnabled = 1004,
    InsufficientPrepaidBalance = 1005,
    InvalidAmount = 1006,
    Replay = 1007,
    EmergencyStopActive = 1009,
    Underflow = 1010,
    RecoveryNotAllowed = 1011,
    Overflow = 1012,
    NotInitialized = 1013,
    InvalidExportLimit = 1014,
    InvalidInput = 1015,
    Reentrancy = 1016,
    AlreadyInitialized = 1017,
    UsageCapExceeded = 1018,
    RateLimitExceeded = 1019,
    InvalidFeeBps = 1020,
    TreasuryNotConfigured = 1021,
}

impl Error {
    pub const fn to_code(self) -> u32 {
        self as u32
    }
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct BatchChargeResult {
    pub success: bool,
    pub error_code: u32,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct PlanTemplate {
    pub merchant: Address,
    pub amount: i128,
    pub interval_seconds: u64,
    pub usage_enabled: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NextChargeInfo {
    pub next_charge_timestamp: u64,
    pub is_charge_expected: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryReason {
    AccidentalTransfer = 0,
    DeprecatedFlow = 1,
    UnreachableSubscriber = 2,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct RecoveryEvent {
    pub admin: Address,
    pub recipient: Address,
    pub amount: i128,
    pub reason: RecoveryReason,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct UsageCapReachedEvent {
    pub subscription_id: u32,
    pub period_index: u32,
    pub cap_units: i128,
    pub attempted_units: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct ProtocolFeeSkimmedEvent {
    pub subscription_id: u32,
    pub merchant: Address,
    pub treasury: Address,
    pub gross_amount: i128,
    pub fee_amount: i128,
    pub net_amount: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct SubscriptionChargedEvent {
    pub subscription_id: u32,
    pub merchant: Address,
    pub amount: i128,
}
