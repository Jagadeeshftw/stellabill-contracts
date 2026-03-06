//! Contract types: errors, subscription data structures, and event types.
//!
//! Kept in a separate module to reduce merge conflicts when editing state machine
//! or contract entrypoints.

use soroban_sdk::{contracterror, contracttype, Address, String};

pub const BILLING_SNAPSHOT_FLAG_CLOSED: u32 = 1 << 0;
pub const BILLING_SNAPSHOT_FLAG_INTERVAL_CHARGED: u32 = 1 << 1;
pub const BILLING_SNAPSHOT_FLAG_USAGE_CHARGED: u32 = 1 << 2;
pub const BILLING_SNAPSHOT_FLAG_EMPTY_PERIOD: u32 = 1 << 3;

/// Maximum number of metadata keys per subscription.
pub const MAX_METADATA_KEYS: u32 = 10;
/// Maximum length of a metadata key in bytes.
pub const MAX_METADATA_KEY_LENGTH: u32 = 32;
/// Maximum length of a metadata value in bytes.
pub const MAX_METADATA_VALUE_LENGTH: u32 = 256;

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    MerchantSubs(Address),
    EmergencyStop,
}

/// Represents the lifecycle state of a subscription.
///
/// See `docs/subscription_lifecycle.md` for how each status is entered and exited.
///
/// # State Machine
///
/// - **Active**: Subscription is active and charges can be processed.
///   - Can transition to: `Paused`, `Cancelled`, `InsufficientBalance`, `GracePeriod`
/// - **Paused**: Subscription is temporarily suspended, no charges processed.
///   - Can transition to: `Active`, `Cancelled`
/// - **Cancelled**: Subscription is permanently terminated (terminal state).
///   - No outgoing transitions
/// - **InsufficientBalance**: Subscription failed due to insufficient funds.
///   - Can transition to: `Active` (after deposit + resume), `Cancelled`
/// - **GracePeriod**: Subscription is in grace period after a missed charge.
///   - Can transition to: `Active`, `InsufficientBalance`, `Cancelled`
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionStatus {
    /// Subscription is active and ready for charging.
    Active = 0,
    /// Subscription is temporarily paused, no charges processed.
    Paused = 1,
    /// Subscription is permanently cancelled (terminal state).
    Cancelled = 2,
    /// Subscription failed due to insufficient balance for charging.
    InsufficientBalance = 3,
    /// Subscription is in grace period after a missed charge.
    GracePeriod = 4,
}

/// Stores subscription details and current state.
///
/// The `status` field is managed by the state machine. Use the provided
/// transition helpers to modify status, never set it directly.
/// See `docs/subscription_lifecycle.md` for lifecycle and on-chain representation.
///
/// # Storage Schema
///
/// This is a named-field struct encoded on-ledger as a ScMap keyed by field names.
/// Adding new fields at the end with conservative defaults is a storage-extending change.
/// Changing field types or removing fields is a breaking change.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Subscription {
    pub subscriber: Address,
    pub merchant: Address,
    /// Recurring charge amount per billing interval (in token base units, e.g. stroops for USDC).
    pub amount: i128,
    /// Billing interval in seconds.
    pub interval_seconds: u64,
    pub last_payment_timestamp: u64,
    /// Current lifecycle state. Modified only through state machine transitions.
    pub status: SubscriptionStatus,
    /// Subscriber's prepaid balance held in escrow by the contract.
    pub prepaid_balance: i128,
    pub usage_enabled: bool,
    /// Optional subscription expiry timestamp. `None` means no expiration.
    pub expiration: Option<u64>,
    /// Anchor timestamp for billing period calculations.
    pub billing_anchor_timestamp: u64,
    /// Current billing period index.
    pub current_period_index: u32,
    /// Usage units consumed in the current period.
    pub current_period_usage_units: i128,
    /// Optional per-period usage cap in units.
    pub usage_cap_units: Option<i128>,
    /// Optional maximum usage calls per rate window.
    pub usage_rate_limit_max_calls: Option<u32>,
    /// Rate limit window duration in seconds.
    pub usage_rate_window_secs: u64,
    /// Optional maximum total amount (in token base units) that may ever be charged
    /// over the entire lifespan of this subscription. `None` means no cap.
    pub lifetime_cap: Option<i128>,
    /// Cumulative total of all amounts successfully charged so far.
    pub lifetime_charged: i128,
}

