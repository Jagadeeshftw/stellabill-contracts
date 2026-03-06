#![no_std]

//! Prepaid subscription vault for recurring USDC billing.
//!
//! For subscription lifecycle, status transitions, and on-chain representation
//! see `docs/subscription_lifecycle.md`.
//!
//! For lifetime charge cap semantics see `docs/lifetime_caps.md`.
//!
//! For metadata key-value store see `docs/subscription_metadata.md`.

// -- Modules ------------------------------------------------------------------
mod admin;
mod blocklist;
mod charge_core;
mod merchant;
mod metadata;
mod queries;
mod reentrancy;
pub mod safe_math;
mod state_machine;
mod subscription;
mod types;

use soroban_sdk::{contract, contractimpl, Address, Env, String, Symbol, Vec};

// -- Re-exports ---------------------------------------------------------------
pub use blocklist::{BlocklistAddedEvent, BlocklistEntry, BlocklistRemovedEvent};
pub use queries::compute_next_charge_info;
pub use state_machine::{can_transition, get_allowed_transitions, validate_status_transition};
pub use types::{
    BatchChargeResult, BatchWithdrawResult, BillingPeriodSnapshot, CapInfo, ContractSnapshot,
    DataKey, EmergencyStopDisabledEvent, EmergencyStopEnabledEvent, Error, FundsDepositedEvent,
    LifetimeCapReachedEvent, MerchantWithdrawalEvent, MetadataDeletedEvent, MetadataSetEvent,
    MigrationExportEvent, NextChargeInfo, OneOffChargedEvent, PlanTemplate, RecoveryEvent,
    RecoveryReason, Subscription, SubscriptionCancelledEvent, SubscriptionChargedEvent,
    SubscriptionCreatedEvent, SubscriptionPausedEvent, SubscriptionResumedEvent,
    SubscriptionStatus, SubscriptionSummary, BILLING_SNAPSHOT_FLAG_CLOSED,
    BILLING_SNAPSHOT_FLAG_EMPTY_PERIOD, BILLING_SNAPSHOT_FLAG_INTERVAL_CHARGED,
    BILLING_SNAPSHOT_FLAG_USAGE_CHARGED, MAX_METADATA_KEYS, MAX_METADATA_KEY_LENGTH,
    MAX_METADATA_VALUE_LENGTH,
};

/// Maximum subscription ID this contract will ever allocate.
pub const MAX_SUBSCRIPTION_ID: u32 = u32::MAX;

const STORAGE_VERSION: u32 = 2;
const MAX_EXPORT_LIMIT: u32 = 100;

// -- Internal helpers ---------------------------------------------------------

