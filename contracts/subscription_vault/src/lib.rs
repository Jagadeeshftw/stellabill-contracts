#![no_std]

mod admin;
mod charge_core;
mod merchant;
mod queries;
mod reentrancy;
mod safe_math;
mod state_machine;
mod subscription;
mod types;

use soroban_sdk::{contract, contractimpl, Address, Env, Vec};

pub use state_machine::{can_transition, get_allowed_transitions, validate_status_transition};
pub use types::*;

pub const MAX_SUBSCRIPTION_ID: u32 = u32::MAX;

fn require_not_emergency_stop(env: &Env) -> Result<(), Error> {
    let stopped = env
        .storage()
        .instance()
        .get(&soroban_sdk::Symbol::new(env, "emergency_stop"))
        .unwrap_or(false);
    if stopped {
        return Err(Error::EmergencyStopActive);
    }
    Ok(())
}

#[contract]
pub struct SubscriptionVault;

#[contractimpl]
impl SubscriptionVault {
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

    pub fn create_subscription(
        env: Env,
        subscriber: Address,
        merchant: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
        expiration: Option<u64>,
    ) -> Result<u32, Error> {
        require_not_emergency_stop(&env)?;
        if let Some(exp) = expiration {
            if exp <= env.ledger().timestamp() {
                return Err(Error::InvalidInput);
            }
        }
        let id = subscription::do_create_subscription(
            &env,
            subscriber,
            merchant,
            amount,
            interval_seconds,
            usage_enabled,
            expiration,
        )?;
        if id == MAX_SUBSCRIPTION_ID {
            return Err(Error::SubscriptionLimitReached);
        }
        Ok(id)
    }

    pub fn create_plan_template(
        env: Env,
        merchant: Address,
        amount: i128,
        interval_seconds: u64,
        usage_enabled: bool,
    ) -> Result<u32, Error> {
        subscription::do_create_plan_template(&env, merchant, amount, interval_seconds, usage_enabled)
    }

    pub fn create_subscription_from_plan(
        env: Env,
        subscriber: Address,
        plan_template_id: u32,
    ) -> Result<u32, Error> {
        subscription::do_create_subscription_from_plan(&env, subscriber, plan_template_id)
    }

    pub fn get_plan_template(env: Env, plan_template_id: u32) -> Result<PlanTemplate, Error> {
        subscription::get_plan_template(&env, plan_template_id)
    }

    pub fn deposit_funds(
        env: Env,
        subscription_id: u32,
        subscriber: Address,
        amount: i128,
    ) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        subscription::do_deposit_funds(&env, subscription_id, subscriber, amount)
    }

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

    pub fn charge_subscription(env: Env, subscription_id: u32) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        charge_core::charge_one(&env, subscription_id, env.ledger().timestamp(), None)
    }

    pub fn charge_usage(env: Env, subscription_id: u32, usage_amount: i128) -> Result<(), Error> {
        require_not_emergency_stop(&env)?;
        charge_core::charge_usage_one(&env, subscription_id, usage_amount)
    }

    pub fn batch_charge(
        env: Env,
        subscription_ids: Vec<u32>,
    ) -> Result<Vec<BatchChargeResult>, Error> {
        require_not_emergency_stop(&env)?;
        admin::do_batch_charge(&env, &subscription_ids)
    }

    pub fn charge_one_off(
        env: Env,
        subscription_id: u32,
        merchant: Address,
        amount: i128,
    ) -> Result<(), Error> {
        subscription::do_charge_one_off(&env, subscription_id, merchant, amount)
    }

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

    pub fn withdraw_subscriber_funds(
        env: Env,
        subscription_id: u32,
        subscriber: Address,
    ) -> Result<(), Error> {
        subscription::do_withdraw_subscriber_funds(&env, subscription_id, subscriber)
    }

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

    pub fn get_merchant_subscription_count(env: Env, merchant: Address) -> u32 {
        queries::get_merchant_subscription_count(&env, merchant)
    }

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
}

#[cfg(test)]
mod test;