/// A snapshot of a single billing period for audit and dispute resolution.
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
    /// The requested state transition is not allowed by the state machine.
    InvalidStatusTransition = 400,
    /// Caller does not have the required authorization.
    Unauthorized = 401,
    /// The top-up amount is below the minimum required threshold.
    BelowMinimumTopup = 402,
    /// Caller is authorized but does not have permission for this specific action.
    Forbidden = 403,
    /// The requested resource was not found in storage.
    NotFound = 404,
    InvalidRecoveryAmount = 405,
    SubscriptionExpired = 410,
    /// The contract has allocated the maximum number of subscriptions.
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
    /// Lifetime charge cap has been reached; no further charges are allowed.
    LifetimeCapReached = 1017,
    /// Contract is already initialized; init may only be called once.
    AlreadyInitialized = 1018,
    UsageCapExceeded = 1019,
    RateLimitExceeded = 1020,
    InvalidFeeBps = 1021,
    TreasuryNotConfigured = 1022,
    /// Metadata key limit reached for this subscription.
    MetadataKeyLimitReached = 1023,
    /// Metadata key exceeds maximum allowed length.
    MetadataKeyTooLong = 1024,
    /// Metadata value exceeds maximum allowed length.
    MetadataValueTooLong = 1025,
    /// Subscriber is on the blocklist and cannot create or interact with subscriptions.
    SubscriberBlocklisted = 1026,
}

impl Error {
    pub const fn to_code(self) -> u32 {
        self as u32
    }
}

/// Result of charging one subscription in a batch.
#[contracttype]
#[derive(Clone, Debug)]
pub struct BatchChargeResult {
    pub success: bool,
    /// If success is false, the error code; otherwise 0.
    pub error_code: u32,
}

/// Result of a batch merchant withdrawal operation.
#[contracttype]
#[derive(Clone, Debug)]
pub struct BatchWithdrawResult {
    pub success: bool,
    pub error_code: u32,
}

/// A read-only snapshot of the contract's configuration and current state.
#[contracttype]
#[derive(Clone, Debug)]
pub struct ContractSnapshot {
    pub admin: Address,
    pub token: Address,
    pub min_topup: i128,
    pub next_id: u32,
    pub storage_version: u32,
    pub timestamp: u64,
}

/// A summary of a subscription's current state, intended for migration or reporting.
#[contracttype]
#[derive(Clone, Debug)]
pub struct SubscriptionSummary {
    pub subscription_id: u32,
    pub subscriber: Address,
    pub merchant: Address,
    pub amount: i128,
    pub interval_seconds: u64,
    pub last_payment_timestamp: u64,
    pub status: SubscriptionStatus,
    pub prepaid_balance: i128,
    pub usage_enabled: bool,
    pub lifetime_cap: Option<i128>,
    pub lifetime_charged: i128,
}

/// Event emitted when subscriptions are exported for migration.
#[contracttype]
#[derive(Clone, Debug)]
pub struct MigrationExportEvent {
    pub admin: Address,
    pub start_id: u32,
    pub limit: u32,
    pub exported: u32,
    pub timestamp: u64,
}

/// Defines a reusable subscription plan template.
#[contracttype]
#[derive(Clone, Debug)]
pub struct PlanTemplate {
    pub merchant: Address,
    /// Recurring charge amount per interval (token base units).
    pub amount: i128,
    pub interval_seconds: u64,
    pub usage_enabled: bool,
    /// Optional lifetime cap applied to subscriptions created from this template.
    pub lifetime_cap: Option<i128>,
}

/// Result of computing next charge information for a subscription.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NextChargeInfo {
    /// Estimated timestamp for the next charge attempt.
    pub next_charge_timestamp: u64,
    /// Whether a charge is actually expected based on the subscription status.
    pub is_charge_expected: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryReason {
    /// Funds sent to contract address by mistake.
    AccidentalTransfer = 0,
    /// Funds from deprecated contract flows or logic errors.
    DeprecatedFlow = 1,
    /// Funds from cancelled subscriptions with unreachable addresses.
    UnreachableSubscriber = 2,
}