fn require_admin_auth(env: &Env, admin: &Address) -> Result<(), Error> {
    admin.require_auth();
    let stored_admin = admin::require_admin(env)?;
    if admin != &stored_admin {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

fn get_emergency_stop(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&Symbol::new(env, "emergency_stop"))
        .unwrap_or(false)
}

fn require_not_emergency_stop(env: &Env) -> Result<(), Error> {
    if get_emergency_stop(env) {
        return Err(Error::EmergencyStopActive);
    }
    Ok(())
}

// -- Contract -----------------------------------------------------------------

#[contract]
pub struct SubscriptionVault;

#[contractimpl]
impl SubscriptionVault {
    // -- Admin / Config -------------------------------------------------------

    /// Initialize the contract: set token address, admin, minimum top-up, and grace period.
    pub fn init(
        env: Env,
        token: Address,
        token_decimals: u32,
        admin: Address,
        min_topup: i128,
        grace_period: u64,
    ) -> Result<(), Error> {
        admin::do_init(&env, token, token_decimals, admin, min_topup, grace_period)
    }

    /// Update the minimum top-up threshold. Only callable by admin.
    pub fn set_min_topup(env: Env, admin: Address, min_topup: i128) -> Result<(), Error> {
        admin::do_set_min_topup(&env, admin, min_topup)
    }

    pub fn get_min_topup(env: Env) -> Result<i128, Error> {
        admin::get_min_topup(&env)
    }

    pub fn set_grace_period(env: Env, admin: Address, grace_period: u64) -> Result<(), Error> {
        admin::do_set_grace_period(&env, admin, grace_period)
    }

    pub fn get_grace_period(env: Env) -> Result<u64, Error> {
        admin::get_grace_period(&env)
    }

    pub fn set_treasury(env: Env, admin: Address, treasury: Address) -> Result<(), Error> {
        admin::do_set_treasury(&env, admin, treasury)
    }

    pub fn get_treasury(env: Env) -> Result<Address, Error> {
        admin::do_get_treasury(&env)
    }

    pub fn set_protocol_fee_bps(env: Env, admin: Address, fee_bps: u32) -> Result<(), Error> {
        admin::do_set_protocol_fee_bps(&env, admin, fee_bps)
    }

    pub fn get_protocol_fee_bps(env: Env) -> u32 {
        admin::get_protocol_fee_bps(&env)
    }

    /// Get the current admin address.
    pub fn get_admin(env: Env) -> Result<Address, Error> {
        admin::do_get_admin(&env)
    }

    /// Rotate admin to a new address. Only callable by current admin.
    pub fn rotate_admin(env: Env, current_admin: Address, new_admin: Address) -> Result<(), Error> {
        admin::do_rotate_admin(&env, current_admin, new_admin)
    }

    /// Recover stranded funds from the contract. Admin only.
    pub fn recover_stranded_funds(
        env: Env,
        admin: Address,
        recipient: Address,
        amount: i128,
        reason: RecoveryReason,
    ) -> Result<(), Error> {
        admin::do_recover_stranded_funds(&env, admin, recipient, amount, reason)
    }

    // -- Emergency Stop -------------------------------------------------------

    /// Get the current emergency stop status.
    pub fn get_emergency_stop_status(env: Env) -> bool {
        get_emergency_stop(&env)
    }

    /// Enable the emergency stop (circuit breaker). Admin only.
    pub fn enable_emergency_stop(env: Env, admin: Address) -> Result<(), Error> {
        require_admin_auth(&env, &admin)?;
        if get_emergency_stop(&env) {
            return Ok(());
        }
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "emergency_stop"), &true);
        env.events().publish(
            (Symbol::new(&env, "emergency_stop_enabled"),),
            EmergencyStopEnabledEvent {
                admin,
                timestamp: env.ledger().timestamp(),
            },
        );
        Ok(())
    }

    /// Disable the emergency stop (circuit breaker). Admin only.
    pub fn disable_emergency_stop(env: Env, admin: Address) -> Result<(), Error> {
        require_admin_auth(&env, &admin)?;
        if !get_emergency_stop(&env) {
            return Ok(());
        }
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "emergency_stop"), &false);
        env.events().publish(
            (Symbol::new(&env, "emergency_stop_disabled"),),
            EmergencyStopDisabledEvent {
                admin,
                timestamp: env.ledger().timestamp(),
            },
        );
        Ok(())
    }

    // -- Migration / Export ---------------------------------------------------

    /// **ADMIN ONLY**: Export contract-level configuration for migration tooling.
    pub fn export_contract_snapshot(env: Env, admin: Address) -> Result<ContractSnapshot, Error> {
        require_admin_auth(&env, &admin)?;

        let token: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token"))
            .ok_or(Error::NotFound)?;
        let min_topup: i128 = admin::get_min_topup(&env)?;
        let next_id: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "next_id"))
            .unwrap_or(0);

        env.events().publish(
            (Symbol::new(&env, "migration_contract_snapshot"),),
            (admin.clone(), env.ledger().timestamp()),
        );

        Ok(ContractSnapshot {
            admin,
            token,
            min_topup,
            next_id,
            storage_version: STORAGE_VERSION,
            timestamp: env.ledger().timestamp(),
        })
    }

    /// Export a single subscription summary for migration tooling. Admin only.
    pub fn export_subscription_summary(
        env: Env,
        admin: Address,
        subscription_id: u32,
    ) -> Result<SubscriptionSummary, Error> {
        require_admin_auth(&env, &admin)?;
        let sub = queries::get_subscription(&env, subscription_id)?;

        env.events().publish(
            (Symbol::new(&env, "migration_export"),),
            MigrationExportEvent {
                admin: admin.clone(),
                start_id: subscription_id,
                limit: 1,
                exported: 1,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(SubscriptionSummary {
            subscription_id,
            subscriber: sub.subscriber,
            merchant: sub.merchant,
            amount: sub.amount,
            interval_seconds: sub.interval_seconds,
            last_payment_timestamp: sub.last_payment_timestamp,
            status: sub.status,
            prepaid_balance: sub.prepaid_balance,
            usage_enabled: sub.usage_enabled,
            lifetime_cap: sub.lifetime_cap,
            lifetime_charged: sub.lifetime_charged,
        })
    }

    /// Export a paginated list of subscription summaries. Admin only.
    pub fn export_subscription_summaries(
        env: Env,
        admin: Address,
        start_id: u32,
        limit: u32,
    ) -> Result<Vec<SubscriptionSummary>, Error> {
        require_admin_auth(&env, &admin)?;
        if limit > MAX_EXPORT_LIMIT {
            return Err(Error::InvalidExportLimit);
        }
        if limit == 0 {
            return Ok(Vec::new(&env));
        }

        let next_id: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "next_id"))
            .unwrap_or(0);
        if start_id >= next_id {
            return Ok(Vec::new(&env));
        }

        let end_id = start_id.saturating_add(limit).min(next_id);
        let mut out = Vec::new(&env);
        let mut exported = 0u32;
        let mut id = start_id;
        while id < end_id {
            if let Some(sub) = env.storage().instance().get::<u32, Subscription>(&id) {
                out.push_back(SubscriptionSummary {
                    subscription_id: id,
                    subscriber: sub.subscriber,
                    merchant: sub.merchant,
                    amount: sub.amount,
                    interval_seconds: sub.interval_seconds,
                    last_payment_timestamp: sub.last_payment_timestamp,
                    status: sub.status,
                    prepaid_balance: sub.prepaid_balance,
                    usage_enabled: sub.usage_enabled,
                    lifetime_cap: sub.lifetime_cap,
                    lifetime_charged: sub.lifetime_charged,
                });
                exported += 1;
            }
            id += 1;
        }

        env.events().publish(
            (Symbol::new(&env, "migration_export"),),
            MigrationExportEvent {
                admin,
                start_id,
                limit,
                exported,
                timestamp: env.ledger().timestamp(),
            },
        );

        Ok(out)
    }

    // -- Subscription Lifecycle -----------------------------------------------

    /// Create a new subscription.
    ///
    /// **Disabled when emergency stop is active.**
    pub fn create_subscription(
        env: Env,
        subscriber: Address,
        merchant: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        lifetime_cap: Option<i128>,
    ) -> Result<u32, Error> {
        require_not_emergency_stop(&env)?;
        subscription::do_create_subscription(
            &env,
            subscriber,
            merchant,
            amount,
            interval_seconds,
            usage_enabled,
            lifetime_cap,
        )
    }

    /// Subscriber deposits USDC into their prepaid vault.
    ///
    /// **Disabled when emergency stop is active.**
    pub fn deposit_funds(
        env: Env,
        subscription_id: u32,
        subscriber: Address,
        amount: i128,
    ) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        subscription::do_deposit_funds(&env, subscription_id, subscriber, amount)
    }

    /// Creates a plan template that can be used to instantiate subscriptions.
    pub fn create_plan_template(
        env: Env,
        merchant: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        lifetime_cap: Option<i128>,
    ) -> Result<u32, Error> {
        subscription::do_create_plan_template(
            &env,
            merchant,
            amount,
            interval_seconds,
            usage_enabled,
            lifetime_cap,
        )
    }

    /// Creates a subscription from a predefined plan template.
    pub fn create_subscription_from_plan(
        env: Env,
        subscriber: Address,
        plan_template_id: u32,
    ) -> Result<u32, Error> {
        subscription::do_create_subscription_from_plan(&env, subscriber, plan_template_id)
    }

    /// Retrieves a plan template by its ID.
    pub fn get_plan_template(env: Env, plan_template_id: u32) -> Result<PlanTemplate, Error> {
        subscription::get_plan_template(&env, plan_template_id)
    }

    /// Cancel the subscription. Allowed from Active, Paused, or InsufficientBalance.
    pub fn cancel_subscription(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
    ) -> Result<(), Error> {
        subscription::do_cancel_subscription(&env, subscription_id, authorizer)
    }

    pub fn pause_subscription(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
    ) -> Result<(), Error> {
        subscription::do_pause_subscription(&env, subscription_id, authorizer)
    }

    pub fn resume_subscription(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
    ) -> Result<(), Error> {
        subscription::do_resume_subscription(&env, subscription_id, authorizer)
    }

    pub fn withdraw_subscriber_funds(
        env: Env,
        subscription_id: u32,
        subscriber: Address,
    ) -> Result<(), Error> {
        subscription::do_withdraw_subscriber_funds(&env, subscription_id, subscriber)
    }

    pub fn set_usage_cap(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
        usage_cap_units: Option<i128>,
    ) -> Result<(), Error> {
        subscription::do_set_usage_cap(&env, subscription_id, authorizer, usage_cap_units)
    }

    pub fn set_usage_rate_limit(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
        max_calls: Option<u32>,
        window_seconds: u64,
    ) -> Result<(), Error> {
        subscription::do_set_usage_rate_limit(
            &env,
            subscription_id,
            authorizer,
            max_calls,
            window_seconds,
        )
    }

    /// Merchant-initiated one-off charge against the subscription's prepaid balance.
    pub fn charge_one_off(
        env: Env,
        subscription_id: u32,
        merchant: Address,
        amount: i128,
    ) -> Result<(), Error> {
        subscription::do_charge_one_off(&env, subscription_id, merchant, amount)
    }

    // -- Charging -------------------------------------------------------------

    /// Charge a subscription for one billing interval.
    ///
    /// **Disabled when emergency stop is active.**
    pub fn charge_subscription(env: Env, subscription_id: u32) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        charge_core::charge_one(&env, subscription_id, env.ledger().timestamp(), None)
    }

    /// Charge a metered usage amount against the subscription's prepaid balance.
    ///
    /// **Disabled when emergency stop is active.**
    pub fn charge_usage(env: Env, subscription_id: u32, usage_amount: i128) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        charge_core::charge_usage_one(&env, subscription_id, usage_amount)
    }

    /// Charge a batch of subscriptions in one transaction. Admin only.
    ///
    /// **Disabled when emergency stop is active.**
    pub fn batch_charge(
        env: Env,
        subscription_ids: Vec<u32>,
    ) -> Result<Vec<BatchChargeResult>, Error> {
        require_not_emergency_stop(&env)?;
        admin::do_batch_charge(&env, &subscription_ids)
    }

    // -- Merchant -------------------------------------------------------------

    pub fn withdraw_merchant_funds(env: Env, merchant: Address, amount: i128) -> Result<(), Error> {
        merchant::withdraw_merchant_funds(&env, merchant, amount)
    }

    pub fn withdraw_treasury_funds(env: Env, admin: Address, amount: i128) -> Result<(), Error> {
        merchant::withdraw_treasury_funds(&env, admin, amount)
    }

    pub fn get_merchant_balance(env: Env, merchant: Address) -> i128 {
        merchant::get_merchant_balance(&env, &merchant)
    }

    pub fn get_treasury_balance(env: Env) -> i128 {
        merchant::get_treasury_balance(&env)
    }

    // -- Queries --------------------------------------------------------------

    pub fn get_subscription(env: Env, subscription_id: u32) -> Result<Subscription, Error> {
        queries::get_subscription(&env, subscription_id)
    }

    pub fn estimate_topup_for_intervals(
        env: Env,
        subscription_id: u32,
        num_intervals: u32,
    ) -> Result<i128, Error> {
        queries::estimate_topup_for_intervals(&env, subscription_id, num_intervals)
    }

    pub fn get_next_charge_info(env: Env, subscription_id: u32) -> Result<NextChargeInfo, Error> {
        let sub = queries::get_subscription(&env, subscription_id)?;
        Ok(queries::compute_next_charge_info(&sub))
    }

    pub fn get_subscriptions_by_merchant(
        env: Env,
        merchant: Address,
        start: u32,
        limit: u32,
    ) -> Vec<Subscription> {
        queries::get_subscriptions_by_merchant(&env, merchant, start, limit)
    }

    /// Return the total number of subscriptions ever created.
    pub fn get_subscription_count(env: Env) -> u32 {
        let key = Symbol::new(&env, "next_id");
        env.storage().instance().get(&key).unwrap_or(0u32)
    }

    /// Return the total number of subscriptions for a merchant.
    pub fn get_merchant_subscription_count(env: Env, merchant: Address) -> u32 {
        queries::get_merchant_subscription_count(&env, merchant)
    }

    /// List all subscription IDs for a given subscriber with pagination.
    pub fn list_subscriptions_by_subscriber(
        env: Env,
        subscriber: Address,
        start_from_id: u32,
        limit: u32,
    ) -> Result<queries::SubscriptionsPage, Error> {
        queries::list_subscriptions_by_subscriber(&env, subscriber, start_from_id, limit)
    }

    pub fn get_billing_period_snapshot(
        env: Env,
        subscription_id: u32,
        period_index: u32,
    ) -> Result<BillingPeriodSnapshot, Error> {
        queries::get_billing_period_snapshot(&env, subscription_id, period_index)
    }

    /// Get lifetime cap information for a subscription.
    pub fn get_cap_info(env: Env, subscription_id: u32) -> Result<CapInfo, Error> {
        queries::get_cap_info(&env, subscription_id)
    }

    // -- Metadata -------------------------------------------------------------

    /// Set or update a metadata key-value pair on a subscription.
    ///
    /// Authorization: subscriber or merchant.
    /// Does not affect financial state (balances, status, charges).
    pub fn set_metadata(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
        key: String,
        value: String,
    ) -> Result<(), Error> {
        metadata::do_set_metadata(&env, subscription_id, &authorizer, key, value)
    }

    /// Delete a metadata key from a subscription.
    ///
    /// Authorization: subscriber or merchant.
    pub fn delete_metadata(
        env: Env,
        subscription_id: u32,
        authorizer: Address,
        key: String,
    ) -> Result<(), Error> {
        metadata::do_delete_metadata(&env, subscription_id, &authorizer, key)
    }

    /// Get a metadata value by key.
    pub fn get_metadata(env: Env, subscription_id: u32, key: String) -> Result<String, Error> {
        metadata::do_get_metadata(&env, subscription_id, key)
    }

    /// List all metadata keys for a subscription.
    pub fn list_metadata_keys(env: Env, subscription_id: u32) -> Result<Vec<String>, Error> {
        metadata::do_list_metadata_keys(&env, subscription_id)
    }

    // -- Blocklist ------------------------------------------------------------

    pub fn add_to_blocklist(
        env: Env,
        authorizer: Address,
        subscriber: Address,
        reason: Option<soroban_sdk::String>,
    ) -> Result<(), Error> {
        blocklist::do_add_to_blocklist(&env, authorizer, subscriber, reason)
    }

    pub fn remove_from_blocklist(
        env: Env,
        admin: Address,
        subscriber: Address,
    ) -> Result<(), Error> {
        blocklist::do_remove_from_blocklist(&env, admin, subscriber)
    }

    pub fn is_blocklisted(env: Env, subscriber: Address) -> bool {
        blocklist::is_blocklisted(&env, &subscriber)
    }

    pub fn get_blocklist_entry(env: Env, subscriber: Address) -> Result<BlocklistEntry, Error> {
        blocklist::get_blocklist_entry(&env, subscriber)
    }
}

#[cfg(test)]
mod test;