/// View of a subscription's lifetime cap status.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapInfo {
    pub lifetime_cap: Option<i128>,
    pub lifetime_charged: i128,
    pub remaining_cap: Option<i128>,
    pub cap_reached: bool,
}

/// Event emitted when emergency stop is enabled.
#[contracttype]
#[derive(Clone, Debug)]
pub struct EmergencyStopEnabledEvent {
    pub admin: Address,
    pub timestamp: u64,
}

/// Event emitted when emergency stop is disabled.
#[contracttype]
#[derive(Clone, Debug)]
pub struct EmergencyStopDisabledEvent {
    pub admin: Address,
    pub timestamp: u64,
}

/// Event emitted when admin recovers stranded funds.
#[contracttype]
#[derive(Clone, Debug)]
pub struct RecoveryEvent {
    pub admin: Address,
    pub recipient: Address,
    pub amount: i128,
    pub reason: RecoveryReason,
    pub timestamp: u64,
}

/// Event emitted when a usage cap is reached.
#[contracttype]
#[derive(Clone, Debug)]
pub struct UsageCapReachedEvent {
    pub subscription_id: u32,
    pub period_index: u32,
    pub cap_units: i128,
    pub attempted_units: i128,
}

/// Event emitted when protocol fee is skimmed.
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
pub struct SubscriptionCreatedEvent {
    pub subscription_id: u32,
    pub subscriber: Address,
    pub merchant: Address,
    pub amount: i128,
    pub interval_seconds: u64,
    pub lifetime_cap: Option<i128>,
}

/// Event emitted when funds are deposited into a subscription vault.
#[contracttype]
#[derive(Clone, Debug)]
pub struct FundsDepositedEvent {
    pub subscription_id: u32,
    pub subscriber: Address,
    pub amount: i128,
}

/// Event emitted when a subscription interval charge succeeds.
#[contracttype]
#[derive(Clone, Debug)]
pub struct SubscriptionChargedEvent {
    pub subscription_id: u32,
    pub merchant: Address,
    pub amount: i128,
    pub lifetime_charged: i128,
}

/// Event emitted when a subscription is cancelled.
#[contracttype]
#[derive(Clone, Debug)]
pub struct SubscriptionCancelledEvent {
    pub subscription_id: u32,
    pub authorizer: Address,
    pub refund_amount: i128,
}

/// Event emitted when a subscription is paused.
#[contracttype]
#[derive(Clone, Debug)]
pub struct SubscriptionPausedEvent {
    pub subscription_id: u32,
    pub authorizer: Address,
}

/// Event emitted when a subscription is resumed.
#[contracttype]
#[derive(Clone, Debug)]
pub struct SubscriptionResumedEvent {
    pub subscription_id: u32,
    pub authorizer: Address,
}

/// Event emitted when a merchant withdraws funds.
#[contracttype]
#[derive(Clone, Debug)]
pub struct MerchantWithdrawalEvent {
    pub merchant: Address,
    pub amount: i128,
}

/// Event emitted when a merchant-initiated one-off charge is applied.
#[contracttype]
#[derive(Clone, Debug)]
pub struct OneOffChargedEvent {
    pub subscription_id: u32,
    pub merchant: Address,
    pub amount: i128,
}

/// Event emitted when the lifetime charge cap is reached.
#[contracttype]
#[derive(Clone, Debug)]
pub struct LifetimeCapReachedEvent {
    pub subscription_id: u32,
    pub lifetime_cap: i128,
    pub lifetime_charged: i128,
    pub timestamp: u64,
}

/// Event emitted when metadata is set or updated on a subscription.
#[contracttype]
#[derive(Clone, Debug)]
pub struct MetadataSetEvent {
    pub subscription_id: u32,
    pub key: String,
    pub authorizer: Address,
}

/// Event emitted when metadata is deleted from a subscription.
#[contracttype]
#[derive(Clone, Debug)]
pub struct MetadataDeletedEvent {
    pub subscription_id: u32,
    pub key: String,
    pub authorizer: Address,
}
